//! Received buffers of the RXQ ring as Linux rtw89 handles them for an AX chip (JS
//! js/txrx-rx.mjs): rtw89_pci_rxbd_deliver_skbs (segments) and rtw89_core_rx (frame, C2H or
//! PPDU status). No registers are touched here: `pci::rx_poll` does the ring part.

use alloc::vec::Vec;

use crate::fw::{C2h, c2h_parse};
use crate::pci::RxBuf;
use crate::pci::rx::RXBD_INFO;
use crate::tables::regs::*;

use super::desc::{RXD_LONG, RXD_SHORT, RxDesc, query_rxdesc};
use super::ppdu::{self, PhyPpdu, RxOps, RxStatus};
use super::{RxEvent, Txrx};

/// One buffer for `rx_parse`: what `pci::rx_poll` took (data after the rxbd_info, FS/LS, its
/// WRITE_SIZE), or one whole packet from its RX descriptor on (`len` `None`)
#[derive(Debug, Clone, Copy)]
pub struct RxIn<'a> {
    pub data: &'a [u8],
    pub fs: bool,
    pub ls: bool,
    /// RTW89_PCI_RXBD_WRITE_SIZE (counts the rxbd_info too); `None`: a whole packet
    pub len: Option<u32>,
}

impl<'a> RxIn<'a> {
    /// A whole packet, RX descriptor first
    pub fn packet(data: &'a [u8]) -> RxIn<'a> {
        RxIn {
            data,
            fs: true,
            ls: true,
            len: None,
        }
    }
}

impl<'a> From<&'a RxBuf> for RxIn<'a> {
    fn from(b: &'a RxBuf) -> RxIn<'a> {
        RxIn {
            data: &b.data,
            fs: b.fs,
            ls: b.ls,
            len: Some(b.len),
        }
    }
}

/// What one buffer was (the JS result's `type`)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxType {
    Frame,
    Ppdu,
    C2h,
    Other,
    /// a first or middle segment, kept
    Partial,
    Dropped,
}

/// A frame handed to mac80211 (rtw89_core_rx_to_mac80211)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxEntry {
    pub desc: RxDesc,
    /// the frame as Linux hands it up, FCS included (RX_INCLUDES_FCS)
    pub frame: Vec<u8>,
    pub status: RxStatus,
    /// Linux's phy_ppdu: the PPDU status the frame waited for (also when it did not match)
    pub ppdu: Option<PhyPpdu>,
    /// the PPDU status' descriptor Linux passes on with it
    pub ppdu_desc: Option<RxDesc>,
    /// rtw89_core_rx_ppdu_match: counter, rate, GI and bandwidth agree (only then the signal)
    pub ppdu_match: bool,
}

impl RxEntry {
    /// The frame without FCS, as mac80211 trims it
    pub fn mpdu(&self) -> &[u8] {
        &self.frame[..self.frame.len().saturating_sub(super::data::FCS_LEN)]
    }
}

/// What one buffer gave (JS CONTRACTS.md's `{ type, desc, frame, ppdu }` plus `deliver`)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxResult {
    pub typ: RxType,
    pub desc: Option<RxDesc>,
    /// the packet of this buffer when it went up at once (it is also in `deliver`), or the
    /// C2H / other report's bytes
    pub frame: Option<Vec<u8>>,
    pub ppdu: Option<PhyPpdu>,
    /// the frames handed to mac80211 by this call, in Linux's order (flushed frames first)
    pub deliver: Vec<RxEntry>,
    /// the frame waits for its PPDU status (it comes up in a later `deliver`); its length
    pub pending: Option<usize>,
    pub c2h: Option<C2h>,
    /// why a buffer was dropped (Linux's message)
    pub reason: Option<&'static str>,
}

impl RxResult {
    pub(super) fn new(typ: RxType, desc: Option<RxDesc>) -> RxResult {
        RxResult {
            typ,
            desc,
            frame: None,
            ppdu: None,
            deliver: Vec::new(),
            pending: None,
            c2h: None,
            reason: None,
        }
    }
}

/// The half-received packet (rx_ring->diliver_skb)
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RxSeg {
    data: Vec<u8>,
    room: usize,
}

/// The RX delivery and PPDU state (JS `d.txrx` RX part)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxState {
    /// rtwdev->ppdu_sts.curr_rx_ppdu_cnt (U8_MAX after rtw89_core_ppdu_sts_init)
    pub ppdu_curr: u32,
    /// frames waiting for their PPDU status (ppdu_sts.rx_queue)
    pub queue: Vec<RxEntry>,
    /// rx_ring->diliver_desc: one per ring, long-only fields keep their values
    pub desc: RxDesc,
    seg: Option<RxSeg>,
    /// Not Linux: a bound against a hostile or broken chip that sends frames waiting for a
    /// PPDU status that never comes (Linux queues without bound). Past it the waiting frames
    /// go up without PPDU status (an event says so). Not a hardware count (R1).
    pub ppdu_queue_bound: usize,
}

/// The default `RxState::ppdu_queue_bound`: far above one A-MPDU's 256 frames
pub const PPDU_QUEUE_BOUND: usize = 1024;

impl Default for RxState {
    fn default() -> RxState {
        RxState {
            ppdu_curr: 0xff,
            queue: Vec::new(),
            desc: RxDesc::default(),
            seg: None,
            ppdu_queue_bound: PPDU_QUEUE_BOUND,
        }
    }
}

impl RxState {
    // the RX half of rtw89_pci_reset_trx_rings (pci.c:1776, its RX part at 1843):
    // diliver_skb, diliver_desc.ready
    pub(super) fn reset(&mut self) {
        self.seg = None;
        self.desc.ready = false;
    }
    /// A packet is half received (a first segment came, the last not yet)
    pub fn in_packet(&self) -> bool {
        self.seg.is_some()
    }
}

fn dropped(t: &mut Txrx, reason: &'static str) -> RxResult {
    t.rx.seg = None;
    t.rx.desc.ready = false;
    t.events.push(RxEvent::Dropped { reason });
    let mut r = RxResult::new(RxType::Dropped, None);
    r.reason = Some(reason);
    r
}

// Linux: rtw89_pci_rxbd_deliver_skbs (pci.c:321) for one buffer of `pci::rx_poll` or one whole
// packet, then rtw89_core_rx (core.c:4527). `ops`: the 8852B chip ops phy owns
// (convert_rpl_to_rssi, query_ppdu, cfo_parse) and rx_stats at rtw89_core_rx_stats' place.
// Frames go up only through the result's `deliver`. A torn buffer is dropped with a reason,
// never a panic: on the chip an error would end the stage and power off.
pub fn rx_parse(t: &mut Txrx, rx: RxIn, ops: &mut dyn RxOps) -> RxResult {
    let data = rx.data;
    // rx_info->len counts from the start of the buffer, rxbd_info included
    let end = match rx.len {
        None => data.len(),
        Some(len) => (len as usize).saturating_sub(RXBD_INFO).min(data.len()),
    };
    // rtw89_pci_validate_rx_tag: check_rx_tag is false for the 8852BE; no_rxbd_fs false
    let start = if rx.fs {
        if t.rx.seg.is_some() {
            return dropped(t, "skb ready before first segment");
        }
        if t.rx.desc.ready {
            return dropped(t, "desc ready before first segment");
        }
        let long = data
            .get(..4)
            .is_some_and(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & AX_RXD_LONG_RXD != 0);
        if data.len() < RXD_SHORT || (long && data.len() < RXD_LONG) {
            return dropped(t, "buffer shorter than its RX descriptor");
        }
        query_rxdesc(data, 0, &mut t.rx.desc);
        // rtw89_alloc_skb_for_rx(pkt_size)
        t.rx.seg = Some(RxSeg {
            data: Vec::with_capacity(t.rx.desc.pkt_size as usize),
            room: t.rx.desc.pkt_size as usize,
        });
        t.rx.desc.offset + t.rx.desc.rxd_len // the first segment has the RX desc
    } else {
        if t.rx.seg.is_none() {
            return dropped(t, "no last skb");
        }
        0
    };
    // rtw89_skb_put_rx_data
    let room = t.rx.seg.as_ref().map_or(0, |s| s.room);
    let mut copy = end.saturating_sub(start);
    if copy > room {
        if !(rx.fs && rx.ls) {
            return dropped(t, "invalid length");
        }
        copy = t.rx.desc.pkt_size as usize;
    }
    if let Some(seg) = t.rx.seg.as_mut() {
        let from = start.min(data.len());
        let to = start.saturating_add(copy).min(data.len());
        seg.data.extend_from_slice(&data[from..to]);
        seg.room = seg.room.saturating_sub(copy);
    }
    if !t.rx.desc.ready {
        return dropped(t, "no rx desc information");
    }
    if !rx.ls {
        return RxResult::new(RxType::Partial, None);
    }
    let skb = t.rx.seg.take().map(|s| s.data).unwrap_or_default();
    let mut desc = t.rx.desc.clone();
    desc.copy_len = skb.len();
    desc.write_size = rx.len;
    t.rx.desc.ready = false;
    core_rx(t, desc, skb, ops)
}

// Linux: rtw89_core_rx (core.c:4527)
fn core_rx(t: &mut Txrx, desc: RxDesc, skb: Vec<u8>, ops: &mut dyn RxOps) -> RxResult {
    if desc.pkt_type != RTW89_CORE_RX_TYPE_WIFI {
        return process_report(t, desc, skb, ops);
    }
    let mut out = RxResult::new(RxType::Frame, Some(desc.clone()));
    if t.rx.ppdu_curr != desc.ppdu_cnt {
        flush_ppdu_queue(t, &mut out.deliver, Some(&desc), ops);
        t.rx.ppdu_curr = desc.ppdu_cnt;
    }
    let status = ppdu::update_rx_status(t, &desc);
    // rtw89_core_rx_pkt_hdl: only for stations of our AP mode (refcount_ap_info 0 here);
    // rtw89_core_skb_pn_valid: only with an RX BA session, which a non-HT station never has
    let waits = desc.long_rxdesc && (1u32 << (desc.frame_type & 31)) & PPDU_FILTER_BITMAP != 0;
    let mut entry = RxEntry {
        desc: desc.clone(),
        frame: skb,
        status,
        ppdu: None,
        ppdu_desc: None,
        ppdu_match: false,
    };
    if waits {
        out.pending = Some(entry.frame.len());
        if t.rx.queue.len() >= t.rx.ppdu_queue_bound {
            let flushed = t.rx.queue.len() as u32;
            flush_ppdu_queue(t, &mut out.deliver, Some(&desc), ops);
            t.events.push(RxEvent::PpduQueueFull { flushed });
        }
        t.rx.queue.push(entry);
    } else {
        out.frame = Some(entry.frame.clone());
        ppdu::to_mac80211(t, &mut entry, ops, None, Some(&desc));
        out.deliver.push(entry);
    }
    out
}

// Linux: rtw89_core_rx_process_report (core.c:4088)
fn process_report(t: &mut Txrx, desc: RxDesc, skb: Vec<u8>, ops: &mut dyn RxOps) -> RxResult {
    if desc.pkt_type == RTW89_CORE_RX_TYPE_C2H && skb.len() >= RTW89_C2H_HEADER_LEN as usize {
        let mut r = RxResult::new(RxType::C2h, Some(desc));
        r.c2h = c2h_parse(&skb).ok();
        r.frame = Some(skb);
        return r;
    }
    if desc.pkt_type == RTW89_CORE_RX_TYPE_PPDU_STAT {
        return ppdu::process_ppdu_sts(t, desc, skb, ops);
    }
    let mut r = RxResult::new(RxType::Other, Some(desc));
    r.frame = Some(skb);
    r
}

// Linux: rtw89_core_flush_ppdu_rx_queue (core.c:4477): queued frames go up without PPDU status
pub(super) fn flush_ppdu_queue(
    t: &mut Txrx,
    deliver: &mut Vec<RxEntry>,
    desc: Option<&RxDesc>,
    ops: &mut dyn RxOps,
) {
    for mut entry in core::mem::take(&mut t.rx.queue) {
        ppdu::to_mac80211(t, &mut entry, ops, None, desc);
        deliver.push(entry);
    }
}

/// What `rx_frames` collected from a list of buffers
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RxFrames {
    /// the frames mac80211 keeps (FCS good)
    pub frames: Vec<RxEntry>,
    pub c2h: Vec<C2h>,
    /// PPDU status packets
    pub ppdu: usize,
    /// dropped buffers and other reports
    pub other: Vec<RxResult>,
}

impl Txrx {
    // Linux: rtw89_core_flush_ppdu_rx_queue (core.c:4477) for a caller that stops polling:
    // the waiting frames go up without PPDU status
    pub fn rx_flush(&mut self, ops: &mut dyn RxOps) -> Vec<RxEntry> {
        let mut out = Vec::new();
        flush_ppdu_queue(self, &mut out, None, ops);
        out
    }

    // Linux: rtw89_pci_rxbd_deliver (pci.c:413) over `pci::rx_poll`'s buffers; keeps the frames
    // mac80211 would keep (FCS good) and collects the C2Hs
    pub fn rx_frames(&mut self, bufs: &[RxBuf], ops: &mut dyn RxOps) -> RxFrames {
        let mut out = RxFrames::default();
        for b in bufs {
            let mut r = rx_parse(self, RxIn::from(b), ops);
            let deliver = core::mem::take(&mut r.deliver);
            match r.typ {
                RxType::C2h => out.c2h.extend(r.c2h.take()),
                RxType::Ppdu => out.ppdu += 1,
                RxType::Frame | RxType::Partial => {}
                _ => out.other.push(r),
            }
            out.frames
                .extend(deliver.into_iter().filter(|e| !e.status.failed_fcs));
        }
        out
    }
}
