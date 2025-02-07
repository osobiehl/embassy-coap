use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::fmt::format;
use alloc::string::{String, ToString};
extern crate alloc;
use alloc::{format, vec};
use coap_lite::{CoapRequest, CoapResponse, RequestType};
use embassy_net::udp::{PacketMetadata, UdpSocket};

use defmt::{info, warn, Debug2Format};
use embassy_net::{EthernetAddress, IpEndpoint, IpListenEndpoint, Ipv4Cidr, StackResources};

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
pub trait Resource: Send {
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

pub struct InfoResource {}

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

pub struct CoapStateManager {
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

#[embassy_executor::task]
pub async fn coap_task(
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
            port: 5683,
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
        let _ = socket
            .send_to(&msg_bytes, metadata.endpoint)
            .await
            .inspect_err(|e| warn!("send response failed: {}", e));
    }
}
