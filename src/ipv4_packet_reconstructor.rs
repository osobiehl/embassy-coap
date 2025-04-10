use alloc::vec::Vec;
use defmt::{debug, info, warn};
use smoltcp::wire::Ipv4Packet;

use crate::ChannelSender;

pub struct Ipv4PacketParser {
    buffer: Vec<u8>,
    expected_len: Option<usize>,
    expected_header_len: u8,
    sender: ChannelSender,
}

impl Ipv4PacketParser {
    const CRC_SIZE: usize = 2;
    const MAX_PACKET_SIZE: usize = 2000;
    pub fn new(sender: ChannelSender) -> Self {
        Self {
            buffer: Vec::new(),
            expected_len: None,
            expected_header_len: 0,
            sender,
        }
    }

    /// Pushes a byte into the parser. If the byte completes a packet, the packet is sent to the
    /// channel sender. If a packet is invalid, false is returned, otherwise true is returned.
    pub fn push_byte(&mut self, byte: u8) -> bool {
        self.buffer.push(byte);

        if self.buffer.len() >= 20 {
            if self.expected_header_len == 0{
                let packet = Ipv4Packet::new_unchecked(&self.buffer);
                debug!("header length: {}", packet.header_len());
                self.expected_header_len = packet.header_len();
            };
        }

        if self.buffer.len()  == self.expected_header_len as usize{
            let packet = Ipv4Packet::new_unchecked(&self.buffer);
            let total_length = packet.total_len() as usize;
            if !packet.verify_checksum() || total_length > Self::MAX_PACKET_SIZE{
                warn!("Invalid IPv4 header checksum, discarding packet");
                self.buffer.clear();
                self.expected_len = None;
                self.expected_header_len = 0;
                return false;
            }
            info!("expected total length of: {}", total_length);
            self.expected_len = Some(total_length + Self::CRC_SIZE);
        }

        if let Some(expected) = self.expected_len {
            if self.buffer.len() == expected {
                self.process_and_send_packet();
            }
        }
        return true;
    }

    pub fn push_slice(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if !self.push_byte(byte) {
                warn!("could not push byte, breaking early and ignoring the rest of the slice");
                return;
            }
        }
    }

    fn process_and_send_packet(&mut self) {
        if let Ok(_) = Ipv4Packet::new_checked(&self.buffer[..self.buffer.len() - Self::CRC_SIZE]) {
            let packet = core::mem::take(&mut self.buffer);
            if let Err(_) = self.sender.try_send(packet) {
                warn!("could not send ip packet: everything full , dropping packet",);
            } else {
                info!("sent ip packet");
            }
        } else {
            warn!("Invalid IPv4 packet, discarding");
        }
        self.buffer.clear();
        self.expected_len = None;
        self.expected_header_len = 0;
    }
}
