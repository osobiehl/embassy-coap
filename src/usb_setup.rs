extern crate alloc;
use defmt::*;
use defmt_rtt as _; // global logger
use embassy_futures::select::{select, Either};
use embassy_net::driver::{
    Driver as NetDriver, LinkState,
};
use embassy_net_driver_channel::{RxRunner, StateRunner, TxRunner};
use embassy_stm32::peripherals;
use embassy_usb::class::cdc_ncm::{
    Receiver as UsbReceiver, Sender as UsbSender,
};
use embassy_usb::UsbDevice;


use panic_probe as _;

use crate::{ChannelReceiver, MTU};

pub type StmUsbDriver<'a> = embassy_stm32::usb::Driver<'a, peripherals::USB_OTG_FS>;

#[embassy_executor::task]
pub async fn usb_task(mut device: UsbDevice<'static, StmUsbDriver<'static>>) -> ! {
    device.run().await
}

#[embassy_executor::task]
pub async fn usb_reception_task_rx(
    mut rx_usb: UsbReceiver<'static, StmUsbDriver<'static>>,
    mut rx_chan: RxRunner<'static, MTU>,
    state_chan: StateRunner<'static>,
) -> ! {
    loop {
        trace!("WAITING for connection");
        state_chan.set_link_state(LinkState::Down);

        rx_usb.wait_connection().await.unwrap();

        trace!("Connected");
        state_chan.set_link_state(LinkState::Up);

        loop {
            let p = rx_chan.rx_buf().await;
            match rx_usb.read_packet(p).await {
                Ok(n) => rx_chan.rx_done(n),
                Err(e) => {
                    warn!("error reading packet: {:?}", e);
                    break;
                }
            };
        }
    }
}

#[embassy_executor::task]
pub async fn usb_sending_task_tx(
    mut tx_usb: UsbSender<'static, StmUsbDriver<'static>>,
    mut tx_chan: TxRunner<'static, MTU>,
    receiver_for_ibus_packets: ChannelReceiver,
) {
    let mut holder_vec;
    loop {
        let rx_from_ibus_receiver = receiver_for_ibus_packets.receive();
        let rx_from_stack = tx_chan.tx_buf();
        let rx = select(rx_from_stack, rx_from_ibus_receiver).await;
        let mut should_signal = false;
        let recv_slice = match rx {
            Either::First(s) => {
                should_signal = true;
                s
            }
            Either::Second(v) => {
                holder_vec = v;
                holder_vec.as_slice()
            }
        };
        if let Err(e) = tx_usb.write_packet(recv_slice).await {
            warn!("Failed to TX packet: {:?}", e);
        }
        if should_signal {
            tx_chan.tx_done();
        }
    }
}
