#![no_std]
#![no_main]

extern crate alloc;
use coap::{coap_task, CoapStateManager, InfoResource};
use core::cmp::Ordering;
use core::future::Future;
use core::marker::PhantomData;
use core::net::Ipv4Addr;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::addr_of_mut;
use core::task::Context;
use defmt::{panic, *};
use defmt_rtt as _; // global logger
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{self, select, Either};
use embassy_net::driver::{
    Capabilities, Driver as NetDriver, HardwareAddress, LinkState, RxToken, TxToken,
};
use embassy_net::raw::RawSocket;
use embassy_net::tcp::TcpSocket;
use embassy_net::{EthernetAddress, IpEndpoint, IpListenEndpoint, Ipv4Cidr, StackResources};
use embassy_stm32::mode::{Async, Mode};
use embassy_stm32::usart::Config as UartConfig;
use embassy_stm32::usart::{Uart, UartRx, UartTx};
use embassy_stm32::usb::{Driver, Instance};
use embassy_stm32::{bind_interrupts, peripherals, rng, usart, usb, Config};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_usb::class::cdc_ncm::embassy_net::{Device, Runner, State as NetState};
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State};
use embassy_usb::{Builder, Config as UsbConfig, UsbDevice};
use serial_line_ip::{Decoder, Encoder};
use smoltcp::socket::raw::PacketMetadata as RawPacketMetadata;
use smoltcp::wire::EthernetProtocol;
use smoltcp::wire::{EthernetFrame, Ipv4Packet};

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
use embassy_sync::channel::{self, Receiver, Sender};

const MAX_MESSAGES_CHANNEL: usize = 15;

use alloc::vec::Vec;
type MessageChannel = channel::Channel<NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;
type ChannelSender = channel::Sender<'static, NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;
type ChannelReceiver = channel::Receiver<'static, NoopRawMutex, Vec<u8>, MAX_MESSAGES_CHANNEL>;

use embedded_alloc::LlffHeap as Heap;
mod coap;

#[global_allocator]
static HEAP: Heap = Heap::empty();

const MTU: usize = 1514;

use core::mem::MaybeUninit;
const HEAP_SIZE: usize = 100_000;
static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];

type MyDriver = Driver<'static, peripherals::USB_OTG_FS>;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, MyDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn usb_ncm_task(class: Runner<'static, MyDriver, MTU>) -> ! {
    class.run().await
}

#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, WrapperDriver<Device<'static, MTU>>>,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn ibus_traffic_to_ethernet_task(
    stack: embassy_net::Stack<'static>,
    receive_from_uart_to_send_via_ethernet: ChannelReceiver,
    send_packets_from_raw_socket_to_uart: ChannelSender,
) -> ! {
    let mut rx_buffer = [0; 4096];
    let mut rx_metadata_buffer = [RawPacketMetadata::EMPTY; 10];
    let mut tx_buffer = [0; 4096];
    let mut tx_metadata_buffer = [RawPacketMetadata::EMPTY; 10];

    let raw_socket = RawSocket::new::<WrapperDriver<Device<'static, MTU>>>(
        stack,
        smoltcp::wire::IpVersion::Ipv4,
        smoltcp::wire::IpProtocol::Udp,
        &mut rx_metadata_buffer,
        &mut rx_buffer,
        &mut tx_metadata_buffer,
        &mut tx_buffer,
    );
    let mut reception_buffer = [0; 2000];

    loop {
        let receive_to_send_to_ibus = receive_from_uart_to_send_via_ethernet.receive();
        let receive_from_waw_socket = raw_socket.recv(&mut reception_buffer);
        let future_selection = select(receive_to_send_to_ibus, receive_from_waw_socket).await;
        match future_selection {
            Either::First(packet_to_send_to_eth) => {
                raw_socket.send(&packet_to_send_to_eth).await;
            }
            Either::Second(raw_packet) => {
                let rx_packet = raw_packet.unwrap();
                send_packets_from_raw_socket_to_uart
                    .send(reception_buffer[..rx_packet].to_vec())
                    .await;
            }
        }
    }
}

#[embassy_executor::task]
async fn uart_rx_task(mut rx_uart: UartRx<'static, Async>, send_to_raw_socket: ChannelSender) -> ! {
    let mut rx_buffer = [0; 2000];
    let mut slip_decode_buffer = [0; 4000];

    let slip_decode_buffer_slice = &mut slip_decode_buffer;

    let mut slip_decoder = serial_line_ip::Decoder::new();
    let mut next_packet = vec![];
    loop {
        let uart_read_bytes = match rx_uart.read_until_idle(&mut rx_buffer).await {
            Ok(b) => b,
            Err(e) => {
                warn!("uart reception failed: {:?}", e);
                continue;
            }
        };
        let mut read_slice = &rx_buffer[..uart_read_bytes];
        while read_slice.len() != 0 {
            match slip_decoder.decode(read_slice, slip_decode_buffer_slice) {
                Ok((bytes_processed, output_slice, is_end_of_packet)) => {
                    next_packet.extend_from_slice(output_slice);
                    if is_end_of_packet {
                        // mem::take clears the vector and leaves it empty
                        send_to_raw_socket
                            .send(core::mem::take(&mut next_packet))
                            .await;
                        read_slice = &read_slice[bytes_processed..];
                        slip_decoder = Decoder::new();
                    } else {
                        warn!("split packet received: not very well tested");
                    }
                }
                Err(e) => {
                    // reset everything
                    slip_decoder = Decoder::new();
                    next_packet.clear();
                    warn!("decoding failed: {:?}", Debug2Format(&e))
                }
            }
        }
    }
}

#[embassy_executor::task]
async fn uart_tx_task(
    mut tx_uart: UartTx<'static, Async>,
    receive_from_raw_socket_to_uart: ChannelReceiver,
) -> ! {
    let mut encode_buffer = [0u8; 2000];
    loop {
        let mut encoder = Encoder::new();
        let rx = receive_from_raw_socket_to_uart.receive().await;
        let bytes_writen = match encoder.encode(&rx, &mut encode_buffer) {
            Ok(t) => t.written,
            Err(e) => {
                warn!("slip decoding failed: {:?}", Debug2Format(&e));
                continue;
            }
        };
        match tx_uart.write(&encode_buffer[..bytes_writen]).await {
            Ok(b) => info!("wrote {:?} bytes", b),
            Err(e) => warn!("uart transmission failed: {:?}", e),
        }
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
    expected_mac: EthernetAddress,
    magic_mac: EthernetAddress,
    subnet: Ipv4Cidr,
    ip_address: Ipv4Addr,
    sender_to_redirect_task: ChannelSender,
}

impl<D: NetDriver> WrapperDriver<D> {
    pub fn new(
        inner: D,
        expected_mac: EthernetAddress,
        magic_mac: EthernetAddress,
        subnet: Ipv4Cidr,
        ip_address: Ipv4Addr,
        sender_to_redirect_task: ChannelSender,
    ) -> Self {
        Self {
            inner,
            expected_mac,
            magic_mac,
            subnet,
            ip_address,
            sender_to_redirect_task,
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
                    let dest_mac = frame.dst_addr();
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

                            if let ArpRepr::EthernetIpv4 {
                                operation: _,
                                source_hardware_addr,
                                source_protocol_addr,
                                target_hardware_addr,
                                target_protocol_addr,
                            } = arp_repr
                            {
                                if source_protocol_addr == Ipv4Addr::new(0, 0, 0, 0)
                                    || source_protocol_addr == target_protocol_addr
                                    || target_protocol_addr == Ipv4Addr::new(10, 0, 0, 1)
                                    || !self.subnet.contains_addr(&target_protocol_addr)
                                    || self.ip_address == target_protocol_addr
                                {
                                    // do not mess with address discovery
                                    return DriverResult::Token(Some((vec_token, packet.1)));
                                }
                                // Create an ARP response
                                let response = ArpRepr::EthernetIpv4 {
                                    operation: ArpOperation::Reply,
                                    source_hardware_addr: self.magic_mac, // Use the "magic" MAC address
                                    source_protocol_addr: target_protocol_addr, // Respond with the requested IP
                                    target_hardware_addr: source_hardware_addr, // Send to the requester's MAC
                                    target_protocol_addr: source_protocol_addr, // Send to the requester's IP
                                };
                                info!("creating response: {:?}", &response);
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
                                        reply_frame.set_dst_addr(source_hardware_addr);
                                        reply_frame.set_src_addr(self.magic_mac);
                                        reply_frame.set_ethertype(EthernetProtocol::Arp);

                                        reply_frame
                                            .payload_mut()
                                            .copy_from_slice(&response_buffer_1);
                                    },
                                )
                            }

                            info!("made a funny");
                            // we successfully intercepted the packet, and returned nothing
                            return DriverResult::Retry;
                        }
                        _ => {}
                    }
                    if frame.ethertype() == EthernetProtocol::Ipv4 {
                        let ipv4_packet = match Ipv4Packet::new_checked(frame.payload()) {
                            Ok(p) => info!("ipv4 packet: {:?}", p),
                            Err(e) => {
                                info!("ipv4 packet parse failed: {:?}", e);
                            }
                        };
                    }
                    // Check if the destination MAC matches the expected one
                    if dest_mac != self.expected_mac {
                        info!("Unexpected MAC address: {}", dest_mac);
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
        let waker = cx.waker().clone();
        self.inner.transmit(cx)
    }

    /// Get the link state.
    ///
    /// This function must return the current link state of the device, and wake `cx.waker()` when
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

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hello World!");
    info!("initializing heap");
    unsafe { HEAP.init(addr_of_mut!(HEAP_MEM) as usize, HEAP_SIZE) };
    info!("heap initialized");

    let mut config = Config::default();
    {
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
    config.baudrate = 1_000_000;

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
    let driver = Driver::new_fs(
        peripherals_instance.USB_OTG_FS,
        Irqs,
        peripherals_instance.PA12,
        peripherals_instance.PA11,
        ep_out_buffer,
        config,
    );

    let mut config = UsbConfig::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("USB-Ethernet example");
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

    // Create classes on the builder.
    static STATE: StaticCell<State> = StaticCell::new();
    let class = CdcNcmClass::new(&mut builder, STATE.init(State::new()), host_mac_addr, 64);

    // Build the builder.
    let usb = builder.build();

    unwrap!(spawner.spawn(usb_task(usb)));
    // Run the USB device.

    static NET_STATE: StaticCell<NetState<MTU, 4, 4>> = StaticCell::new();
    let (runner, device) =
        class.into_embassy_net_device::<MTU, 4, 4>(NET_STATE.init(NetState::new()), our_mac_addr);
    unwrap!(spawner.spawn(usb_ncm_task(runner)));
    let device_wrapper = WrapperDriver::new(
        device,
        EthernetAddress::from_bytes(&our_mac_addr),
        EthernetAddress::from_bytes(&our_mac_addr),
        Ipv4Cidr::new(Ipv4Addr::new(10, 0, 0, 0), 24),
        Ipv4Addr::new(10, 0, 0, 169),
        send_to_uart_task,
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

    unwrap!(spawner.spawn(uart_tx_task(
        uart_sender_component,
        receive_from_ibus_channel
    )));

    unwrap!(spawner.spawn(uart_rx_task(uart_receiver_component, send_to_raw_socket)));

    unwrap!(spawner.spawn(ibus_traffic_to_ethernet_task(
        stack,
        receive_from_uart_task_channel,
        send_to_uart_task
    )))
}
