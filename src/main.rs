#![no_std]
#![no_main]

use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::fmt::format;
use alloc::string::{String, ToString};
use coap_lite::{CoapRequest, CoapResponse, RequestType};
use core::cmp::Ordering;
use core::future::Future;
use core::marker::PhantomData;
use core::net::Ipv4Addr;
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::addr_of_mut;
use defmt::{panic, *};
use defmt_rtt as _; // global logger
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_net::tcp::TcpSocket;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpEndpoint, IpListenEndpoint, Ipv4Cidr, StackResources};
use embassy_stm32::usb::{Driver, Instance};
use embassy_stm32::{bind_interrupts, peripherals, rng, usb, Config};
use embassy_usb::class::cdc_ncm::embassy_net::{Device, Runner, State as NetState};
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State};
use embassy_usb::{Builder, Config as UsbConfig, UsbDevice};
use embedded_io_async::Write;
use heapless::Vec;
use panic_probe as _;
use rand_core::RngCore;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    OTG_FS => usb::InterruptHandler<peripherals::USB_OTG_FS>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
});
use embedded_alloc::LlffHeap as Heap;

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
async fn net_task(mut runner: embassy_net::Runner<'static, Device<'static, MTU>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn coap_task(
    stack: embassy_net::Stack<'static>,
    mut coap_state_manager: CoapStateManager,
) -> ! {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut tx_metadata_buffer = [PacketMetadata::EMPTY; 10];
    let mut rx_metadata_buffer = [PacketMetadata::EMPTY; 10];

    let mut buf = [0; 4096];

    let mut socket = UdpSocket::new(
        stack,
        &mut rx_metadata_buffer,
        &mut rx_buffer,
        &mut tx_metadata_buffer,
        &mut tx_buffer,
    );
    socket
        .bind(IpListenEndpoint {
            addr: None,
            port: 1234,
        })
        .expect("could not pind to port");

    loop {
        let result = socket.recv_from(&mut buf).await;
        let (bytes_read, metadata) = match result {
            Err(e) => {
                warn!("udp read error: {:?}", e);
                continue;
            }
            Ok(b) => b,
        };

        let coap_packet = match coap_lite::Packet::from_bytes(&buf[..bytes_read]) {
            Ok(p) => p,
            Err(e) => {
                warn!("coap packet parsing failed: {}", defmt::Debug2Format(&e));
                continue;
            }
        };
        info!("coap packet: {:?}", Debug2Format(&coap_packet));

        let coap_request = CoapRequest::from_packet(coap_packet, metadata.endpoint);
        let response = coap_state_manager.handle_request(&coap_request).await;
        let Some(response_packet) = response else {
            info!("no response needed, continue");
            continue;
        };

        let msg_bytes = match response_packet.message.to_bytes() {
            Ok(m) => m,
            Err(e) => {
                warn!(
                    "serialization of coap packet failed: {:?}",
                    Debug2Format(&e)
                );
                continue;
            }
        };
        let response = socket.send_to(&msg_bytes, metadata.endpoint).await;
    }
}

extern crate alloc;
use alloc::format;

pub fn not_implemented(request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
    response.set_status(coap_lite::ResponseType::NotImplemented);
    let method = request.get_method();
    response.message.payload = format!(
        "method not implemented: {:?} -> {}",
        method,
        request.get_path()
    )
    .into_bytes();
}
pub fn not_found(request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
    response.set_status(coap_lite::ResponseType::NotFound);
    response.message.payload = format!("resource not found:  {}", request.get_path()).into_bytes();
}

#[async_trait::async_trait]
trait Resource: Send {
    fn name(&self) -> &str;
    async fn get(&mut self, request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
        not_implemented(request, response);
    }
    async fn put(&mut self, request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
        not_implemented(request, response);
    }
    async fn post(&mut self, request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
        not_implemented(request, response);
    }
}

struct InfoResource {}

#[async_trait::async_trait]
impl Resource for InfoResource {
    fn name(&self) -> &str {
        return "info";
    }
    async fn get(&mut self, _request: &CoapRequest<IpEndpoint>, response: &mut CoapResponse) {
        response.set_status(coap_lite::ResponseType::Valid);
        response.message.payload = b"this is written in rust :P".to_vec();
    }
}

struct CoapStateManager {
    handlers: BTreeMap<String, Box<dyn Resource>>,
}

impl CoapStateManager {
    pub fn new() -> Self {
        Self {
            handlers: Default::default(),
        }
    }
    pub fn add_resource<R: Resource + 'static>(&mut self, resource: R) {
        let r = Box::new(resource);
        self.add_dyn_resource(r);
    }
    pub fn add_dyn_resource(&mut self, resource: Box<dyn Resource>) {
        if let Some(r) = self.handlers.insert(resource.name().to_string(), resource) {
            warn!("resource with name replaced: {}", r.name())
        }
    }

    pub async fn handle_request(
        &mut self,
        request: &CoapRequest<IpEndpoint>,
    ) -> Option<CoapResponse> {
        let mut response = CoapResponse::new(&request.message)?;

        match self.handlers.get_mut(&request.get_path()) {
            None => {
                not_found(request, &mut response);
            }
            Some(handler) => match request.get_method() {
                RequestType::Get => handler.get(request, &mut response).await,
                RequestType::Put => handler.put(request, &mut response).await,
                RequestType::Post => handler.post(request, &mut response).await,
                _ => not_implemented(request, &mut response),
            },
        };
        return Some(response);
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

    let p = embassy_stm32::init(config);

    // Create the driver, from the HAL.

    static EP_OUT_BUFFER: StaticCell<[u8; 256]> = StaticCell::new();
    let ep_out_buffer = EP_OUT_BUFFER.init_with(|| [0u8; 256]);

    let mut config = embassy_stm32::usb::Config::default();
    // Do not enable vbus_detection. This is a safe default that works in all boards.
    // However, if your USB device is self-powered (can stay powered on if USB is unplugged), you need
    // to enable vbus_detection to comply with the USB spec. If you enable it, the board
    // has to support it or USB won't work at all. See docs on `vbus_detection` for details.
    config.vbus_detection = false;
    let driver = Driver::new_fs(p.USB_OTG_FS, Irqs, p.PA12, p.PA11, ep_out_buffer, config);

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
    // Host's MAC addr. This is the MAC the host "thinks" its USB-to-ethernet adapter has.
    let host_mac_addr = [0x88, 0x88, 0x88, 0x88, 0x88, 0x88];

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

    // let config = embassy_net::Config::dhcpv4(Default::default());
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: Ipv4Cidr::new(Ipv4Addr::new(10, 0, 0, 169), 24),
        dns_servers: Vec::new(),
        gateway: Some(Ipv4Addr::new(10, 0, 0, 1)),
    });

    let mut rng = embassy_stm32::rng::Rng::new(p.RNG, Irqs);

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) =
        embassy_net::new(device, config, RESOURCES.init(StackResources::new()), seed);

    unwrap!(spawner.spawn(net_task(runner)));

    let resource = InfoResource {};
    let mut manager = CoapStateManager::new();
    manager.add_resource(resource);
    unwrap!(spawner.spawn(coap_task(stack, manager)))
}
