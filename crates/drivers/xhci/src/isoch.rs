//! Isochronous transfers (SN-8a; xHCI 1.2 4.11.2.5, 4.10.3, 6.4.1.3; Linux `xhci-ring.c` `xhci_queue_isoc_tx`,
//! `xhci_queue_isoc_tx_prepare`, `process_isoc_td`, `skip_isoc_td`, `handle_tx_event`).
//!
//! - **Queue**: a request's packets become one TD each, in order, all pushed under one hold of the slot's lock
//!   (an abort never cuts a request in half). The first TRB of each TD is an Isoch TRB: TBC/TLBPC as Linux
//!   (`xhci_get_burst_count`, `xhci_get_last_burst_packet_count`), Start Isoch ASAP, or the Frame ID when the
//!   driver asked for a bus frame, the controller has Contiguous Frame ID (HCCPARAMS1.CFC) and the frame lies
//!   in the window of xHCI 4.11.2.5 (`frame_window`; outside it SIA, as Linux). IOC on every TD; BEI on all but
//!   a request's last (xHCI 1.0+; with `Quirks::avoid_bei` an interrupt at least every 8th TD). Rings grow like
//!   bulk rings (R1). The data in front of a Link TRB is whole packets where the TD can be cut there
//!   (`align_at_links`), else the TD crosses the link as it is (Linux does not align isochronous TDs).
//! - **Events** are matched by TRB pointer. A TD the controller passed without an event (a missed service
//!   interval) ends as `Transfer(23)` with 0 bytes when a later TD's event comes (Linux `skip_isoc_td`); a
//!   Missed Service event that names a TD ends it and those before it the same way, one that names none waits
//!   for the next event. Ring Underrun (OUT) / Overrun (IN) only mean the ring ran empty: counted, the next
//!   request's doorbell starts the endpoint again. Short IN packets are no error; a TD that ended on an earlier
//!   TRB than its last remembers that last TRB, whose late event (Linux `last_td_was_short`) is then no stray.
//!   Other errors end the packet with what moved; isochronous endpoints run on after errors (CErr 0), but STALL, Babble, USB Transaction and
//!   Split Transaction may halt one: the hotplug worker then looks (`isoch_repair.rs`) and, if it is halted, does
//!   Reset Endpoint + Set TR Dequeue to the next live TD (Linux `xhci_requires_manual_halt_cleanup`), retried
//!   and given up after a bound.
//! - **Stop**: `abort` (Stop Endpoint, Set TR Dequeue to the enqueue pointer, every queued packet `Cancelled`),
//!   device gone (`Gone`), alternate setting changed (`Invalid`): requests end whole, with `IsoResult::error`.

use crate::bulk::{dci_of, ep_ring, halts, After};
use crate::ctx::max_esit_payload;
use crate::hc::{Controller, Env};
use crate::queue::{align_at_links, pieces, td_trbs, Owner, Td, TdState};
use crate::slot::SlotEntry;
use crate::trb::{cc, classify_transfer, Trb, TransferStatus, BEI};
use crate::xfer::{code_err, Deferred, EpRing};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use usb_core::{Endpoint, HcSlot, IsoRequest, IsoResult, IsoUrb, Speed, UsbError, XferResult};

klog::counter!(pub(crate) ISO_REQUESTS, "usb.xhci.iso_requests");
klog::counter!(pub(crate) ISO_PACKETS, "usb.xhci.iso_packets");
klog::counter!(pub(crate) ISO_MISSED, "usb.xhci.iso_missed");
klog::counter!(pub(crate) ISO_RING_EMPTY, "usb.xhci.iso_ring_empty");
klog::counter!(pub(crate) ISO_ERRORS, "usb.xhci.iso_errors");
klog::counter!(ISO_FRAME_ASAP, "usb.xhci.iso_frame_asap");

/// With `Quirks::avoid_bei`: an event interrupt at least every this many TDs (Linux `AVOID_BEI_INTERVAL_MIN`).
const AVOID_BEI_EVERY: usize = 8;
/// HCCPARAMS1 Contiguous Frame ID capability.
const HCC_CFC: u32 = 1 << 11;

/// A request whose packets are ending.
struct Partial {
    urb: Arc<IsoUrb>,
    got: Vec<Option<XferResult>>,
    whole: Option<UsbError>,
}

/// What an endpoint's isochronous state counts (ktests, host tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsoStats {
    pub packets: u64,
    pub missed: u64,
    pub ring_empty: u64,
    pub errors: u64,
    /// Transfer Events that matched no queued TD (the follow-up of a TD that ended early is not one).
    pub stray: u64,
}

/// An isochronous endpoint's bookkeeping in its `EpRing`.
#[derive(Default)]
pub(crate) struct IsoEp {
    partial: Vec<Partial>,
    /// A Missed Service event named no TD: TDs before the next event's one were missed.
    skip: bool,
    /// An error that may have halted the endpoint: the worker looks (`isoch_repair.rs`).
    pub(crate) check: bool,
    /// The completion code of that error.
    pub(crate) check_code: u8,
    /// Repairs that failed in a row (Reset Endpoint or Set TR Dequeue refused).
    pub(crate) repair_fails: u32,
    /// The repair was given up: the endpoint refuses requests until its alternate setting is set again.
    pub(crate) dead: Option<UsbError>,
    /// The last TRB of a TD that ended on an earlier TRB (short IN packet, error mid-TD): the controller may still
    /// report that TRB (Linux `last_td_was_short`); that event is no stray.
    tail: Option<u64>,
    pub(crate) stats: IsoStats,
}

impl IsoEp {
    /// Packet `idx` of the TD's request ended with `res` (`whole`: the request as a whole was ended). The request
    /// is handed on with its last packet.
    pub(crate) fn end(&mut self, td: &Td, res: XferResult, whole: Option<UsbError>, out: &mut Vec<Deferred>) {
        let Owner::Iso { urb, idx, .. } = &td.owner else { return };
        let Some(k) = self.partial.iter().position(|p| Arc::ptr_eq(&p.urb, urb)) else { return };
        let p = &mut self.partial[k];
        if let Some(slot) = p.got.get_mut(*idx) {
            *slot = Some(res);
        }
        p.whole = p.whole.or(whole);
        if p.got.iter().all(Option::is_some) {
            let p = self.partial.remove(k);
            out.push(Deferred::Iso(p.urb, IsoResult { packets: p.got.into_iter().flatten().collect(), error: p.whole }));
        }
    }
}

/// Transfer Burst Count and Transfer Last Burst Packet Count of a TD of `packets` packets (Linux
/// `xhci_get_burst_count`, `xhci_get_last_burst_packet_count`): 0/0 before xHCI 1.0; below SuperSpeed one
/// burst of all packets; SuperSpeed bursts of bMaxBurst+1.
pub fn burst_fields(speed: Speed, ep: &Endpoint, packets: usize, version: u16) -> (u32, u32) {
    if version < 0x100 {
        return (0, 0);
    }
    let packets = packets.max(1) as u32;
    if speed.is_superspeed() {
        let mb = ep.companion.map_or(0, |c| c.max_burst as u32) + 1;
        let tbc = packets.div_ceil(mb) - 1;
        let residue = packets % mb;
        return (tbc, if residue == 0 { mb - 1 } else { residue - 1 });
    }
    (0, packets - 1)
}

/// The frames software may name in a Frame ID now (xHCI 4.11.2.5, Linux `xhci_get_isoc_frame_id`): from
/// MFINDEX + IST + 1 microframe rounded up to a frame, to MFINDEX + 895 ms rounded down; both 11-bit frame
/// numbers. `ist`: HCSPARAMS2 bits 3:0 (bit 3 set: in frames).
pub fn frame_window(mfindex: u32, ist: u32) -> (u16, u16) {
    let ist = if ist & 8 != 0 { (ist & 7) << 3 } else { ist & 7 };
    let mf = mfindex & 0x3FFF;
    let start = (mf + ist + 1).div_ceil(8) * 8;
    let end = (mf + 895 * 8) / 8 * 8;
    (((start >> 3) & 0x7FF) as u16, ((end >> 3) & 0x7FF) as u16)
}

/// `frame` lies in the window (which may wrap at 2048).
pub fn frame_in_window(frame: u16, (start, end): (u16, u16)) -> bool {
    let f = frame & 0x7FF;
    match start.cmp(&end) {
        core::cmp::Ordering::Less => f >= start && f <= end,
        core::cmp::Ordering::Greater => f >= start || f <= end,
        core::cmp::Ordering::Equal => false,
    }
}

/// Whether TD `i` of `n` blocks its event interrupt (Linux `trb_block_event_intr`).
fn block_event(i: usize, n: usize, version: u16, avoid_bei: bool) -> bool {
    if version < 0x100 || i + 1 == n {
        return false;
    }
    !(avoid_bei && i > 0 && i % AVOID_BEI_EVERY == 0)
}

impl<E: Env> Controller<E> {
    /// The bus frame of every packet of a request that names one, or `None` per packet for ASAP.
    fn iso_frames(&self, speed: Speed, ep: &Endpoint, frame: Option<u16>, n: usize) -> Vec<Option<u16>> {
        let Some(f) = frame.filter(|_| self.rd(0x10) & HCC_CFC != 0) else { return vec![None; n] };
        let window = frame_window(self.rd(self.caps.rtsoff), self.rd(0x08) & 0xF);
        let step = 1u64 << (ep.interval.clamp(1, 16) - 1);
        let fs = matches!(speed, Speed::Low | Speed::Full);
        let mut asap = 0usize;
        let v = (0..n).map(|i| {
            // Modulo 2^64; only the low 11 bits (frames) count, so wrapping is harmless for any request length.
            let off = (i as u64).wrapping_mul(step);
            let at = if fs { (f as u64).wrapping_add(off) } else { ((f as u64) << 3).wrapping_add(off) >> 3 };
            let at = (at & 0x7FF) as u16;
            frame_in_window(at, window).then_some(at).or_else(|| {
                asap += 1;
                None
            })
        }).collect();
        if asap > 0 {
            ISO_FRAME_ASAP.inc();
            klog::event!(Debug, "usb", "xhci.iso_frame_asap", ctrl = self.name.as_str(), frame = f, window_start = window.0,
                window_end = window.1, packets = asap; "frame outside the scheduling window; those packets go out as soon as possible");
        }
        v
    }

    /// Queues an isochronous request (`usb_core::Host::submit_iso`).
    pub(crate) fn submit_iso(&self, h: HcSlot, ep: u8, req: IsoRequest, urb: Arc<IsoUrb>) -> Result<(), UsbError> {
        let n = req.packets.len();
        if n == 0 || n != urb.packets() || !req.in_bounds() {
            return Err(UsbError::Invalid("isochronous request without packets or outside its buffer"));
        }
        let xb = self.own_buffer(&req.buf)?;
        if xb.is_stuck() {
            return Err(UsbError::Busy);
        }
        let segs = xb.segments();
        let mut pcs = Vec::new();
        pcs.try_reserve(n).map_err(|_| UsbError::NoMemory)?;
        for p in &req.packets {
            pcs.push(pieces(&segs, p.offset, p.len).ok_or(UsbError::Invalid("isochronous packet outside its buffer"))?);
        }
        let e = self.entry(h)?;
        let desc = ep_ring(&mut e.xfer.lock(), ep)?.ep.ok_or(UsbError::Invalid("endpoint not configured"))?;
        let esit = max_esit_payload(e.speed, &desc) as usize;
        if req.packets.iter().any(|p| p.len > esit) {
            return Err(UsbError::Invalid("isochronous packet longer than the endpoint's Max ESIT Payload"));
        }
        let frames = self.iso_frames(e.speed, &desc, req.frame, n);
        let need: usize = pcs.iter().map(|p| p.len() + 1).sum();
        let mut partial = Some(Partial { urb: urb.clone(), got: vec![None; n], whole: None });
        loop {
            {
                let mut x = e.xfer.lock();
                if e.gone.load(Ordering::Acquire) {
                    return Err(UsbError::Gone);
                }
                let r = ep_ring(&mut x, ep)?;
                if r.iso.is_none() || r.ep != Some(desc) {
                    return Err(UsbError::Invalid("not an isochronous endpoint of the current alternate setting"));
                }
                if let Some(err) = r.iso.as_ref().and_then(|i| i.dead) {
                    return Err(err);
                }
                if r.ring.fits(need, r.oldest()) {
                    self.push_iso(r, &e.speed, &desc, &req, &urb, &pcs, &frames, xb.stuck.clone())?;
                    if let (Some(iso), Some(p)) = (r.iso.as_mut(), partial.take()) {
                        iso.partial.push(p);
                    }
                    let now = !r.busy && !r.halted;
                    drop(x);
                    ISO_REQUESTS.inc();
                    if now {
                        self.ring_doorbell(e.id, dci_of(ep));
                    }
                    return Ok(());
                }
            }
            self.grow_for(&e, ep, need)?;
        }
    }

    /// Pushes one TD per packet (the caller checked the room). If the ring refuses one anyway, the request's TDs
    /// already pushed become No-Op zombies and the request is refused.
    #[allow(clippy::too_many_arguments)]
    fn push_iso(&self, r: &mut EpRing<E::Mem>, speed: &Speed, desc: &Endpoint, req: &IsoRequest, urb: &Arc<IsoUrb>,
        pcs: &[Vec<(u64, u32)>], frames: &[Option<u16>], stuck: Arc<core::sync::atomic::AtomicBool>) -> Result<(), UsbError> {
        let (mps, dir_in, kib) = (desc.max_packet() as usize, desc.is_in(), self.kib_td_size());
        let (version, avoid) = (self.caps.version, self.quirks.avoid_bei);
        let n = pcs.len();
        for (i, pc) in pcs.iter().enumerate() {
            let lay = align_at_links(pc, mps, r.ring.before_link(), r.ring.per_segment()).unwrap_or_else(|| pc.clone());
            let mut trbs = td_trbs(&lay, mps, dir_in, kib);
            let (tbc, tlbpc) = burst_fields(*speed, desc, req.packets[i].len.div_ceil(mps.max(1)), version);
            trbs[0] = trbs[0].into_isoch(tbc, tlbpc, frames.get(i).copied().flatten());
            if block_event(i, n, version, avoid) {
                let last = trbs.len() - 1;
                trbs[last].control |= BEI;
            }
            match r.ring.push_td(&trbs) {
                Ok((addrs, first_cycle)) => {
                    let owner = Owner::Iso { urb: urb.clone(), idx: i, _buf: req.buf.clone(), stuck: stuck.clone() };
                    let trbs = addrs.into_iter().zip(lay.iter().map(|l| l.1)).collect();
                    r.queue(Td { trbs, first_cycle, owner, state: TdState::Live, stopped_bytes: None, seq: 0 });
                }
                Err(_) => {
                    for t in r.tds.iter_mut().rev().take(i) {
                        t.state = TdState::Zombie;
                        let addrs: Vec<u64> = t.trbs.iter().map(|x| x.0).collect();
                        for a in addrs {
                            r.ring.make_noop(a);
                        }
                    }
                    return Err(UsbError::Invalid("transfer ring"));
                }
            }
        }
        Ok(())
    }

    /// A Transfer Event on an isochronous endpoint (slot lock held; see the module doc).
    pub(crate) fn iso_event_locked(&self, e: &SlotEntry<E::Mem>, r: &mut EpRing<E::Mem>, t: &Trb, out: &mut Vec<Deferred>) -> After {
        let (code, dci) = (t.code(), t.endpoint());
        let mut after = After::default();
        if matches!(code, cc::RING_UNDERRUN | cc::RING_OVERRUN) {
            ISO_RING_EMPTY.inc();
            if let Some(iso) = r.iso.as_mut() {
                iso.stats.ring_empty += 1;
            }
            klog::event!(Debug, "usb", "xhci.iso_ring_empty", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = code,
                queued = r.tds.len());
            return after;
        }
        let found = r.tds.iter().enumerate().find_map(|(k, td)| td.index_of(t.param).map(|i| (k, i)));
        if matches!(code, cc::STOPPED | cc::STOPPED_LENGTH_INVALID | cc::STOPPED_SHORT_PACKET) {
            if let Some((k, i)) = found
                && r.tds[k].state != TdState::Zombie
            {
                let td = &mut r.tds[k];
                let residual = if code == cc::STOPPED_LENGTH_INVALID { td.trbs[i].1 } else { t.status & 0x00FF_FFFF };
                td.stopped_bytes = Some(td.moved(i, residual));
            }
            return after;
        }
        if code == cc::MISSED_SERVICE {
            match found {
                Some((k, _)) => self.iso_missed(e, r, k + 1, out),
                None => {
                    if let Some(iso) = r.iso.as_mut() {
                        iso.skip = true;
                    }
                }
            }
            return after;
        }
        let Some((k, i)) = found else {
            match r.iso.as_mut() {
                Some(iso) if iso.tail.is_some_and(|a| a == t.param) => {
                    iso.tail = None;
                    klog::event!(Debug, "usb", "xhci.iso_tail_event", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = code;
                        "event for the last TRB of a TD that already ended");
                }
                Some(iso) => {
                    iso.stats.stray += 1;
                    after.stray = true;
                }
                None => after.stray = true,
            }
            return after;
        };
        let status = classify_transfer(t);
        if status == TransferStatus::Complete && i + 1 != r.tds[k].trbs.len() {
            after.stray = true;
            return after;
        }
        if code == cc::SUCCESS && matches!(status, TransferStatus::Short { .. }) {
            self.note_success_residue();
        }
        self.iso_missed(e, r, k, out);
        let Some(td) = r.tds.pop_front() else { return after };
        r.ring.consumed(td.trbs.len());
        td.set_stuck(false);
        let Some(iso) = r.iso.as_mut() else { return after };
        iso.skip = false;
        iso.tail = (i + 1 != td.trbs.len()).then(|| td.trbs[td.trbs.len() - 1].0);
        if td.state == TdState::Zombie {
            return after;
        }
        let res = match status {
            TransferStatus::Complete => XferResult::ok(td.len()),
            TransferStatus::Short { residual } => XferResult::ok(td.moved(i, residual)),
            TransferStatus::Error(c) => {
                ISO_ERRORS.inc();
                iso.stats.errors += 1;
                let moved = td.moved(i, t.status & 0x00FF_FFFF);
                klog::event!(Debug, "usb", "xhci.iso_error", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = c, moved = moved);
                if halts(c) {
                    iso.check = true;
                    iso.check_code = c;
                    after.kick = true;
                }
                XferResult::failed(code_err(c), moved)
            }
        };
        ISO_PACKETS.inc();
        iso.stats.packets += 1;
        iso.end(&td, res, None, out);
        after
    }

    /// The first `k` TDs were passed by the controller without their own event: missed service intervals.
    fn iso_missed(&self, e: &SlotEntry<E::Mem>, r: &mut EpRing<E::Mem>, k: usize, out: &mut Vec<Deferred>) {
        let mut missed = 0u64;
        for _ in 0..k {
            let Some(td) = r.tds.pop_front() else { break };
            r.ring.consumed(td.trbs.len());
            td.set_stuck(false);
            if td.state != TdState::Zombie
                && let Some(iso) = r.iso.as_mut()
            {
                missed += 1;
                iso.stats.missed += 1;
                iso.end(&td, XferResult::failed(UsbError::Transfer(cc::MISSED_SERVICE), 0), None, out);
            }
        }
        if missed > 0 {
            ISO_MISSED.inc();
            klog::event!(Debug, "usb", "xhci.iso_missed", ctrl = self.name.as_str(), slot = e.id, dci = r.ep.map_or(0, |d| d.dci()),
                packets = missed);
        }
    }

    /// The isochronous counters of endpoint `ep` of a live device (tests).
    pub fn iso_stats(&self, h: HcSlot, ep: u8) -> Option<IsoStats> {
        let e = self.entry(h).ok()?;
        let mut x = e.xfer.lock();
        x.ep_ring(dci_of(ep))?.iso.as_ref().map(|i| i.stats)
    }

    /// TDs queued on endpoint `ep` (tests: nothing may stay after a stop).
    pub fn queued_tds(&self, h: HcSlot, ep: u8) -> Option<usize> {
        let e = self.entry(h).ok()?;
        let mut x = e.xfer.lock();
        x.ep_ring(dci_of(ep)).map(|r| r.tds.iter().filter(|t| t.state != TdState::Zombie).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usb_core::SsCompanion;

    fn ep(mps: u16, burst: Option<u8>) -> Endpoint {
        let companion = burst.map(|b| SsCompanion { max_burst: b, attributes: 0, bytes_per_interval: 0 });
        Endpoint { address: 0x02, attributes: 0x05, max_packet_raw: mps, interval: 1, companion }
    }

    #[test]
    fn burst_counts_as_linux() {
        // Full/high speed: one burst, TLBPC = packets - 1 (a zero-length packet still counts one).
        assert_eq!(burst_fields(Speed::Full, &ep(288, None), 1, 0x110), (0, 0));
        assert_eq!(burst_fields(Speed::High, &ep(1024, None), 3, 0x110), (0, 2));
        assert_eq!(burst_fields(Speed::Full, &ep(288, None), 0, 0x110), (0, 0));
        // SuperSpeed bMaxBurst 3 (4 per burst): 10 packets = 3 bursts, the last of 2.
        assert_eq!(burst_fields(Speed::Super, &ep(1024, Some(3)), 10, 0x110), (2, 1));
        assert_eq!(burst_fields(Speed::Super, &ep(1024, Some(3)), 8, 0x110), (1, 3), "last burst full");
        // Before xHCI 1.0 both fields are reserved.
        assert_eq!(burst_fields(Speed::High, &ep(1024, None), 3, 0x96), (0, 0));
    }

    #[test]
    fn frame_window_as_linux() {
        // MFINDEX 0x100 (frame 32, microframe 0), IST 2 microframes: start rounds up to frame 33, end 32 + 895.
        let w = frame_window(0x100, 2);
        assert_eq!(w, (33, 927));
        assert!(frame_in_window(33, w) && frame_in_window(927, w));
        assert!(!frame_in_window(32, w) && !frame_in_window(928, w));
        // IST in frames (bit 3): 1 frame = 8 microframes, microframe 3: (0x103 + 9) -> frame 34.
        assert_eq!(frame_window(0x103, 8 | 1).0, 34);
        // Near the 2048 wrap: start 2000, end wraps to 847.
        let w = frame_window(2000 * 8 - 3, 0);
        assert_eq!(w, (2000, (1999 + 895) % 2048));
        assert!(frame_in_window(2040, w) && frame_in_window(5, w) && !frame_in_window(1500, w));
        assert!(!frame_in_window(7, (7, 7)));
    }

    #[test]
    fn event_interrupt_blocking() {
        // xHCI 1.0+: only the request's last TD interrupts; Intel (avoid BEI): also every 8th.
        let v: Vec<bool> = (0..10).map(|i| block_event(i, 10, 0x110, false)).collect();
        assert_eq!(v, [true, true, true, true, true, true, true, true, true, false]);
        let v: Vec<bool> = (0..10).map(|i| block_event(i, 10, 0x110, true)).collect();
        assert_eq!(v, [true, true, true, true, true, true, true, true, false, false]);
        assert!(!block_event(0, 4, 0x96, false), "0.96: never");
    }
}
