//! The WLAN as an Ethernet-like card of the network stack (spec/wlan.md §3 "a netdev layer with
//! several interfaces at once"; 802.11 <-> 802.3 is rtw89-core's txrx). The link is up only once
//! the group key is in (spec §2.4 trap 4: the DHCP offer comes as a broadcast), so the stack's DHCP
//! (NW: 7 tries 1 s apart, then 2, 4, 8, 16, 32 s, then every 60 s; renew and rebind by smoltcp)
//! starts after the GTK. Receive and transmit never wait for the chip: they move frames through
//! the card's queues, the driver thread does the rest and wakes the stack when frames came.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use devmodel::DevError;
use devmodel::net::{LinkState, MacAddr, NetDevice, NetStats};

use crate::card::Card;

/// Legacy rates only (no HT/VHT/HE yet): 54 Mbit/s at most
pub const LINK_MBPS: u32 = 54;

pub struct WlanNet(pub Arc<Card>);

impl NetDevice for WlanNet {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn mac(&self) -> MacAddr {
        MacAddr(self.0.mac_bytes())
    }
    /// WL-4 CR 1: the cable wins the default route (NetworkManager's wifi metric).
    fn route_metric(&self) -> u32 {
        600
    }
    fn link(&self) -> LinkState {
        let up = self.0.link_up.load(Ordering::Acquire);
        LinkState { up, speed_mbps: if up { LINK_MBPS } else { 0 }, full_duplex: false }
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), DevError> {
        self.0.queue_tx(frame)
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        self.0.take_rx(buf)
    }
    /// The panic mirror never goes over the WLAN: no chip access without the thread's lock
    fn poll_tx(&self, _frame: &[u8]) -> bool {
        false
    }
    fn stats(&self) -> NetStats {
        let c = &self.0;
        let s = c.status.lock().stats;
        NetStats {
            rx_frames: s.rx_frames,
            tx_frames: s.tx_frames,
            rx_bytes: s.rx_bytes,
            tx_bytes: s.tx_bytes,
            rx_dropped: s.rx_dropped + c.rx_overflow.load(Ordering::Relaxed),
            tx_dropped: s.tx_dropped + c.tx_refused.load(Ordering::Relaxed) + c.tx_no_link.load(Ordering::Relaxed),
        }
    }
}
