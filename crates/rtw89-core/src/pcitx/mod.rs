//! Frames out on the RTL8852BE's PCIe TX rings and their release reports
//! (JS js/pcitx.mjs), as Linux rtw89 pci.c does it for an AX chip: a WD page
//! (WD body, WD info, WP info, one 8-byte address info) points at the frame, a
//! TX BD points at the WD page, the host index write kicks the ring. Pages come
//! back through the release reports on RPQ and the TX BD index
//! (rtw89_pci_release_tx).
//!
//! Memory (`pci::pci_alloc`): every ACHn/CH8/CH9 ring buffer is one WD page of
//! 128 bytes followed by up to 2048 bytes of frame, so a ring has as many WD
//! pages as BDs (Linux: 512 pages per ring). Page seq = buffer index. CH12 is
//! the firmware command ring (`pci::fwcmd`) and has no WD pages.
//!
//! Things Linux logs (rtw89_warn/err) become [`TxEvent`]s the caller drains
//! ([`PciTx::take_events`]) and turns into `klog::event!`s at the boundary (B6).

#[cfg(any(test, feature = "std"))]
pub mod fakechip;
mod release;
mod submit;
#[cfg(test)]
mod tests;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;

use crate::error::Rtw89Error;
use crate::tables::regs::*;

use crate::pci::rings::{PciRings, TX_BUF, TxRing};

pub use release::ch_dma;

/// RTW89_PCI_TXWD_PAGE_SIZE: the WD page in front of each frame buffer
pub const WD_PAGE: u32 = RTW89_PCI_TXWD_PAGE_SIZE;
/// The frame room behind the WD page (`pci_alloc`'s TX buffer minus the page; no A-MSDU)
pub const FRAME_ROOM: usize = TX_BUF - WD_PAGE as usize;
/// sizeof(struct rtw89_pci_tx_wp_info)
const WP_INFO: usize = 8;
/// sizeof(struct rtw89_pci_tx_addr_info_32)
const ADDR_INFO: usize = 8;

/// Where a WD page is: rtw89_pci_tx_wd.list on the free list, on the ring's busy
/// list, or on none (`list_empty`: given to the chip and its BD reclaimed, or just taken)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Free,
    Busy,
    None,
}

/// What rtw89_pci_tx_wd.queue holds of the frame (the skb), for its report
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxSkb {
    pub len: u32,
    pub qsel: u32,
    pub mac_id: u32,
    /// the 802.11 sequence number of the frame (TxDescInfo `seq`)
    pub wlan_seq: u32,
}

/// struct rtw89_pci_tx_wd: one WD page
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WdPage {
    pub seq: u32,
    /// bus address of the page; the frame follows at `addr + WD_PAGE`
    pub addr: u32,
    /// the bytes of the page the BD covers
    pub len: u32,
    pub on: Place,
    pub skb: Option<TxSkb>,
}

/// struct rtw89_pci_tx_wd_ring with the ring's busy list (rtw89_pci_alloc_tx_wd_ring, pci.c:3539)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WdRing {
    pub name: &'static str,
    pub ch: u8,
    /// the ring buffers the pages were built from: a new `pci_alloc` gives new pages
    bufs: Vec<u32>,
    pub pages: Vec<WdPage>,
    /// free_pages (seqs, oldest first)
    pub free: VecDeque<u32>,
    /// tx_ring->busy_pages (seqs, oldest first)
    pub busy: VecDeque<u32>,
}

impl WdRing {
    // Linux: rtw89_pci_alloc_tx_wd_ring (pci.c:3539): every page on the free list
    fn new(ring: &TxRing) -> WdRing {
        let mut w = WdRing {
            name: ring.name,
            ch: ring.ch,
            bufs: ring.bufs.clone(),
            pages: Vec::with_capacity(ring.bufs.len()),
            free: VecDeque::with_capacity(ring.bufs.len()),
            busy: VecDeque::new(),
        };
        for (seq, &addr) in ring.bufs.iter().enumerate() {
            w.pages.push(WdPage {
                seq: seq as u32,
                addr,
                len: WD_PAGE,
                on: Place::None,
                skb: None,
            });
            w.enqueue(seq as u32);
        }
        w
    }

    // Linux: rtw89_pci_enqueue_txwd (pci.h:1721). The memset of the page is left
    // out: every submit writes all bytes the BD length covers.
    fn enqueue(&mut self, seq: u32) {
        if let Some(p) = self.pages.get_mut(seq as usize) {
            p.on = Place::Free;
            self.free.push_back(seq);
        }
    }

    /// The seqs on the free list, oldest first
    pub fn free_seqs(&self) -> Vec<u32> {
        self.free.iter().copied().collect()
    }
    /// The seqs on the busy list, oldest first
    pub fn busy_seqs(&self) -> Vec<u32> {
        self.busy.iter().copied().collect()
    }
}

/// One release report as the TX status of its frame (rtw89_pci_tx_status, pci.c:460)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxReport {
    /// the ring's name (ACH0, CH8, ...)
    pub ch: &'static str,
    pub seq: u32,
    /// RTW89_PCI_RPP_TX_STATUS: RTW89_TX_DONE (0), RETRY_LIMIT, LIFE_TIME, MACID_DROP
    pub tx_status: u32,
    /// tx_status == RTW89_TX_DONE (IEEE80211_TX_STAT_ACK)
    pub acked: bool,
    pub len: u32,
    pub qsel: u32,
    pub mac_id: u32,
    pub wlan_seq: u32,
}

/// A frame on its ring: `seq` is its WD page, `wp` the ring's host index after it.
/// `kicked` is false when `tx` was asked not to kick, or when its kick failed (the
/// error is a `TxEvent::Fault`): the frame stays queued and the next `kick` of the
/// ring sends it; calling `tx` again would queue it twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queued {
    pub ch: &'static str,
    pub seq: u32,
    pub wp: u32,
    pub kicked: bool,
}

/// What Linux warns about on the TX path (the JS driver's `d.log` lines; `Display` gives
/// the same text). Typed for the boundary's `klog::event!` (B6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxEvent {
    /// rtw89_pci_tx_write: no free BD (the frame was not queued, -ENOSPC)
    NoTxbd { ring: &'static str },
    /// rtw89_pci_txbd_submit: no free WD page (the frame was not queued, -ENOSPC)
    NoTxwd { ring: &'static str },
    /// rtw89_pci_reclaim_txbd: the chip moved its index past more BDs than are busy
    NoBusyTxwd { ring: &'static str },
    /// rtw89_pci_release_txwd_skb: a report came before the TX BD index moved past its page
    NotIdle { ch: u8, seq: u32 },
    /// rtw89_pci_release_tx_skbs: an RPQ buffer without FS and LS (`info` = its rxbd_info)
    NotFsLs { info: u32 },
    /// rtw89_pci_release_tx: a buffer was refused; the `skipped` rest of the RPQ buffers is skipped
    ReleaseFailed { skipped: u32 },
    /// rtw89_pci_release_rpp: a report for a channel the 8852BE has no ring for
    MaskedTxch { txch: u32 },
    /// rtw89_pci_release_rpp: a report seq past Linux's WD ring (RTW89_PCI_TXWD_NUM_MAX)
    InvalidSeq { seq: u32 },
    /// a report seq past this ring's pages (BitsOS has `len` pages, not 512), or no ring
    NoWdPage { txch: u32, seq: u32 },
    /// rtw89_core_get_ch_dma: a report's QSEL maps to no channel; Linux takes ACH0
    QselUnmapped { qsel: u32 },
    /// Not Linux: a bus error or a bad chip index where the path cannot stop: in the
    /// release path after reports were taken (the reports stay delivered), or a failed
    /// kick in `tx` after the frame was queued (`Queued::kicked` is false)
    Fault(Rtw89Error),
}

impl fmt::Display for TxEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxEvent::NoTxbd { ring } => write!(f, "no available TXBD on {ring}"),
            TxEvent::NoTxwd { ring } => write!(f, "no available TXWD on {ring}"),
            TxEvent::NoBusyTxwd { ring } => write!(f, "No busy txwd pages available on {ring}"),
            TxEvent::NotIdle { ch, seq } => write!(f, "queue {ch} txwd {seq} is not idle"),
            TxEvent::NotFsLs { .. } => write!(f, "cannot process RP frame not set FS/LS"),
            TxEvent::ReleaseFailed { .. } => write!(f, "failed to release TX skbs"),
            TxEvent::MaskedTxch { txch } => write!(f, "should no release report on txch {txch}"),
            TxEvent::InvalidSeq { seq } => write!(f, "invalid seq {seq}"),
            TxEvent::NoWdPage { txch, seq } => write!(f, "no WD page {seq} on txch {txch}"),
            TxEvent::QselUnmapped { qsel } => write!(f, "Cannot map qsel to dma: {qsel}"),
            TxEvent::Fault(e) => write!(f, "fault: {e:?}"),
        }
    }
}

/// The TX side's host state (JS `d.txrx.wd`, `d.txrx.txDone`): the WD rings, the
/// reports not handed out yet and the events not drained yet. Both lists only grow
/// between two calls of the caller's poll, which drains them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PciTx {
    pub wd: Vec<WdRing>,
    done: Vec<TxReport>,
    events: Vec<TxEvent>,
}

impl PciTx {
    pub fn new() -> PciTx {
        PciTx::default()
    }

    // Linux: rtw89_pci_ops_reset (pci.c:1878), the parts `pci::reset` leaves out:
    // rtw89_pci_release_tx_ring (pci.c:1871) for every frame ring, all WD pages free
    // again. Call it after `pci_alloc`/`pci::pre_init` (the ring's pages are also built
    // when first used). The RX delivery state rtw89_pci_reset_trx_rings clears
    // (diliver_skb, diliver_desc.ready) is the RX parser's (txrx), which resets it there.
    pub fn init(&mut self, rings: &PciRings) {
        self.wd.clear();
        for ring in rings.tx.iter().filter(|r| r.name != "CH12") {
            self.wd.push(WdRing::new(ring));
        }
        self.done.clear();
    }

    /// The WD ring of the TX ring `name`, if built
    pub fn wd_ring(&self, name: &str) -> Option<&WdRing> {
        self.wd.iter().find(|w| w.name == name)
    }

    /// What happened since the last call (Linux's warnings), oldest first
    pub fn take_events(&mut self) -> Vec<TxEvent> {
        core::mem::take(&mut self.events)
    }
    pub fn events(&self) -> &[TxEvent] {
        &self.events
    }

    /// The reports collected and not handed out yet (a `reclaim` hands them out)
    pub fn pending_reports(&self) -> &[TxReport] {
        &self.done
    }

    // The WD ring of `ring` (JS `wdRing`): built when missing or when the ring's
    // buffers changed (a new pci_alloc)
    fn wd_index(&mut self, ring: &TxRing) -> usize {
        match self.wd.iter().position(|w| w.name == ring.name) {
            Some(i) if self.wd[i].bufs == ring.bufs => i,
            Some(i) => {
                self.wd[i] = WdRing::new(ring);
                i
            }
            None => {
                self.wd.push(WdRing::new(ring));
                self.wd.len() - 1
            }
        }
    }
}

/// The TX ring of DMA channel `ch` (JS `txRing`), or an error naming it
fn tx_ring_index(rings: &PciRings, ch: u32) -> Result<usize, Rtw89Error> {
    rings
        .tx
        .iter()
        .position(|r| u32::from(r.ch) == ch)
        .ok_or(Rtw89Error::TxRefused {
            what: "no TX ring for this DMA channel",
            ch,
            value: ch,
        })
}
