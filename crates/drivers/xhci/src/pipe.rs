//! Interrupt IN pipes (US-3; spec usb.md 3.6; V1 `usbintr.asm`): `depth` TDs of one buffer each kept queued, each
//! buffer re-queued after its report went to the sink. The ring gets enough segments up front that re-queueing
//! never needs to grow it under the event lock; if it ever does not fit, the hotplug worker grows it and arms
//! the buffer again.
//!
//! Errors: after a halting error the worker resets the endpoint (toggle 0, queue kept), clears the device's
//! halt after a STALL and arms again; after an error that leaves the endpoint running (Missed Service and the
//! like) it only arms again. 50 errors in a row give the pipe up (V1 `XH_ERR_MAX`); a pipe given up or stopped
//! has what is still queued taken off.

use crate::bulk::ep_ring;
use crate::ctx::ep_state;
use crate::hc::{Controller, Env, SEGMENT_BYTES};
use crate::mem::DmaMem;
use crate::queue::{pieces, td_trbs, Owner, Td, TdState};
use crate::slot::SlotEntry;
use crate::trb::{cc, TRB_BYTES};
use crate::xfer::{Deferred, EpRing, Pipe, PipeState};
use usb_core::Endpoint;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use usb_core::{EpKind, HcSlot, PipeOpts, Setup, TimedSink, UsbError, XferResult};

klog::counter!(PIPE_REPORTS, "usb.xhci.pipe_reports");
klog::counter!(PIPE_ERRORS, "usb.xhci.pipe_errors");
klog::counter!(PIPE_REARMS, "usb.xhci.pipe_rearms");

/// Errors in a row after which an interrupt pipe is given up (V1 `XH_ERR_MAX`).
pub const PIPE_ERROR_LIMIT: u32 = 50;
/// Longest interrupt IN transfer of a pipe (one TRB: its buffer never crosses 64 KiB).
pub const PIPE_MAX_LEN: usize = 0x1_0000;

/// What the worker does for a pipe.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    /// An error ended a TD: reset a halted endpoint, then arm again.
    Repair,
    /// Given up or stopped: take what is still queued off.
    Cleanup,
    /// Buffers that could not be queued again without growing the ring.
    Rearm,
}

/// Buffers of the endpoint's pipe not queued at the moment.
fn missing<M: DmaMem>(r: &EpRing<M>) -> usize {
    let Some(p) = r.pipe.as_ref() else { return 0 };
    (0..p.bufs.len()).filter(|&b| !r.tds.iter().any(|t| t.state != TdState::Zombie && matches!(t.owner, Owner::Pipe(x) if x == b))).count()
}

impl<E: Env> Controller<E> {
    /// Hands a TD's end to its owner (a pipe's report or error stamped `at`, `irq_stamp`). For a pipe buffer that
    /// should go out again, returns its index.
    pub(crate) fn end_td(r: &mut EpRing<E::Mem>, td: &Td, res: XferResult, at: u64, out: &mut Vec<Deferred>) -> Option<usize> {
        match &td.owner {
            Owner::Urb { urb, .. } => {
                out.push(Deferred::Done(urb.clone(), res));
                None
            }
            Owner::Iso { .. } => {
                if let Some(iso) = r.iso.as_mut() {
                    iso.end(td, res, None, out);
                }
                None
            }
            Owner::Pipe(n) => {
                // A stopped pipe said its last word (`Stopped`): what still arrives is dropped.
                let p = r.pipe.as_mut().filter(|p| p.state != PipeState::Stopped)?;
                match res.error {
                    None => {
                        let mut v = vec![0u8; res.actual.min(p.len)];
                        if p.bufs.get(*n).is_some_and(|b| b.read_bytes(0, &mut v)) {
                            PIPE_REPORTS.inc();
                            p.errors = 0;
                            out.push(Deferred::Data(p.sink.clone(), v, at));
                        }
                        (p.state == PipeState::Armed).then_some(*n)
                    }
                    Some(err) => {
                        PIPE_ERRORS.inc();
                        p.errors += 1;
                        p.last_code = match err {
                            UsbError::Stall => cc::STALL,
                            UsbError::Babble => cc::BABBLE,
                            UsbError::Transaction => cc::USB_TRANSACTION,
                            UsbError::Transfer(c) => c,
                            _ => 0,
                        };
                        out.push(Deferred::Error(p.sink.clone(), err, at));
                        if p.errors >= PIPE_ERROR_LIMIT && p.state != PipeState::Stopped {
                            p.state = PipeState::Stopped;
                            out.push(Deferred::Stopped(p.sink.clone(), err));
                            klog::event!(Warn, "usb", "xhci.pipe_given_up", dci = r.ep.map(|d| d.dci()).unwrap_or(0),
                                code = p.last_code, errors = p.errors);
                        } else if p.state == PipeState::Armed {
                            p.state = PipeState::Repair;
                        }
                        None
                    }
                }
            }
        }
    }

    /// Queues buffer `n` of the endpoint's pipe as one TD. False if the ring cannot take it without growing.
    pub(crate) fn push_pipe_td(r: &mut EpRing<E::Mem>, n: usize, kib: bool) -> bool {
        let (Some(p), Some(d)) = (r.pipe.as_ref(), r.ep) else { return false };
        let Some(b) = p.bufs.get(n) else { return false };
        let Some(pc) = pieces(&[(b.dev_addr(), p.len)], 0, p.len) else { return false };
        let trbs = td_trbs(&pc, d.max_packet() as usize, true, kib);
        if !r.ring.fits(trbs.len(), r.oldest()) {
            return false;
        }
        match r.ring.push_td(&trbs) {
            Ok((addrs, first_cycle)) => {
                let trbs = addrs.into_iter().zip(pc.iter().map(|q| q.1)).collect();
                r.queue(Td { trbs, first_cycle, owner: Owner::Pipe(n), state: TdState::Live, stopped_bytes: None, seq: 0 });
                true
            }
            Err(_) => false,
        }
    }

    /// Queues every pipe buffer that is not queued; true if any went out.
    fn arm_missing(r: &mut EpRing<E::Mem>, kib: bool) -> bool {
        let Some(n) = r.pipe.as_ref().map(|p| p.bufs.len()) else { return false };
        let mut any = false;
        for b in 0..n {
            let queued = r.tds.iter().any(|t| t.state != TdState::Zombie && matches!(t.owner, Owner::Pipe(x) if x == b));
            if !queued {
                any |= Self::push_pipe_td(r, b, kib);
            }
        }
        any
    }

    /// Gives endpoint `ep`'s ring at least `want` segments (an empty ring is never in the one position where
    /// growing is unsafe, `ProducerRing::grow`).
    fn ensure_segments(&self, e: &SlotEntry<E::Mem>, ep: u8, want: usize) -> Result<(), UsbError> {
        let have = ep_ring(&mut e.xfer.lock(), ep)?.ring.segments();
        if have >= want {
            return Ok(());
        }
        let mut segs = Vec::with_capacity(want - have);
        for _ in have..want {
            segs.push(self.alloc_mem(SEGMENT_BYTES, 64)?);
        }
        let mut x = e.xfer.lock();
        let r = ep_ring(&mut x, ep)?;
        let oldest = r.oldest();
        r.ring.grow(segs, oldest).map_err(|_| UsbError::Invalid("transfer ring"))
    }

    /// Starts an interrupt IN pipe (`usb_core::Host::interrupt_in_at`; `interrupt_in` drops the stamp).
    pub(crate) fn start_pipe(&self, h: HcSlot, ep: u8, opts: PipeOpts, sink: TimedSink) -> Result<(), UsbError> {
        if ep & 0x80 == 0 || opts.len == 0 || opts.len > PIPE_MAX_LEN || opts.depth == 0 {
            return Err(UsbError::Invalid("interrupt pipes are IN, 1 byte to 64 KiB, at least one queued"));
        }
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        let mut bufs = Vec::new();
        bufs.try_reserve(opts.depth).map_err(|_| UsbError::NoMemory)?;
        for _ in 0..opts.depth {
            bufs.push(self.alloc_mem(opts.len, 64)?);
        }
        let desc = {
            let mut x = e.xfer.lock();
            let r = ep_ring(&mut x, ep)?;
            if r.ep.is_none_or(|d| d.kind() != EpKind::Interrupt) {
                return Err(UsbError::Invalid("not an interrupt endpoint"));
            }
            if r.pipe.is_some() || r.has_live() {
                return Err(UsbError::Busy);
            }
            r.ep
        };
        // IN-2b: a pipe that wants another interval than the configured one (1 ms gamepads) gets it by one
        // Configure Endpoint for this endpoint only; a refusal keeps the old one.
        if let (Some(b), Some(cur)) = (opts.interval, desc) {
            let want = Endpoint { interval: b, ..cur };
            if crate::ctx::interval(e.speed, &want) != crate::ctx::interval(e.speed, &cur)
                && let Err(err) = self.set_interval(&e, &op, want)
            {
                klog::event!(Warn, "usb", "xfer.interval_refused", ctrl = self.name.as_str(), slot = e.id, ep = klog::Hex(ep as u64),
                    want = b, have = cur.interval, error = err.as_str(); "the pipe starts with the descriptor's interval");
            }
        }
        // `depth` one-TRB TDs re-queued one by one never reach the oldest one's segment with this many.
        let per = SEGMENT_BYTES / TRB_BYTES - 1;
        self.ensure_segments(&e, ep, 1 + opts.depth.div_ceil(per))?;
        let kib = self.kib_td_size();
        let (now, short) = {
            let mut x = e.xfer.lock();
            if e.gone.load(Ordering::Acquire) {
                return Err(UsbError::Gone);
            }
            let r = ep_ring(&mut x, ep)?;
            r.pipe = Some(Pipe { bufs, len: opts.len, sink, errors: 0, state: PipeState::Armed, last_code: 0, cleanup_tried: false });
            let any = Self::arm_missing(r, kib);
            (any && !r.busy && !r.halted, missing(r) > 0)
        };
        if now {
            self.ring_doorbell(e.id, crate::bulk::dci_of(ep));
        }
        if short {
            self.kick();
        }
        Ok(())
    }

    /// `usb_core::Host::stop_pipe`. A handle whose pipe is gone already (given up and cleaned, or stopped before)
    /// stops nothing: transfers a driver queued on the endpoint since then stay.
    pub(crate) fn stop_pipe(&self, h: HcSlot, ep: u8) -> Result<(), UsbError> {
        let e = self.entry(h)?;
        let op = self.lock_op(&e)?;
        if ep_ring(&mut e.xfer.lock(), ep)?.pipe.is_none() {
            return Ok(());
        }
        self.set_busy(&e, ep)?;
        let dci = crate::bulk::dci_of(ep);
        self.abort_locked(&e, &op, dci, UsbError::Cancelled);
        drop(op);
        self.unbusy(&e, dci);
        self.deliver_pending();
        Ok(())
    }

    /// The hotplug worker's part (see the module doc).
    pub(crate) fn repair_pipes(&self) {
        self.repair_isoch();
        let entries: Vec<_> = self.slots.lock().iter().flatten().cloned().collect();
        for e in entries {
            let todo: Vec<(u8, u8, u8, Job)> = {
                let x = e.xfer.lock();
                x.eps.iter().enumerate().filter_map(|(d, r)| {
                    let r = r.as_ref()?;
                    let p = r.pipe.as_ref()?;
                    let job = match p.state {
                        PipeState::Repair => Job::Repair,
                        PipeState::Stopped if !p.cleanup_tried => Job::Cleanup,
                        PipeState::Armed if missing(r) > 0 => Job::Rearm,
                        _ => return None,
                    };
                    Some((d as u8, r.ep.map(|ep| ep.address).unwrap_or(0), p.last_code, job))
                }).collect()
            };
            for (dci, address, code, job) in todo {
                let Ok(op) = self.lock_op(&e) else { continue };
                match job {
                    Job::Cleanup => {
                        if let Some(r) = e.xfer.lock().ep_ring(dci) {
                            r.busy = true;
                            if let Some(p) = r.pipe.as_mut() {
                                p.cleanup_tried = true;
                            }
                        }
                        self.abort_locked(&e, &op, dci, UsbError::Cancelled);
                        drop(op);
                        self.unbusy(&e, dci);
                    }
                    Job::Rearm => {
                        let n = e.xfer.lock().ep_ring(dci).map(|r| missing(r)).unwrap_or(0);
                        let grown = self.grow_for(&e, address, n.max(1));
                        let any = {
                            let mut x = e.xfer.lock();
                            match x.ep_ring(dci) {
                                Some(r) if r.pipe.as_ref().is_some_and(|p| p.state == PipeState::Armed) => {
                                    Self::arm_missing(r, self.kib_td_size()) && !r.busy && !r.halted
                                }
                                _ => false,
                            }
                        };
                        drop(op);
                        PIPE_REARMS.inc();
                        klog::event!(Debug, "usb", "xhci.pipe_rearm", ctrl = self.name.as_str(), slot = e.id, dci = dci, buffers = n,
                            error = grown.err().map(|x| x.as_str()).unwrap_or(""));
                        if any {
                            self.ring_doorbell(e.id, dci);
                        }
                    }
                    Job::Repair => {
                        self.repair_one(&e, op, dci, address, code);
                    }
                }
                self.deliver_pending();
            }
        }
    }

    fn repair_one(&self, e: &SlotEntry<E::Mem>, op: ksync::MutexGuard<'_, crate::slot::SlotOp<E::Mem>>, dci: u8, address: u8, code: u8) {
        if let Some(r) = e.xfer.lock().ep_ring(dci) {
            r.busy = true;
        }
        if self.ep_state(&op, dci) != ep_state::HALTED {
            // The error left the endpoint running (or stopped): nothing to reset, the buffer goes out again.
            drop(op);
            {
                let mut x = e.xfer.lock();
                if let Some(r) = x.ep_ring(dci) {
                    r.halted = false;
                    if let Some(p) = r.pipe.as_mut().filter(|p| p.state == PipeState::Repair) {
                        p.state = PipeState::Armed;
                    }
                    if r.pipe.as_ref().is_some_and(|p| p.state == PipeState::Armed) {
                        Self::arm_missing(r, self.kib_td_size());
                    }
                }
            }
            self.unbusy(e, dci);
            if e.xfer.lock().ep_ring(dci).is_some_and(|r| missing(r) > 0) {
                self.kick();
            }
            klog::event!(Debug, "usb", "xhci.pipe_rearmed", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = code);
            return;
        }
        let reset = self.reset_queue(e, &op, dci);
        drop(op);
        if let Err(err) = reset {
            // Not restarted: one more error; tried again in a later round until the limit.
            let mut stopped = None;
            {
                let mut x = e.xfer.lock();
                if let Some(p) = x.ep_ring(dci).and_then(|r| r.pipe.as_mut()) {
                    p.errors += 1;
                    if p.errors >= PIPE_ERROR_LIMIT && p.state == PipeState::Repair {
                        p.state = PipeState::Stopped;
                        stopped = Some(p.sink.clone());
                        self.defer(vec![Deferred::Stopped(p.sink.clone(), UsbError::Transfer(code))]);
                    }
                }
            }
            self.unbusy(e, dci);
            match stopped {
                Some(_) => klog::event!(Warn, "usb", "xhci.pipe_given_up", ctrl = self.name.as_str(), slot = e.id, dci = dci,
                    code = code, errors = PIPE_ERROR_LIMIT, error = err.as_str(); "endpoint does not restart"),
                None => self.kick(),
            }
            return;
        }
        if code == cc::STALL {
            let _ = self.control_transfer(Self::handle_of(e), Setup::clear_halt(address), usb_core::Data::None);
        }
        {
            let mut x = e.xfer.lock();
            if let Some(r) = x.ep_ring(dci) {
                if let Some(p) = r.pipe.as_mut().filter(|p| p.state == PipeState::Repair) {
                    p.state = PipeState::Armed;
                }
                if r.pipe.as_ref().is_some_and(|p| p.state == PipeState::Armed) {
                    Self::arm_missing(r, self.kib_td_size());
                }
            }
        }
        self.unbusy(e, dci);
        klog::event!(Debug, "usb", "xhci.pipe_repaired", ctrl = self.name.as_str(), slot = e.id, dci = dci, code = code);
    }
}
