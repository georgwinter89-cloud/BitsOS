//! Frames in and out of the RTL8852BE (JS js/txrx.mjs with txrx-desc/-rx/-ieee/-data.mjs).
//!
//! TX: the descriptor info of rtw89_core_tx_update_desc_info (core.c) for management and data
//! frames, the WD info dwords pci's `fill_txdesc` takes, mac80211's sequence numbers, and
//! [`tx_frame`] = the whole way of one frame to its ring through pcitx. RX: the received
//! buffers of the RXQ ring as Linux handles them for an AX chip ([`rx_parse`]: segments,
//! frame, C2H or PPDU status, the PPDU status pairing that gives a frame its RSSI, the
//! rx_status mac80211 would get). Plus the 802.11 frames a station sends and parses
//! ([`ieee`], [`parse`]) and the Ethernet <-> 802.11 data frame conversion with the CCMP
//! header ([`data`]).
//!
//! [`Txrx`] is the JS `d.txrx` plus what txrx reads of the rest of the device: the channel
//! (`d.chan`, with the one before it for rtw89_correct_cck_chan), whether a scan runs
//! (`d.sta.scanning`) and the station's default MAC ID (`d.sta.macId`). No registers are
//! touched here except through pcitx; nothing waits.
//!
//! Things the JS driver only returns (dropped buffers) become [`RxEvent`]s the caller drains
//! ([`Txrx::take_events`]) and logs at the boundary (B6).

pub mod data;
pub mod desc;
pub mod ieee;
pub mod parse;
mod ppdu;
mod rx;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_hostile;
mod tx;

use core::fmt;

use crate::events::EventLog;
use crate::phy::Band;

pub use data::{CcmpKey, DataTx, EthRx, ccmp_hdr, ccmp_pn, ccmp_replay, eth_to_wlan, wlan_to_eth};
pub use desc::{RxDesc, get_ch_dma, get_qsel, get_tid_indicate, query_rxdesc, wd_info_dwords};
pub use ppdu::{Encoding, HeGi, PhyOps, PhyPpdu, RxOps, RxStatus, ch_to_freq};
pub use rx::{PPDU_QUEUE_BOUND, RxEntry, RxFrames, RxIn, RxResult, RxState, RxType, rx_parse};
pub use tx::{
    BtcPkt, TxInfo, TxKey, TxKind, TxOpts, TxSent, TxState, TxType, tx_frame, tx_info, tx_seq,
};

/// The channel txrx sees (JS `d.chan`): band and primary channel, and the channel before the
/// last switch (rtw89_chan_rcd, for rtw89_correct_cck_chan)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxrxChan {
    pub band: Band,
    pub ch: u32,
    pub prev: Option<(Band, u32)>,
}

/// What txrx's RX path reports (the JS driver only returned it)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RxEvent {
    /// rtw89_pci_rxbd_deliver_skbs dropped a buffer (`reason` as Linux's message)
    Dropped { reason: &'static str },
    /// Not Linux: more frames waited for their PPDU status than `RxState::ppdu_queue_bound`
    /// (a hostile or broken chip); they went up without it
    PpduQueueFull { flushed: u32 },
}

impl fmt::Display for RxEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RxEvent::Dropped { reason } => write!(f, "RX buffer dropped: {reason}"),
            RxEvent::PpduQueueFull { flushed } => {
                write!(f, "{flushed} frames went up without their PPDU status")
            }
        }
    }
}

/// The host state of the frame path (JS `d.txrx`) and what it reads of the device
#[derive(Debug, Clone, Default)]
pub struct Txrx {
    pub tx: TxState,
    pub rx: RxState,
    /// the channel set (`None` before the first switch)
    pub chan: Option<TxrxChan>,
    /// rtwdev->scanning (rtw89_core_update_rx_freq_from_ie needs it)
    pub scanning: bool,
    /// the station's MAC ID, used when a TX request names none
    pub mac_id: Option<u32>,
    events: EventLog<RxEvent>,
}

impl Txrx {
    pub fn new() -> Txrx {
        Txrx::default()
    }

    // Linux: the RX half of rtw89_pci_reset_trx_rings (pci.c:1776, its RX part at 1843): the
    // half-received frame and the ready descriptor are forgotten (JS pciTxInit; check_rx_tag is
    // false for the 8852BE). Call it with `PciTx::init` after pci_alloc / the PCIe pre-init.
    pub fn pci_reset(&mut self) {
        self.rx.reset();
    }

    /// The RX path's events since the last call, oldest first
    pub fn take_events(&mut self) -> alloc::vec::Vec<RxEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[RxEvent] {
        self.events.as_slice()
    }
}
