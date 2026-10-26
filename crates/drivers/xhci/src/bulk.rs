//! Bulk and interrupt transfers for class drivers (US-3; spec usb.md 3.5, 3.6; V1 `bulk.asm`, `bulkxfer.asm`;
//! Linux `xhci-ring.c`). Interrupt IN pipes are in `pipe.rs`.
//!
//! - **Queue**: any number of TDs per endpoint (R1); a ring that cannot take a TD grows by segments
//!   (`ProducerRing::fits`, `grow`). Each TD is cut at 64 KiB boundaries (`queue.rs`); its first TRB is handed
//!   over last.
//! - **Events** name a TRB of the TD at the head: a short packet anywhere ends the TD (AMD: Success with a
//!   residual counts as short, `classify_transfer`), Success on the last TRB completes it, an error ends it with
//!   the bytes moved so far; STALL, USB Transaction, Babble and Split Transaction errors also halt the endpoint
//!   (xHCI 4.10.2, Linux `xhci_requires_manual_halt_cleanup`), other errors leave it running. A later event for
//!   a TD already ended (spurious success after a short packet) is stray.
//! - **Cancel / timeout** (V1 "stop and skip"): Stop Endpoint; if the TD is the next the controller would run,
//!   Set TR Dequeue past it; otherwise its TRBs become No-Ops. Either way nothing stale stays queued and the TDs
//!   behind it keep their place. If the endpoint will not stop, the TD stays as a zombie that holds its buffer.
//!   A transfer whose result is already on the way keeps that result (`settle`).
//! - **Abort** ends what was queued when it fixed its skip target; TDs queued meanwhile (another thread, a
//!   completion callback) keep their place and run afterwards.
//! - **Reset to toggle 0** (`reset_endpoint`, after a STALL): Reset Endpoint when halted, else Stop Endpoint +
//!   Configure Endpoint dropping and adding the endpoint (only that resets the controller's toggle); the dequeue
//!   pointer goes to the first TD still wanted. Configure Endpoint forgets how far a TD had got, so a TD the stop
//!   caught after it had moved data ends there (`Cancelled` with those bytes) instead of starting over. The
//!   device side (CLEAR_FEATURE(ENDPOINT_HALT)) is `Device::clear_halt`.
//!
//! Commands never run under the `xhci.xfer` lock; results are queued under it (`Controller::defer`) and handed
//! on after every lock is dropped (`deliver_pending`).

use crate::ctx::{ep_context, ep_state};
use crate::hc::{Controller, Env, Source, State, FALLBACK_NS, SEGMENT_BYTES};
use crate::mem::DmaMem;
use crate::queue::{align_at_links, pieces, td_trbs, Owner, Td, TdState};
use crate::ring::ProducerRing;
use crate::slot::{SlotEntry, SlotOp};
use crate::trb::{cc, classify_transfer, Trb, TransferStatus};
use crate::xfer::{code_err, stray_event, Deferred, EpRing, PipeState, Xfer};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use usb_core::{Endpoint, EpKind, HcSlot, Request, Urb, UsbError, XferResult};

klog::counter!(TRANSFERS, "usb.xhci.transfers");
klog::counter!(TRANSFER_ERRORS, "usb.xhci.transfer_errors");
klog::counter!(SHORT_TRANSFERS, "usb.xhci.short_transfers");
klog::counter!(SUCCESS_RESIDUE, "usb.xhci.success_with_residue");
klog::counter!(CANCELS, "usb.xhci.cancels");
klog::counter!(TIMEOUTS, "usb.xhci.transfer_timeouts");
klog::counter!(CANCEL_STUCK, "usb.xhci.cancel_stuck");
klog::counter!(CANCEL_LATE, "usb.xhci.cancel_unmatched");
klog::counter!(TD_NOOPS, "usb.xhci.td_noops");
klog::counter!(EP_RESETS, "usb.xhci.ep_resets");
klog::counter!(EP_RESET_FAILURES, "usb.xhci.ep_reset_failures");
klog::counter!(RESET_ENDED, "usb.xhci.reset_ended_tds");
klog::counter!(ABORTS, "usb.xhci.aborts");
klog::counter!(RING_GROWS, "usb.xhci.ring_grows");
klog::counter!(SKIPPED_TDS, "usb.xhci.skipped_tds");

/// Device Context Index of an endpoint address (2 x number + IN).
pub fn dci_of(ep: u8) -> u8 {
    2 * (ep & 0x0F) + (ep >> 7)
}

/// Completion codes after which the endpoint is Halted (xHCI 4.10.2; Linux calls `xhci_handle_halted_endpoint`
/// for these). Any other error leaves it running. A doorbell on a halted endpoint is ignored by the controller,
/// so a halting code missing here would cost a wasted doorbell, while a running endpoint taken for halted would
/// leave its queue waiting.
pub(crate) fn halts(code: u8) -> bool {
    matches!(code, cc::STALL | cc::USB_TRANSACTION | cc::BABBLE | cc::SPLIT_TRANSACTION)
}

/// The ring of bulk, interrupt or isochronous endpoint `ep` (address), checked against what was configured.
pub(crate) fn ep_ring<M: DmaMem>(x: &mut Xfer<M>, ep: u8) -> Result<&mut EpRing<M>, UsbError> {
    let r = x.ep_ring(dci_of(ep)).ok_or(UsbError::Invalid("endpoint not configured"))?;
    match r.ep {
        Some(d) if d.address == ep && matches!(d.kind(), EpKind::Bulk | EpKind::Interrupt | EpKind::Isoch) => Ok(r),
        _ => Err(UsbError::Invalid("not a bulk, interrupt or isochronous endpoint of this configuration")),
    }
}

/// The TRBs (with their data lengths) of a TD that starts at the ring's enqueue pointer: the data in front of every
/// Link TRB it passes is whole packets (`align_at_links`); where a TRB is too short for that, the rest of the
/// segment is padded with Transfer No-Ops and the TD starts after the link.
fn layout<M: DmaMem>(ring: &ProducerRing<M>, p: &[(u64, u32)], mps: usize, dir_in: bool, kib: bool) -> Result<Vec<(Trb, u32)>, UsbError> {
    let per = ring.per_segment();
    let with_lens = |a: Vec<(u64, u32)>| td_trbs(&a, mps, dir_in, kib).into_iter().zip(a.iter().map(|x| x.1)).collect::<Vec<_>>();
    if let Some(a) = align_at_links(p, mps, ring.before_link(), per) {
        return Ok(with_lens(a));
    }
    let a = align_at_links(p, mps, per, per).ok_or(UsbError::Invalid("a piece too short to end in front of a ring link"))?;
    let mut v: Vec<(Trb, u32)> = (0..ring.before_link()).map(|_| (Trb::transfer_noop(), 0)).collect();
    v.extend(with_lens(a));
    Ok(v)
}

/// How a cancel ends a TD once its endpoint stood still (or did not).
enum Plan {
    Skip(u64, bool),
    Noop,
    Stuck,
}

/// What `ep_event` leaves for after the slot's lock is dropped.
#[derive(Default)]
pub(crate) struct After {
    pub(crate) stray: bool,
    pub(crate) kick: bool,
    pub(crate) doorbell: bool,
}

impl<E: Env> Controller<E> {
    /// The controller predates xHCI 1.0: TD Size counts KiB (`queue::td_trbs`).
    pub(crate) fn kib_td_size(&self) -> bool {
        self.caps.version < 0x100
    }

    /// Grows endpoint `ep`'s ring until a TD of `n` TRBs fits (`ProducerRing::fits`); the segments are allocated
    /// outside the lock. Every extra round means another context changed the ring meanwhile.
    pub(crate) fn grow_for(&self, e: &SlotEntry<E::Mem>, ep: u8, n: usize) -> Result<(), UsbError> {
        loop {
            let k = {
                let mut x = e.xfer.lock();
                let r = ep_ring(&mut x, ep)?;
                if r.ring.fits(n, r.oldest()) {
                    return Ok(());
                }
                r.ring.segments_for(n)
            };
            let mut segs = Vec::new();
            for _ in 0..k {
                segs.push(self.alloc_mem(SEGMENT_BYTES, 64)?);
            }
            let mut x = e.xfer.lock();
            if e.gone.load(Ordering::Acquire) {
                return Err(UsbError::Gone);
            }
            let r = ep_ring(&mut x, ep)?;
            let oldest = r.oldest();
            if r.ring.fits(n, oldest) {
                return Ok(());
            }
            let enough = segs.len() >= r.ring.segments_for(n);
            r.ring.grow(segs, oldest).map_err(|_| UsbError::Invalid("transfer ring"))?;
            RING_GROWS.inc();
            klog::event!(Debug, "usb", "xhci.ring_grow", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64),
                segments = r.ring.segments(), queued = r.tds.len());
            if enough && !r.ring.fits(n, oldest) {
                // Cannot happen with consistent bookkeeping; never loop on it.
                return Err(UsbError::Invalid("transfer ring"));
            }
        }
    }

    /// Queues a transfer (see `usb_core::Host::submit`).
    pub(crate) fn submit(&self, h: HcSlot, ep: u8, req: Request, urb: Arc<Urb>) -> Result<(), UsbError> {
        let xb = self.own_buffer(&req.buf)?;
        if xb.is_stuck() {
            return Err(UsbError::Busy);
        }
        if !req.in_bounds() {
            return Err(UsbError::Invalid("transfer range outside its buffer"));
        }
        let stuck = xb.stuck.clone();
        let p = pieces(&xb.segments(), req.offset, req.len).ok_or(UsbError::Invalid("transfer range outside its buffer"))?;
        let e = self.entry(h)?;
        let dci = dci_of(ep);
        let kib = self.kib_td_size();
        let mut owner = Some(Owner::Urb { urb, _buf: req.buf.clone(), stuck });
        // Another submit may take the room between growing and pushing: then it made progress, and this goes again.
        loop {
            let need = {
                let mut x = e.xfer.lock();
                if e.gone.load(Ordering::Acquire) {
                    return Err(UsbError::Gone);
                }
                let r = ep_ring(&mut x, ep)?;
                // A pipe owns the endpoint until it is stopped and its own TDs are off the ring.
                if r.pipe.is_some() {
                    return Err(UsbError::Busy);
                }
                if r.iso.is_some() {
                    return Err(UsbError::Invalid("an isochronous endpoint takes isochronous requests"));
                }
                let desc = r.ep.ok_or(UsbError::Invalid("endpoint not configured"))?;
                let lay = layout(&r.ring, &p, desc.max_packet() as usize, desc.is_in(), kib)?;
                if r.ring.fits(lay.len(), r.oldest()) {
                    let trbs: Vec<Trb> = lay.iter().map(|l| l.0).collect();
                    let (addrs, first_cycle) = r.ring.push_td(&trbs).map_err(|_| UsbError::Invalid("transfer ring"))?;
                    let td_trbs = addrs.into_iter().zip(lay.iter().map(|l| l.1)).collect();
                    let owner = owner.take().ok_or(UsbError::Invalid("owner"))?;
                    r.queue(Td { trbs: td_trbs, first_cycle, owner, state: TdState::Live, stopped_bytes: None, seq: 0 });
                    let now = !r.busy && !r.halted;
                    drop(x);
                    TRANSFERS.inc();
                    if now {
                        self.ring_doorbell(e.id, dci);
                    }
                    return Ok(());
                }
                lay.len()
            };
            self.grow_for(&e, ep, need)?;
        }
    }

    /// Sleeps until `urb` is complete, the device is gone or the controller stops, at most `timeout_ns`. Drains
    /// the event ring itself when no event thread answers (early boot).
    pub(crate) fn wait_urb(&self, h: HcSlot, urb: &Urb, timeout_ns: u64) -> bool {
        let start = self.now();
        let deadline = start.saturating_add(timeout_ns);
        loop {
            if urb.is_done() {
                return true;
            }
            if self.entry(h).is_err() {
                self.deliver_pending();
                return urb.is_done();
            }
            let now = self.now();
            if now >= deadline {
                self.process_events(Source::Waiter);
                return urb.is_done();
            }
            let mut c = || {
                if !self.thread_alive.load(Ordering::Acquire) || self.now().saturating_sub(start) >= FALLBACK_NS {
                    self.process_events(Source::Waiter);
                }
                urb.is_done() || self.state() != State::Running
            };
            self.env.wait(urb.wait_queue(), &mut c, (deadline - now).min(FALLBACK_NS));
        }
    }

    /// A URB the host no longer finds queued: its end is on the way from another context (an event popped its TD
    /// and queued the result) or was delivered already. Waits a bounded time for it, delivering what is queued,
    /// and ends it with `res` only if nothing came, so a transfer that really completed keeps its result.
    fn settle(&self, urb: &Urb, res: XferResult) {
        let deadline = self.now().saturating_add(FALLBACK_NS);
        loop {
            self.deliver_pending();
            let now = self.now();
            if urb.is_done() || now >= deadline {
                break;
            }
            self.env.wait(urb.wait_queue(), &mut || {
                self.deliver_pending();
                urb.is_done()
            }, deadline - now);
        }
        if urb.complete(res) {
            CANCEL_LATE.inc();
            klog::event!(Debug, "usb", "xhci.cancel_unmatched", ctrl = self.name.as_str(),
                error = res.error.map(|e| e.as_str()).unwrap_or(""); "no result came for a transfer not queued any more");
        }
    }

    /// A Transfer Event for a bulk or interrupt endpoint (event ring lock held). Results are queued under the
    /// slot's lock, so their order is the order of events.
    pub(crate) fn ep_event(&self, e: &Arc<SlotEntry<E::Mem>>, t: &Trb) {
        let mut x = e.xfer.lock();
        let mut out = Vec::new();
        let after = match x.ep_ring(t.endpoint()) {
            Some(r) => self.ep_event_locked(e, r, t, &mut out),
            None => After { stray: true, ..After::default() },
        };
        self.defer(out);
        drop(x);
        if after.stray {
            stray_event(t);
        }
        if after.doorbell {
            self.ring_doorbell(e.id, t.endpoint());
        }
        if after.kick {
            self.kick();
        }
    }

    fn ep_event_locked(&self, e: &SlotEntry<E::Mem>, r: &mut EpRing<E::Mem>, t: &Trb, out: &mut Vec<Deferred>) -> After {
        if r.iso.is_some() {
            return self.iso_event_locked(e, r, t, out);
        }
        let dci = t.endpoint();
        let code = t.code();
        let mut after = After::default();
        let Some((k, i)) = r.tds.iter().enumerate().find_map(|(k, td)| td.index_of(t.param).map(|i| (k, i))) else {
            after.stray = true;
            return after;
        };
        if matches!(code, cc::STOPPED | cc::STOPPED_LENGTH_INVALID | cc::STOPPED_SHORT_PACKET) {
            // Our own Stop Endpoint names the TRB it stood at: how far that TD got (a cancel reports it; a reset of
            // a running endpoint ends a TD that had begun). The TD itself goes on unless one of them ends it.
            let td = &mut r.tds[k];
            if td.state != TdState::Zombie {
                let residual = if code == cc::STOPPED_LENGTH_INVALID { td.trbs[i].1 } else { t.status & 0x00FF_FFFF };
                td.stopped_bytes = Some(td.moved(i, residual));
            }
            return after;
        }
        let status = classify_transfer(t);
        if status == TransferStatus::Complete && i + 1 != r.tds[k].trbs.len() {
            // Success on a TRB without IOC: nothing ends here (no TD of ours asks for it).
            after.stray = true;
            return after;
        }
        if code == cc::SUCCESS && matches!(status, TransferStatus::Short { .. }) {
            self.note_success_residue();
        }
        // The controller is past every TD before this one.
        let kib = self.kib_td_size();
        let mut rearm = Vec::new();
        for _ in 0..k {
            let Some(mut old) = r.tds.pop_front() else { break };
            r.ring.consumed(old.trbs.len());
            if old.state != TdState::Zombie {
                SKIPPED_TDS.inc();
                klog::event!(Warn, "usb", "xhci.td_skipped", ctrl = self.name.as_str(), slot = e.id, dci = dci;
                    "the controller reported a later TD first; the earlier one ends as a transfer error");
                old.state = TdState::Zombie;
                rearm.extend(Self::end_td(r, &old, XferResult::failed(UsbError::Transfer(code), 0), self.irq_stamp(), out));
            }
            old.set_stuck(false);
        }
        let Some(td) = r.tds.pop_front() else { return after };
        r.ring.consumed(td.trbs.len());
        td.set_stuck(false);
        if let TransferStatus::Error(c) = status
            && halts(c)
        {
            // The endpoint halted, whoever's TD it was; the TDs behind wait for a reset.
            r.halted = true;
        }
        if td.state == TdState::Zombie {
            after.kick = r.pipe.is_some() && matches!(status, TransferStatus::Error(_));
            return after;
        }
        let res = match status {
            TransferStatus::Complete => XferResult::ok(td.len()),
            TransferStatus::Short { residual } => {
                SHORT_TRANSFERS.inc();
                XferResult::ok(td.moved(i, residual))
            }
            TransferStatus::Error(c) => {
                TRANSFER_ERRORS.inc();
                klog::event!(Debug, "usb", "xhci.transfer_error", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = c,
                    halted = r.halted, moved = td.moved(i, t.status & 0x00FF_FFFF), queued = r.tds.len());
                XferResult::failed(code_err(c), td.moved(i, t.status & 0x00FF_FFFF))
            }
        };
        rearm.extend(Self::end_td(r, &td, res, self.irq_stamp(), out));
        if !rearm.is_empty() {
            let mut ok = false;
            for n in rearm {
                if Self::push_pipe_td(r, n, kib) {
                    ok = true;
                } else {
                    // No room without growing (not here, under the event lock): the worker grows and re-arms.
                    after.kick = true;
                }
            }
            after.doorbell = ok && !r.busy && !r.halted;
        } else if res.error.is_some() && r.pipe.is_some() {
            after.kick = true;
        }
        after
    }

    /// AMD's Success with a residual (Linux `XHCI_TRUST_TX_LENGTH`): classified as short for every controller
    /// (`classify_transfer`); counted, and named once per controller with whether its quirk expects it.
    pub(crate) fn note_success_residue(&self) {
        SUCCESS_RESIDUE.inc();
        if self.stats.success_residue.fetch_add(1, Ordering::Relaxed) == 0 {
            let q = self.quirks.trust_tx_length;
            if q {
                klog::event!(Info, "usb", "xhci.success_with_residue", ctrl = self.name.as_str(), quirk = q;
                    "short transfers end as Success with a residual (expected for this controller)");
            } else {
                klog::event!(Warn, "usb", "xhci.success_with_residue", ctrl = self.name.as_str(), quirk = q;
                    "short transfers end as Success with a residual although no quirk names it; treated as short");
            }
        }
    }

    /// Configure Endpoint that drops and adds endpoint `dci` with its ring continuing at `ptr`/`cycle` (resets the
    /// controller's data toggle; Linux `xhci_endpoint_reset`).
    fn configure_one(&self, e: &SlotEntry<E::Mem>, op: &SlotOp<E::Mem>, dci: u8, ep: &Endpoint, ptr: u64, cycle: bool) -> Result<(), UsbError> {
        let s = self.slot_from_output(op)?;
        let mut c = ep_context(e.speed, ep, ptr);
        c[2] = (ptr as u32 & !0xF) | cycle as u32;
        let ic = self.ctx_in(&op.inp);
        if !(op.inp.zero() && ic.set_flags(1 << dci, 1 | 1 << dci) && ic.set_all(0, &s) && ic.set_all(dci as usize, &c)) {
            return Err(UsbError::Invalid("context memory"));
        }
        self.cmd(Trb::configure_endpoint(op.inp.dev_addr(), e.id)).map(|_| ())
    }

    /// Data toggle back to 0 with the queue kept: Reset Endpoint + Set TR Dequeue after a halt, else Stop Endpoint
    /// (if running) + Configure Endpoint drop/add; the controller continues at the first TD still wanted. A TD the
    /// stop caught after it had moved data cannot go on after Configure Endpoint (the controller forgets its place
    /// inside it) and would repeat its first part: it ends with `Cancelled` and the bytes it moved. Results are
    /// queued; the caller delivers them once the operation lock is dropped.
    pub(crate) fn reset_queue(&self, e: &SlotEntry<E::Mem>, op: &SlotOp<E::Mem>, dci: u8) -> Result<(), UsbError> {
        let mut st = self.ep_state(op, dci);
        if st == ep_state::RUNNING {
            // Only this stop's position counts (an older stop's may be stale by now).
            if let Some(r) = e.xfer.lock().ep_ring(dci) {
                for t in r.tds.iter_mut().filter(|t| t.state == TdState::Live) {
                    t.stopped_bytes = None;
                }
            }
            if self.cmd(Trb::stop_endpoint(e.id, dci)).is_err() {
                // It may have halted in the meantime (an error raced the stop).
                st = self.ep_state(op, dci);
                if st == ep_state::RUNNING {
                    return Err(UsbError::Busy);
                }
            }
        }
        let st = if st == ep_state::RUNNING { self.ep_state(op, dci) } else { st };
        let (ptr, cycle, desc, ended) = {
            let mut x = e.xfer.lock();
            let r = x.ep_ring(dci).ok_or(UsbError::Invalid("endpoint not configured"))?;
            let mut ended = None;
            if st != ep_state::HALTED
                && let Some(td) = r.tds.iter_mut().find(|t| t.state != TdState::Zombie)
                && td.state == TdState::Live
                && let Some(b) = td.stopped_bytes.filter(|&b| b > 0)
            {
                td.state = TdState::Zombie;
                if let Owner::Urb { urb, .. } = &td.owner {
                    self.defer(alloc::vec![Deferred::Done(urb.clone(), XferResult::failed(UsbError::Cancelled, b))]);
                }
                ended = Some((b, td.is_pipe()));
            }
            let (p, c) = r.next_live();
            (p, c, r.ep.ok_or(UsbError::Invalid("endpoint not configured"))?, ended)
        };
        if let Some((moved, pipe)) = ended {
            RESET_ENDED.inc();
            klog::event!(Warn, "usb", "xhci.reset_ended_td", ctrl = self.name.as_str(), slot = e.id, dci = dci, moved = moved;
                "a toggle reset caught a transfer under way; it ends with what it moved");
            if pipe {
                // The pipe's buffer is armed again by the worker.
                self.kick();
            }
        }
        if st == ep_state::HALTED {
            self.cmd(Trb::reset_endpoint(e.id, dci))?;
            self.cmd(Trb::set_tr_dequeue(e.id, dci, ptr, cycle))?;
        } else {
            self.configure_one(e, op, dci, &desc, ptr, cycle)?;
        }
        let mut x = e.xfer.lock();
        if let Some(r) = x.ep_ring(dci) {
            r.retire_leading();
            r.halted = false;
        }
        Ok(())
    }

    /// Ends the endpoint's `busy` phase; rings its doorbell if something is still queued.
    pub(crate) fn unbusy(&self, e: &SlotEntry<E::Mem>, dci: u8) {
        let ring = {
            let mut x = e.xfer.lock();
            match x.ep_ring(dci) {
                Some(r) => {
                    r.busy = false;
                    r.has_live() && !r.halted
                }
                None => false,
            }
        };
        if ring {
            self.ring_doorbell(e.id, dci);
        }
    }

    pub(crate) fn set_busy(&self, e: &SlotEntry<E::Mem>, ep: u8) -> Result<(), UsbError> {
        let mut x = e.xfer.lock();
        ep_ring(&mut x, ep)?.busy = true;
        Ok(())
    }

    /// `usb_core::Host::reset_endpoint`.
    pub(crate) fn reset_endpoint(&self, h: HcSlot, ep: u8) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let dci = dci_of(ep);
        if ep_ring(&mut e.xfer.lock(), ep)?.iso.is_some() {
            // No data toggle on isochronous endpoints; `abort` stops and clears them.
            return Err(UsbError::Invalid("isochronous endpoints have no halt to clear"));
        }
        self.set_busy(&e, ep)?;
        let r = self.reset_queue(&e, &op, dci);
        drop(op);
        self.unbusy(&e, dci);
        self.deliver_pending();
        match r {
            Ok(()) => {
                EP_RESETS.inc();
                klog::event!(Debug, "usb", "xhci.ep_reset", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64));
            }
            Err(err) => {
                EP_RESET_FAILURES.inc();
                klog::event!(Warn, "usb", "xhci.ep_reset_failed", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64),
                    error = err.as_str());
            }
        }
        r
    }

    /// Takes one URB off its endpoint (`usb_core::Host::cancel`).
    pub(crate) fn cancel_urb(&self, h: HcSlot, ep: u8, urb: &Arc<Urb>, why: UsbError) {
        if urb.is_done() {
            return;
        }
        let e = match self.entry(h) {
            Ok(e) => e,
            Err(err) => return self.settle(urb, XferResult::failed(err, 0)),
        };
        let op = match self.lock_op(&e) {
            Ok(op) => op,
            Err(err) => return self.settle(urb, XferResult::failed(err, 0)),
        };
        let dci = dci_of(ep);
        let found = {
            let mut x = e.xfer.lock();
            match x.ep_ring(dci) {
                Some(r) => match r.tds.iter_mut().find(|t| t.is_urb(urb) && t.state == TdState::Live) {
                    Some(td) => {
                        td.state = TdState::Cancelling;
                        r.busy = true;
                        true
                    }
                    None => false,
                },
                None => false,
            }
        };
        if !found {
            drop(op);
            // Ended by an event in the meantime (its result may still be on the way), or never queued here.
            return self.settle(urb, XferResult::failed(why, 0));
        }
        CANCELS.inc();
        let mut st = self.ep_state(&op, dci);
        if st == ep_state::RUNNING {
            let _ = self.cmd(Trb::stop_endpoint(e.id, dci));
            st = self.ep_state(&op, dci);
        }
        let plan = {
            let mut x = e.xfer.lock();
            x.ep_ring(dci).and_then(|r| {
                let k = r.tds.iter().position(|t| t.is_urb(urb) && t.state == TdState::Cancelling)?;
                let at_head = r.tds.iter().take(k).all(|t| t.state == TdState::Zombie);
                Some(match st {
                    ep_state::STOPPED | ep_state::ERROR if at_head => {
                        let next = r.tds.iter().skip(k + 1).find(|t| t.state != TdState::Zombie).map(|t| t.first());
                        let (p, c) = next.unwrap_or_else(|| r.ring.enqueue_ptr());
                        Plan::Skip(p, c)
                    }
                    ep_state::RUNNING => Plan::Stuck,
                    _ => Plan::Noop,
                })
            })
        };
        let mut plan = match plan {
            Some(p) => p,
            None => {
                // An event ended it while the endpoint was being stopped: its result is queued.
                drop(op);
                self.unbusy(&e, dci);
                return self.settle(urb, XferResult::failed(why, 0));
            }
        };
        if let Plan::Skip(p, c) = plan
            && self.cmd(Trb::set_tr_dequeue(e.id, dci, p, c)).is_err()
        {
            plan = Plan::Noop;
        }
        drop(op);
        let res = {
            let mut x = e.xfer.lock();
            let Some(r) = x.ep_ring(dci) else {
                drop(x);
                return self.settle(urb, XferResult::failed(why, 0));
            };
            let Some(td) = r.tds.iter_mut().find(|t| t.is_urb(urb) && t.state == TdState::Cancelling) else {
                drop(x);
                self.unbusy(&e, dci);
                return self.settle(urb, XferResult::failed(why, 0));
            };
            td.state = TdState::Zombie;
            let res = XferResult::failed(why, td.stopped_bytes.unwrap_or(0));
            match plan {
                Plan::Skip(..) => r.retire_leading(),
                Plan::Noop => {
                    let addrs: Vec<u64> = td.trbs.iter().map(|t| t.0).collect();
                    for a in addrs {
                        r.ring.make_noop(a);
                    }
                    TD_NOOPS.inc();
                }
                Plan::Stuck => {
                    td.set_stuck(true);
                    CANCEL_STUCK.inc();
                }
            }
            res
        };
        self.unbusy(&e, dci);
        if matches!(plan, Plan::Stuck) {
            klog::event!(Error, "usb", "xhci.cancel_stuck", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64);
                "the endpoint did not stop; the transfer's buffer stays reserved until the controller is past it");
        }
        if why == UsbError::Timeout {
            TIMEOUTS.inc();
            klog::event!(Warn, "usb", "xhci.transfer_timeout", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64),
                moved = res.actual, how = match plan { Plan::Skip(..) => "skipped", Plan::Noop => "noop", Plan::Stuck => "stuck" });
        }
        urb.complete(res);
    }

    /// Cancels what is queued on endpoint `dci` with `why` and moves its dequeue pointer to the enqueue pointer as
    /// it stood then (V1: "dequeue = next free TRB"). A halted endpoint gets Reset Endpoint first (its toggle goes
    /// to 0). TDs queued while the commands run (`submit` queues on a busy endpoint without a doorbell) lie at or
    /// behind that pointer: they keep their buffers and their place and run after the caller's `unbusy`. Returns
    /// how many transfers ended; the caller delivers the results once the operation lock is dropped.
    pub(crate) fn abort_locked(&self, e: &SlotEntry<E::Mem>, op: &SlotOp<E::Mem>, dci: u8, why: UsbError) -> usize {
        // A pipe stops re-arming before the endpoint is stopped, so nothing of it lands behind the skip.
        if let Some(p) = e.xfer.lock().ep_ring(dci).and_then(|r| r.pipe.as_mut()).filter(|p| p.state != PipeState::Stopped) {
            p.state = PipeState::Stopped;
            self.defer(alloc::vec![Deferred::Stopped(p.sink.clone(), why)]);
        }
        let mut st = self.ep_state(op, dci);
        if st == ep_state::RUNNING {
            let _ = self.cmd(Trb::stop_endpoint(e.id, dci));
            st = self.ep_state(op, dci);
        }
        if st == ep_state::HALTED && self.cmd(Trb::reset_endpoint(e.id, dci)).is_ok() {
            st = self.ep_state(op, dci);
        }
        // The skip target and the TDs in front of it, read together (by sequence number: events may still take
        // TDs off the front of an endpoint that did not stop).
        let Some((target, limit)) = e.xfer.lock().ep_ring(dci).map(|r| (r.ring.enqueue_ptr(), r.next_seq)) else { return 0 };
        let skipped = matches!(st, ep_state::STOPPED | ep_state::ERROR)
            && self.cmd(Trb::set_tr_dequeue(e.id, dci, target.0, target.1)).is_ok();
        let n = {
            let mut x = e.xfer.lock();
            let Some(r) = x.ep_ring(dci) else { return 0 };
            let cut = r.tds.iter().take_while(|t| t.seq < limit).count();
            let n = r.tds.iter().take(cut).filter(|t| t.state != TdState::Zombie && !t.is_pipe()).count();
            let mut out = Vec::new();
            r.end_first(cut, why, &mut out);
            self.defer(out);
            if skipped {
                let trbs: usize = r.tds.iter().take(cut).map(|t| t.trbs.len()).sum();
                for t in r.tds.drain(..cut) {
                    t.set_stuck(false);
                }
                r.ring.consumed(trbs);
                r.halted = false;
                // Nothing of a stopped pipe is left on the ring: the endpoint is free for transfers or a new pipe.
                if !r.tds.iter().any(|t| t.is_pipe()) {
                    r.pipe = None;
                }
            } else {
                // Not stopped: the TDs stay as zombies holding their buffers.
                for t in r.tds.iter().take(cut) {
                    t.set_stuck(true);
                }
            }
            n
        };
        if !skipped {
            CANCEL_STUCK.inc();
            klog::event!(Error, "usb", "xhci.abort_stuck", ctrl = self.name.as_str(), slot = e.id, dci = dci, ep_state = st;
                "the endpoint did not stop; its buffers stay reserved");
        }
        n
    }

    /// `usb_core::Host::abort_endpoint`.
    pub(crate) fn abort_endpoint(&self, h: HcSlot, ep: u8) -> Result<usize, UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        self.set_busy(&e, ep)?;
        let dci = dci_of(ep);
        let n = self.abort_locked(&e, &op, dci, UsbError::Cancelled);
        drop(op);
        self.unbusy(&e, dci);
        self.deliver_pending();
        ABORTS.inc();
        klog::event!(Debug, "usb", "xhci.ep_abort", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64), cancelled = n);
        Ok(n)
    }
}
