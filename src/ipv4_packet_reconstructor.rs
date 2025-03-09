use alloc::vec::Vec;
use defmt::{debug, info, warn};
use smoltcp::wire::Ipv4Packet;

use crate::ChannelSender;

pub struct Ipv4PacketParser {
    buffer: Vec<u8>,
    expected_len: Option<usize>,
    sender: ChannelSender,
}

impl Ipv4PacketParser {
    const CRC_SIZE: usize = 2;
    pub fn new(sender: ChannelSender) -> Self {
        Self {
            buffer: Vec::new(),
            expected_len: None,
            sender,
        }
    }

    /// Pushes a byte into the parser. If the byte completes a packet, the packet is sent to the
    /// channel sender. If a packet is invalid, false is returned, otherwise true is returned.
    pub fn push_byte(&mut self, byte: u8) -> bool {
        self.buffer.push(byte);

        if self.buffer.len() == 20 {
            // Perform header checksum verification once the full header is received
            let packet = Ipv4Packet::new_unchecked(&self.buffer);
            if packet.header_len() as usize > self.buffer.len() {
                warn!(
                    "incorrect packet received: expected a header length of {} but got {}",
                    packet.header_len(),
                    self.buffer.len()
                );
                self.buffer.clear();
                self.expected_len = None;
                return false;
            }
            debug!("header length: {}", packet.header_len());
            if !packet.verify_checksum() {
                warn!("Invalid IPv4 header checksum, discarding packet");
                self.buffer.clear();
                self.expected_len = None;
                return false;
            }
            let total_length = packet.total_len() as usize;
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
    }
}
