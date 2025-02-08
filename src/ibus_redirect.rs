extern crate alloc;
use defmt::*;
use defmt_rtt as _; // global logger
use embassy_net::driver::Driver as NetDriver;
use embassy_stm32::mode::Async;
use embassy_stm32::usart::{UartRx, UartTx};
use serial_line_ip::{Decoder, Encoder};

use crate::{ChannelReceiver, ChannelSender};
use alloc::vec;
use panic_probe as _;

#[embassy_executor::task]
pub async fn uart_rx_task(
    mut rx_uart: UartRx<'static, Async>,
    send_to_raw_socket: ChannelSender,
) -> ! {
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
        info!("read: {} bytes", uart_read_bytes);
        let mut read_slice = &rx_buffer[..uart_read_bytes];
        while read_slice.len() != 0 {
            match slip_decoder.decode(read_slice, slip_decode_buffer_slice) {
                Ok((bytes_processed, output_slice, is_end_of_packet)) => {
                    next_packet.extend_from_slice(output_slice);
                    if is_end_of_packet {
                        info!("sending decoded slip bytes to ethernet redirect");
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
                    warn!("decoding failed: {:?}", Debug2Format(&e));
                    read_slice = &[];
                }
            }
        }
    }
}

#[embassy_executor::task]
pub async fn uart_tx_task(
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
            Ok(_) => {}
            Err(e) => warn!("uart transmission failed: {:?}", e),
        }
    }
}
