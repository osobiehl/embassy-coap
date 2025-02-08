extern crate alloc;
use defmt::*;
use defmt_rtt as _; // global logger
use embassy_net::driver::Driver as NetDriver;
use embassy_stm32::mode::Async;
use embassy_stm32::usart::{UartRx, UartTx};
use serial_line_ip::{Decoder, Encoder};

use crate::{log_packet, ChannelReceiver, ChannelSender};
use alloc::vec;
use panic_probe as _;

#[embassy_executor::task]
pub async fn uart_rx_task(rx_uart: UartRx<'static, Async>, send_to_raw_socket: ChannelSender) -> ! {
    // this is a lot of memcpy -- oh well
    let mut rx_buffer = [0u8; 8000];
    let mut uart_ringbuffered = rx_uart.into_ring_buffered(&mut rx_buffer);

    let mut slip_decode_buffer = [0; 4000];
    let mut message_buffer = [0; 3000];

    //let rx_ringbuffer = rx_uart.into_ringbuffer();
    let slip_decode_buffer_slice = &mut slip_decode_buffer;

    let mut slip_decoder = serial_line_ip::Decoder::new();
    let mut slip_state_buffer = vec![];
    let mut next_packet = vec![];
    loop {
        let uart_read_bytes = match uart_ringbuffered.read(&mut message_buffer).await {
            Ok(b) => b,
            Err(e) => {
                warn!("uart reception failed: {:?}", e);
                continue;
            }
        };
        info!("uart read: {} bytes", uart_read_bytes);
        slip_state_buffer.extend_from_slice(&message_buffer[..uart_read_bytes]);
        while !slip_state_buffer.is_empty() {
            match slip_decoder.decode(&slip_state_buffer, slip_decode_buffer_slice) {
                Ok((bytes_processed, output_slice, is_end_of_packet)) => {
                    next_packet.extend_from_slice(output_slice);
                    // Remove the processed bytes from the buffer
                    slip_state_buffer.drain(..bytes_processed);
                    if is_end_of_packet {
                        debug!("Sending decoded SLIP bytes to Ethernet redirect");
                        log_packet(&next_packet, "Packet right after recording");
                        // mem::take clears the vector and leaves it empty
                        send_to_raw_socket
                            .send(core::mem::take(&mut next_packet))
                            .await;

                        // Reset the decoder for the next packet
                        slip_decoder = Decoder::new();
                    } else {
                        warn!("Split packet received: Continuing to accumulate bytes");
                        break;
                    }
                }
                Err(e) => {
                    // Reset everything on error
                    slip_decoder = Decoder::new();
                    next_packet.clear();
                    warn!("Decoding failed: {:?}", Debug2Format(&e));
                    slip_state_buffer.clear(); // Clear the current buffer to recover cleanly
                    break;
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
        let mut bytes_written = match encoder.encode(&rx, &mut encode_buffer) {
            Ok(t) => t.written,
            Err(e) => {
                warn!("slip encoding failed: {:?}", Debug2Format(&e));
                continue;
            }
        };
        debug!("writing slip {:?} encoded bytes to ibus", bytes_written);
        let finish_byte = match encoder.finish(&mut encode_buffer[bytes_written..]) {
            Ok(b) => b.written,
            Err(e) => {
                warn!(
                    "could not finish encoding slip packet: {:?}",
                    Debug2Format(&e)
                );
                continue;
            }
        };
        bytes_written += finish_byte;
        match tx_uart.write(&encode_buffer[..bytes_written]).await {
            Ok(_) => {}
            Err(e) => warn!("uart transmission failed: {:?}", e),
        }
    }
}
