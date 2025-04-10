extern crate alloc;
use defmt::*;
use defmt_rtt as _;
use embassy_futures::select;
// global logger
use embassy_net::driver::Driver as NetDriver;
use embassy_stm32::mode::Async;
use embassy_stm32::peripherals::RNG;
use embassy_stm32::rng::Rng;
use embassy_stm32::usart::{RingBufferedUartRx, UartRx, UartTx};
use embassy_time::{with_timeout, Duration, Timer};
use embedded_io_async::{Read, ReadReady};
use serial_line_ip::{Decoder, Encoder};

use crate::ipv4_packet_reconstructor::Ipv4PacketParser;
use crate::{log_packet, CarrierSenseTimer, ChannelReceiver, ChannelSender};
use alloc::vec;
use alloc::vec::Vec;
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
        debug!("uart read: {} bytes", uart_read_bytes);
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

pub enum InternalBusWriteError {
    Collision,
    UartError,
    IbusDisconnected,
}
pub async fn write_to_internal_bus(
    send_slice: &[u8],
    tx_uart: &mut UartTx<'static, Async>,
    rx_uart: &mut RingBufferedUartRx<'_>,
    input_feeder: &mut Ipv4PacketParser,
) -> Result<(), InternalBusWriteError> {
    let mut rx_dummy_buffer = [0u8; 1500];
    if let Ok(true) = rx_uart.read_ready() {
        warn!("ibus is busy, not writing");
        return Err(InternalBusWriteError::Collision);
    }
    let _ = match tx_uart.write(send_slice).await {
        Ok(()) => {}
        Err(e) => warn!("tx failed: collision likely {:?}", e),
    };
    match with_timeout(
        Duration::from_millis(100),
        rx_uart.read_exact(&mut rx_dummy_buffer[..send_slice.len()]),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            warn!("read failed: collision likely {:?}", e);
            return Err(InternalBusWriteError::UartError);
        }
        Err(_) => {
            warn!("timeout: are pins properly connected?");
            return Err(InternalBusWriteError::IbusDisconnected);
        }
    };

    let receive_slice = &rx_dummy_buffer[..send_slice.len()];
    if send_slice != receive_slice {
        warn!(
            "collision detected, sent {} bytes but receied {}",
            send_slice.len(),
            receive_slice.len()
        );
        info!("tx {:?}", send_slice);
        info!("rx {:?}", receive_slice);
        return Err(InternalBusWriteError::Collision);
    };
    //input_feeder.push_slice(&rx_dummy_buffer[..s]);
    debug!("wrote {} bytes to internal bus!", send_slice.len());
    //Timer::after(Duration::from_millis(1)).await;
    Ok(())
}
pub struct CarrierSenseBackoffCaluator<R: rand_core::RngCore> {
    current_retries: u8,
    max_retries: u8,
    base_backoff: Duration,
    random_component_microseconds: u32,
    random: R,
}

impl<R: rand_core::RngCore> CarrierSenseBackoffCaluator<R> {
    pub fn new(
        max_retries: u8,
        base_backoff: Duration,
        random_component_microseconds: u32,
        random: R,
    ) -> Self {
        Self {
            current_retries: 0,
            max_retries,
            base_backoff,
            random_component_microseconds,
            random,
        }
    }
    fn get_random_backoff(&mut self) -> Duration {
        let duration = self.random.next_u32() % self.random_component_microseconds;
        Duration::from_micros(duration as u64)
    }

    pub fn backoff(&mut self) -> Duration {
        let mut backoff = self.base_backoff * (self.current_retries as u32 + 1);
        if self.current_retries < self.max_retries {
            self.current_retries += 1
        }

        backoff = backoff + self.get_random_backoff();
        info!("backing off for {:?}", backoff);
        backoff
    }

    pub fn reset(&mut self) {
        self.current_retries = 0;
    }
}

#[embassy_executor::task]
pub async fn ibus_half_duplex_task(
    mut tx_uart: UartTx<'static, Async>,
    receive_from_raw_socket_to_uart: ChannelReceiver,
    rx_uart: UartRx<'static, Async>,
    mut packet_feeder: Ipv4PacketParser,
    idle_detector: CarrierSenseTimer,
    mut backoff_calculator: CarrierSenseBackoffCaluator<Rng<RNG, 'static>>,
) {
    // this is a lot of memcpy -- oh well
    let mut rx_ring_buffer = [0u8; 8000];
    let mut read_buffer = [0u8; 1500];

    let mut vec_to_send: Vec<u8> = vec![];

    let mut uart_ringbuffered = rx_uart.into_ring_buffered(&mut rx_ring_buffer);
    uart_ringbuffered.start_uart();
    loop {
        if vec_to_send.is_empty() {
            let rx_uart_fut = uart_ringbuffered.read(&mut read_buffer);
            let tx_request_fut = receive_from_raw_socket_to_uart.receive();
            let result = select::select(rx_uart_fut, tx_request_fut).await;
            match result {
                select::Either::First(ringbuffer_read_result) => {
                    if let Ok(bytes_read) = ringbuffer_read_result {
                        debug!("read {} bytes", bytes_read);
                        packet_feeder.push_slice(&read_buffer[..bytes_read]);
                        //Timer::after(Duration::from_millis(1)).await;
                    }
                }
                select::Either::Second(bytes_to_send) => {
                    if !idle_detector.line_idle()
                        || write_to_internal_bus(
                            &bytes_to_send,
                            &mut tx_uart,
                            &mut uart_ringbuffered,
                            &mut packet_feeder,
                        )
                        .await
                        .is_err()
                    {
                        vec_to_send = bytes_to_send;
                        info!("line busy, retrying");
                    }
                }
            }
        } else {
            // wait for the bus to be idle
            let Ok(bytes_read) = uart_ringbuffered.read(&mut read_buffer).await else {
                warn!("reception failed");
                continue;
            };
            Timer::after(backoff_calculator.backoff()).await;
            packet_feeder.push_slice(&read_buffer[..bytes_read]);
            if !idle_detector.line_idle()
                || write_to_internal_bus(
                    &vec_to_send,
                    &mut tx_uart,
                    &mut uart_ringbuffered,
                    &mut packet_feeder,
                )
                .await
                .is_err()
            {
                warn!("line still idle after retry :(");
                continue;
            } else {
                vec_to_send.clear();
                backoff_calculator.reset();
            }
        }
    }
}
