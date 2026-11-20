//! `C: NetDevice` (owner of back ends: NW, WL). Ethernet frames in and out; the stack (smoltcp) sits above.

use crate::DevError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LinkState {
    pub up: bool,
    pub speed_mbps: u32,
    pub full_duplex: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NetStats {
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
}

/// SYS-K: what `/proc/net/wireless` shows of a station (Linux's `cfg80211_wireless_stats`): whether it is
/// associated, and the AP's signal in dBm when the driver keeps it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wireless {
    pub associated: bool,
    pub signal_dbm: Option<i32>,
}

pub trait NetDevice: Send + Sync {
    fn name(&self) -> &str;
    fn mac(&self) -> MacAddr;
    fn mtu(&self) -> usize {
        1500
    }
    fn link(&self) -> LinkState;
    /// Queues one Ethernet frame (without FCS). `Err(Timeout)` = TX ring full: the stack retries later.
    fn transmit(&self, frame: &[u8]) -> Result<(), DevError>;
    /// Takes one received frame if any (polled path; the IRQ path wakes the NAPI thread which calls this).
    fn receive(&self, buf: &mut [u8]) -> Option<usize>;
    /// Emergency transmit for the panic mirror: polled, no locks, interrupts off. May drop the frame.
    fn poll_tx(&self, frame: &[u8]) -> bool;
    /// Driver housekeeping (link watch, refill); called from the device's thread or the poller.
    fn poll(&self) {}
    fn stats(&self) -> NetStats {
        NetStats::default()
    }
    /// WL-4 CR 1: the card's route metric, smaller wins (NetworkManager's defaults: ethernet 100, wifi 600), so
    /// the cable keeps the default route whatever order the leases came in.
    fn route_metric(&self) -> u32 {
        100
    }
    /// SYS-K: a wireless card's station state (None: not a wireless card). Must answer at once (a kept value).
    fn wireless(&self) -> Option<Wireless> {
        None
    }
}
