#![no_std]
#![no_main]

extern crate alloc;
use coap::{coap_task, CoapStateManager, InfoResource};
use core::net::Ipv4Addr;
use core::ptr::addr_of_mut;
use core::task::Context;
use defmt::*;
use defmt_rtt as _; // global logger
use embassy_executor::Spawner;
use embassy_net::driver::{
    Capabilities, Driver as NetDriver, HardwareAddress, LinkState, RxToken, TxToken,
};
use embassy_net::{EthernetAddress, Ipv4Cidr, StackResources};
use embassy_net_driver_channel::State as NetState;
use embassy_stm32::crc::{self, Crc};
use embassy_stm32::gpio::{self, Flex, Pin};
use embassy_stm32::time::Hertz;
use embassy_stm32::timer::input_capture::{CapturePin, Ch1};
use embassy_stm32::timer::low_level::{InputCaptureMode, Timer, TriggerSource};
use embassy_stm32::timer::Channel;
use embassy_stm32::usart::Config as UartConfig;
use embassy_stm32::usart::Uart;
use embassy_stm32::usb::Driver;
use embassy_stm32::{
    bind_interrupts,
    gpio::{AfType, AnyPin, Input},
    peripherals,
    peripherals::PD12,
    rng, usart, usb, Config,
};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_time::Timer as HighLevelTimer;
use embassy_usb::class::cdc_ncm::embassy_net::Device;
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State};
use embassy_usb::{Builder, Config as UsbConfig};
use ibus_redirect::{ibus_half_duplex_task, uart_rx_task, uart_tx_task};
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{EthernetFrame, Ipv4Packet, UdpPacket};
use smoltcp::wire::{EthernetProtocol, Ipv4Repr};
use usb_setup::{usb_reception_task_rx, usb_sending_task_tx, usb_task, StmUsbDriver};

use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr};

use panic_probe as _;
use rand_core::RngCore;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    OTG_FS => usb::InterruptHandler<peripherals::USB_OTG_FS>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
    USART2 => usart::InterruptHandler<peripherals::USART2>;
});

use alloc::vec;
use embassy_sync::channel::{self};

const MAX_MESSAGES_CHANNEL: usize = 15;

use alloc::vec::Vec;
pub type MessageChannel = channel::Channel<NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;
pub type ChannelSender = channel::Sender<'static, NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;
pub type ChannelReceiver = channel::Receiver<'static, NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;

use embedded_alloc::LlffHeap as Heap;
mod coap;
mod ibus_redirect;
mod ipv4_packet_reconstructor;
mod usb_setup;

#[global_allocator]
static HEAP: Heap = Heap::empty();

pub const MTU: usize = 1514;

use core::mem::MaybeUninit;
const HEAP_SIZE: usize = 100_000;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

pub struct CarrierSenseTimer {
    timer: Timer<'static, peripherals::TIM4>,
    pin: CapturePin<'static, peripherals::TIM4, Ch1>,
}

impl CarrierSenseTimer {
    const UART_WORDS_PER_TICK: u16 = 2;
    const UART_NUMBER_OF_BITS_TO_WAIT: u16 = 4; // wait for 4 possible messages
    const WAIT_MICROSECONDS: u16 = (Self::UART_WORDS_PER_TICK * Self::UART_NUMBER_OF_BITS_TO_WAIT);
    pub fn new(timer4: peripherals::TIM4, pin: PD12) -> Self {
        let capture_pin = CapturePin::new_ch1(pin, gpio::Pull::None);

        let mut timer4 = Timer::new(timer4);
        timer4.set_input_capture_mode(Channel::Ch1, InputCaptureMode::Rising);
        timer4.enable_channel(Channel::Ch1, true);
        timer4.set_trigger_source(TriggerSource::TI1FP1);
        timer4.set_slave_mode(embassy_stm32::timer::low_level::SlaveMode::COMBINED_RESET_TRIGGER);
        timer4.set_tick_freq(Hertz::mhz(1));
        timer4
            .regs_basic()
            .arr()
            .write(|arr| arr.set_arr(Self::WAIT_MICROSECONDS));
        timer4.regs_basic().cr1().modify(|r| r.set_opm(true));
        timer4.set_autoreload_preload(false);
        Self {
            timer: timer4,
            pin: capture_pin,
        }
    }
    pub fn line_idle(&self) -> bool {
        let count = self.timer.regs_core().cnt().read().cnt();
        if count != 0 {
            info!("count: {:?}", count);
        }
        let is_count_ready = count == 0;
        if !is_count_ready {
            info!("someone is sending on the line, warning!");
        }
        return is_count_ready;
    }
}

#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, WrapperDriver<Device<'static, MTU>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn ibus_ipv4_to_ethernet_task(
    receive_packets_from_ibus_ipv4: ChannelReceiver,
    send_packets_from_ibus_to_ethernet_task: ChannelSender,
    destination_mac_address: EthernetAddress,
    source_simulated_mac_address: EthernetAddress,
) {
    loop {
        let receive_ipv4_packet = receive_packets_from_ibus_ipv4.receive().await;

        log_packet(&receive_ipv4_packet, "POV of task sending to ethernet");

        let eth_frame_len = EthernetFrame::<&[u8]>::buffer_len(receive_ipv4_packet.len());
        let mut eth_buf_vec = vec![0; eth_frame_len];
        let mut eth_frame = EthernetFrame::new_unchecked(&mut eth_buf_vec);
        eth_frame.set_dst_addr(destination_mac_address);
        eth_frame.set_src_addr(source_simulated_mac_address);
        eth_frame.set_ethertype(EthernetProtocol::Ipv4);
        eth_frame
            .payload_mut()
            .copy_from_slice(&receive_ipv4_packet);
        send_packets_from_ibus_to_ethernet_task
            .send(eth_buf_vec)
            .await;
    }
}

macro_rules! create_static_channel {
    () => {{
        static IBUS_CHANNEL: StaticCell<MessageChannel> = StaticCell::new();
        let ibus_channel = IBUS_CHANNEL.init_with(MessageChannel::new);
        let send_to_ibus_channel = ibus_channel.sender();
        let receive_from_ibus_channel = ibus_channel.receiver();
        (send_to_ibus_channel, receive_from_ibus_channel)
    }};
}

struct WrapperDriver<D: NetDriver> {
    inner: D,
    router: WrapperIbusRouter, //    expected_mac: EthernetAddress,
                               //    magic_mac: EthernetAddress,
                               //    subnet: Ipv4Cidr,
                               //    ip_address: Ipv4Addr,
                               //    sender_to_redirect_task: ChannelSender,
}

#[derive(Clone, Copy, Format)]
pub struct Ipv4Router {
    pub gateway_address: Ipv4Addr,
    pub subnet: Ipv4Cidr,
    pub device_address: Ipv4Addr, // Ipv4Addr::new(10, 0, 0, 1)
}

impl Ipv4Router {
    fn is_ipv4_packet_valid_for_routing(&self, packet: Ipv4Packet<&[u8]>) -> bool {
        let dest_addr = packet.dst_addr();
        return self.subnet.contains_addr(&dest_addr)
            && dest_addr != self.device_address
            && dest_addr != self.gateway_address;
    }
}

struct WrapperIbusRouter {
    pub ipv4_router: Ipv4Router,
    pub device_mac_address: EthernetAddress,
    pub simulated_mac_address: EthernetAddress,
    pub pc_address: Ipv4Addr,
    pub sender_to_redirect_task: ChannelSender,
    pub crc: Crc<'static>,
}

impl WrapperIbusRouter {
    // returns the packet, and the intended receiver of the new packet
    pub fn intercept_arp_repr(&self, repr: &ArpRepr) -> Option<(ArpRepr, EthernetAddress)> {
        if let ArpRepr::EthernetIpv4 {
            operation: _,
            source_hardware_addr,
            source_protocol_addr,
            target_hardware_addr,
            target_protocol_addr,
        } = repr
        {
            if *source_protocol_addr == Ipv4Addr::new(0, 0, 0, 0)
                || source_protocol_addr == target_protocol_addr
                || *target_protocol_addr == self.ipv4_router.gateway_address
                || !self.ipv4_router.subnet.contains_addr(&target_protocol_addr)
                || self.ipv4_router.device_address == *target_protocol_addr
                || self.pc_address == *target_protocol_addr
            {
                // do not mess with address discovery
                return None;
            }

            let response = ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr: self.simulated_mac_address, // Use the "magic" MAC address
                source_protocol_addr: *target_protocol_addr,      // Respond with the requested IP
                target_hardware_addr: *source_hardware_addr,      // Send to the requester's MAC
                target_protocol_addr: *source_protocol_addr,      // Send to the requester's IP
            };
            debug!("creating response for: {:?}", repr);
            return Some((response, *source_hardware_addr));
        } else {
            return None;
        }
    }

    pub fn intercept_ipv4_packet(&mut self, packet: EthernetFrame<&[u8]>) {
        match Ipv4Packet::new_checked(packet.payload()) {
            Ok(valid_packet) => {
                if self
                    .ipv4_router
                    .is_ipv4_packet_valid_for_routing(valid_packet.clone())
                {
                    let mut vec_packet = valid_packet.into_inner().to_vec();
                    let crc_val = self.crc.feed_bytes(&vec_packet) as u16;
                    self.crc.reset();
                    vec_packet.extend_from_slice(crc_val.to_le_bytes().as_ref());

                    debug!("routing packet to send to redirect task from ethernet ipv4");
                    match self.sender_to_redirect_task.try_send(vec_packet) {
                        Err(_) => {
                            warn!("failed to send packet to redirect task because it is full")
                        }
                        Ok(()) => debug!("succesfully routed packet"),
                    };
                }
            }
            Err(e) => {
                info!("ipv4 packet parse failed: {:?}", e);
            }
        };
    }
}

impl<D: NetDriver> WrapperDriver<D> {
    pub fn new(
        inner: D,
        expected_mac: EthernetAddress,
        magic_mac: EthernetAddress,
        sender_to_redirect_task: ChannelSender,
        test_pc_address: Ipv4Addr,
        router: &Ipv4Router,
        crc: Crc<'static>,
    ) -> Self {
        Self {
            inner,

            router: WrapperIbusRouter {
                device_mac_address: expected_mac,
                simulated_mac_address: magic_mac,
                ipv4_router: router.clone(),
                sender_to_redirect_task,
                pc_address: test_pc_address,
                crc,
            },
        }
    }

    pub fn do_read_poll<'a>(&'a mut self, cx: &mut Context) -> DriverResult<'a, D> {
        let packet = self.inner.receive(cx);
        match packet {
            Some(packet) => {
                let vec_token = packet.0.consume(|bytes| VecToken {
                    data: bytes.to_vec(),
                });
                // Parse the packet as an Ethernet frame
                if let Ok(frame) = EthernetFrame::new_checked(vec_token.data.as_slice()) {
                    match frame.ethertype() {
                        EthernetProtocol::Arp => {
                            let arp_packet = ArpPacket::new_checked(frame.payload())
                                .inspect_err(|e| warn!("could not create arp packet: {:?}", e))
                                .expect("parsing failed");
                            let arp_repr = ArpRepr::parse(&arp_packet).expect("parsing failed");

                            // Check if it's an ARP request
                            if arp_packet.operation() != ArpOperation::Request {
                                return DriverResult::Token(Some((vec_token, packet.1)));
                            }
                            match self.router.intercept_arp_repr(&arp_repr) {
                                Some((response, to)) => {
                                    debug!("arp packet reply: {:?} to {:?}", &response, &to);

                                    let response_len = response.buffer_len();
                                    let mut response_buffer_1 = vec![0; response_len];

                                    let mut p =
                                        ArpPacket::new_unchecked(response_buffer_1.as_mut_slice());
                                    response.emit(&mut p);

                                    packet.1.consume(
                                        EthernetFrame::<&[u8]>::buffer_len(response_buffer_1.len()),
                                        |response_bytes| {
                                            let mut reply_frame =
                                                EthernetFrame::new_unchecked(response_bytes);
                                            reply_frame.set_dst_addr(to);
                                            reply_frame
                                                .set_src_addr(self.router.simulated_mac_address);
                                            reply_frame.set_ethertype(EthernetProtocol::Arp);

                                            reply_frame
                                                .payload_mut()
                                                .copy_from_slice(&response_buffer_1);
                                        },
                                    );
                                    info!(
                                        "created ARP response for {:?}",
                                        arp_packet.source_protocol_addr()
                                    );

                                    // we successfully intercepted the packet, and returned nothing
                                    return DriverResult::Retry;
                                }
                                None => {}
                            };
                        }
                        EthernetProtocol::Ipv4 => {
                            self.router.intercept_ipv4_packet(frame);
                        }
                        _ => {}
                    }
                }

                // Return the packet for further processing
                return DriverResult::Token(Some((vec_token, packet.1)));
            }
            None => return DriverResult::Token(None),
        }
    }
}

struct VecToken {
    data: alloc::vec::Vec<u8>,
}

impl RxToken for VecToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.data)
    }
}

enum DriverResult<'a, D: NetDriver + 'a> {
    Retry,
    Token(
        Option<(
            <WrapperDriver<D> as NetDriver>::RxToken<'a>,
            <WrapperDriver<D> as NetDriver>::TxToken<'a>,
        )>,
    ),
}

impl<D: NetDriver> NetDriver for WrapperDriver<D> {
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;
    type RxToken<'a>
        = VecToken
    where
        Self: 'a;

    fn receive(&mut self, cx: &mut Context) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let y = loop {
            let x = unsafe {
                // Get a raw mutable pointer to `self`
                let self_ptr: *mut Self = self;

                // Dereference and call `do_read_poll`, bypassing Rust borrow checker
                (*self_ptr).do_read_poll(cx)
            };
            match x {
                DriverResult::Retry => {
                    drop(x); // reference to x is dropped here
                    continue;
                }
                DriverResult::Token(tok) => break tok,
            }
        };
        return y;
    }

    fn transmit(&mut self, cx: &mut Context) -> Option<Self::TxToken<'_>> {
        self.inner.transmit(cx)
    }

    /// Get the link state.
    ///
    /// This function must return the current link state of  wake `cx.waker()` when
    /// the link state changes.
    fn link_state(&mut self, cx: &mut Context) -> LinkState {
        self.inner.link_state(cx)
    }

    /// Get a description of device capabilities.
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    /// Get the device's hardware address.
    ///
    /// The returned hardware address also determines the "medium" of this driver. This indicates
    /// what kind of packet the sent/received bytes are, and determines some behaviors of
    /// the interface. For example, ARP/NDISC address resolution is only done for Ethernet mediums.
    fn hardware_address(&self) -> HardwareAddress {
        self.inner.hardware_address()
    }
    // Implement other required methods from the Driver trait
    // ...
}

pub fn log_packet(receive_ipv4_packet: &[u8], msg: &str) {
    match Ipv4Packet::new_checked(&receive_ipv4_packet) {
        Ok(packet) => {
            let Ok(repr) = Ipv4Repr::parse(&packet, &ChecksumCapabilities::default()) else {
                debug!("packet log: not a packet");
                return;
            };

            debug!("{}", msg);
            debug!("packet repr: {:?}", repr);
            if let Ok(udp) = UdpPacket::new_checked(packet.payload()) {
                udp.src_port();
                udp.dst_port();
                info!(
                    "udp packet: src: {:?}, dst: {:?}",
                    udp.src_port(),
                    udp.dst_port()
                )
            } else {
                warn!("packet is not udp packet");
            }
        }

        Err(e) => warn!("packet parse failed: {:?}", e),
    }
}

#[embassy_executor::task]
async fn timer_idle_task(timer: CarrierSenseTimer) {
    loop {
        HighLevelTimer::after_micros(100).await;
        let _ = timer.line_idle();
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World!");
    info!("initializing heap");
    unsafe { HEAP.init(addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) };
    info!("heap initialized");

    let mut config = Config::default();
    {
        //        use embassy_stm32::rcc::*;
        //        config.rcc.hsi = true;
        //        config.rcc.msis = Some(MSIRange::RANGE_48MHZ);
        //        config.rcc.sys = Sysclk::MSIS;
        //        config.rcc.voltage_range = VoltageScale::RANGE2;
        //        config.rcc.hsi48 = Some(Hsi48Config {
        //            sync_from_usb: true,
        //        }); // needed for USB
        //        config.rcc.mux.iclksel = mux::Iclksel::HSI48; // USB uses ICLK
        use embassy_stm32::rcc::*;
        config.rcc.hsi = true;
        config.rcc.pll1 = Some(Pll {
            source: PllSource::HSI, // 16 MHz
            prediv: PllPreDiv::DIV1,
            mul: PllMul::MUL10,
            divp: None,
            divq: None,
            divr: Some(PllDiv::DIV1), // 160 MHz
        });
        config.rcc.sys = Sysclk::PLL1_R;
        config.rcc.voltage_range = VoltageScale::RANGE1;
        config.rcc.hsi48 = Some(Hsi48Config {
            sync_from_usb: true,
        }); // needed for USB
        config.rcc.mux.iclksel = mux::Iclksel::HSI48; // USB uses ICLK
    }

    let peripherals_instance = embassy_stm32::init(config);
    let mut config = UartConfig::default();
    config.invert_tx = true;
    config.invert_rx = true;
    config.baudrate = 500_000;

    let timer4 = peripherals_instance.TIM4;
    let timer = CarrierSenseTimer::new(timer4, peripherals_instance.PD12);

    let uart = Uart::new(
        peripherals_instance.USART2,
        peripherals_instance.PD6,
        peripherals_instance.PD5,
        Irqs,
        peripherals_instance.GPDMA1_CH4,
        peripherals_instance.GPDMA1_CH5,
        config,
    )
    .expect("could not initialize UART");

    let (uart_sender_component, uart_receiver_component) = uart.split();

    // Create the driver, from the HAL.

    static EP_OUT_BUFFER: StaticCell<[u8; 256]> = StaticCell::new();
    let ep_out_buffer = EP_OUT_BUFFER.init_with(|| [0u8; 256]);

    let mut config = embassy_stm32::usb::Config::default();
    // Do not enable vbus_detection. This is a safe default that works in all boards.
    // However, if your USB device is self-powered (can stay powered on if USB is unplugged), you need
    // to enable vbus_detection to comply with the USB spec. If you enable it, the board
    // has to support it or USB won't work at all. See docs on `vbus_detection` for details.
    config.vbus_detection = false;
    let driver: StmUsbDriver = Driver::new_fs(
        peripherals_instance.USB_OTG_FS,
        Irqs,
        peripherals_instance.PA12,
        peripherals_instance.PA11,
        ep_out_buffer,
        config,
    );

    let mut config = UsbConfig::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Mad Joe");
    config.product = Some("IBUS adapter V2");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Create embassy-usb DeviceBuilder using the driver and config.
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut CONFIG_DESC.init([0; 256])[..],
        &mut BOS_DESC.init([0; 256])[..],
        &mut [], // no msos descriptors
        &mut CONTROL_BUF.init([0; 128])[..],
    );

    // Our MAC addr.
    let our_mac_addr = [0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC];
    // magic mac address
    let magic_mac_addr = [0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCA];
    // Host's MAC addr. This is the MAC the host "thinks" its USB-to-ethernet adapter has.
    let host_mac_addr = [0x88, 0x88, 0x88, 0x88, 0x88, 0x88];

    let (send_to_uart_task, receive_from_ibus_channel) = create_static_channel!();
    let (send_to_raw_socket, receive_from_uart_task_channel) = create_static_channel!();
    let (send_to_usb_task, receive_from_raw_socket_channel) = create_static_channel!();

    // Create classes on the builder.
    static STATE: StaticCell<State> = StaticCell::new();
    let class = CdcNcmClass::new(&mut builder, STATE.init(State::new()), host_mac_addr, 64);

    // Build the builder.
    let usb = builder.build();

    unwrap!(spawner.spawn(usb_task(usb)));
    // Run the USB device.

    static NET_STATE: StaticCell<NetState<MTU, 4, 4>> = StaticCell::new();
    let net_state = NET_STATE.init(NetState::new());

    let (tx_usb, rx_usb) = class.split();

    let (runner, device) = embassy_net_driver_channel::new(
        net_state,
        embassy_net_driver_channel::driver::HardwareAddress::Ethernet(our_mac_addr),
    );
    let (state_runner, rx_runner, tx_runner) = runner.split();

    //let runner = Runner {tx_usb, rx_usb, ch: runner};
    // ALL THAT IS MISSING IS JUST TO USE THE LOGIC IN RUNNER AND PASS IT INTO 2 SEPARATE
    // TASKS!
    unwrap!(spawner.spawn(usb_reception_task_rx(rx_usb, rx_runner, state_runner)));
    unwrap!(spawner.spawn(usb_sending_task_tx(
        tx_usb,
        tx_runner,
        receive_from_raw_socket_channel
    )));

    let ipv4_route_rules = Ipv4Router {
        gateway_address: Ipv4Addr::new(10, 0, 0, 1),
        subnet: Ipv4Cidr::new(Ipv4Addr::new(10, 0, 0, 0), 24),
        device_address: Ipv4Addr::new(10, 0, 0, 169),
    };

    const TEST_PC_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 1, 0, 100);

    let crc = Crc::new(
        peripherals_instance.CRC,
        unwrap!(crc::Config::new(
            crc::InputReverseConfig::Byte,
            true,
            crc::PolySize::Width16,
            0x0000_0000,
            0x1021,
        )),
    );
    let device_wrapper = WrapperDriver::new(
        device,
        EthernetAddress::from_bytes(&our_mac_addr),
        EthernetAddress::from_bytes(&magic_mac_addr),
        send_to_uart_task,
        TEST_PC_ADDRESS,
        &ipv4_route_rules,
        crc,
    );

    // let config = embassy_net::Config::dhcpv4(Default::default());
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::new(10, 0, 0, 169), 24),
        dns_servers: heapless::Vec::new(),
        gateway: Some(Ipv4Addr::new(10, 0, 0, 1)),
    });

    let mut rng = embassy_stm32::rng::Rng::new(peripherals_instance.RNG, Irqs);

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        device_wrapper,
        config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    unwrap!(spawner.spawn(net_task(runner)));

    let resource = InfoResource {};
    let mut manager = CoapStateManager::new();
    manager.add_resource(resource);
    unwrap!(spawner.spawn(coap_task(stack, manager)));

    unwrap!(spawner.spawn(ibus_half_duplex_task(
        uart_sender_component,
        receive_from_ibus_channel,
        uart_receiver_component,
        ipv4_packet_reconstructor::Ipv4PacketParser::new(send_to_raw_socket),
        timer
    )));

    //    unwrap!(spawner.spawn(uart_tx_task(
    //        uart_sender_component,
    //        receive_from_ibus_channel
    //    )));
    //
    //    unwrap!(spawner.spawn(timer_idle_task(timer)));
    // unwrap!(spawner.spawn(uart_rx_task(uart_receiver_component, send_to_raw_socket)));

    unwrap!(spawner.spawn(ibus_ipv4_to_ethernet_task(
        receive_from_uart_task_channel,
        send_to_usb_task,
        EthernetAddress::from_bytes(&host_mac_addr),
        EthernetAddress::from_bytes(&magic_mac_addr),
    )));
}
