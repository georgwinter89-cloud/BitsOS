//! Endpoint 0 control transfers (Setup, Data with ISP, Status with IOC; V1 `usbctl.asm`) and the per-slot
//! transfer state shared with the bulk/interrupt queues of `bulk.rs` (US-3). A control transfer that fails or
//! times out leaves endpoint 0 restarted (Reset Endpoint if it halted, Stop Endpoint if it still runs, then Set
//! TR Dequeue past everything queued; V1 `xhci_ep_restart`), so the next transfer works. Transfer events are
//! matched by TRB address; older ones are counted as stray.

use crate::ctx::ep_state;
use crate::hc::{Controller, Env, Source, State, FALLBACK_NS, MS};
use crate::mem::DmaMem;
use crate::queue::{Owner, Td, TdState};
use crate::ring::ProducerRing;
use crate::slot::{SlotEntry, SlotOp};
use crate::trb::{cc, classify_transfer, Trb, TransferStatus, EVENT_DATA, TRT_IN, TRT_NONE, TRT_OUT};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;
use crate::isoch::IsoEp;
use usb_core::{Data, Endpoint, EpKind, HcSlot, IsoResult, IsoUrb, PipeEvent, Setup, TimedSink, Urb, UsbError, XferResult};

klog::counter!(CONTROL_TRANSFERS, "usb.xhci.control_transfers");
klog::counter!(CONTROL_FAILURES, "usb.xhci.control_failures");
klog::counter!(CONTROL_TIMEOUTS, "usb.xhci.control_timeouts");
klog::counter!(EP_RESTARTS, "usb.xhci.endpoint_restarts");
klog::counter!(EP_RESTART_FAILURES, "usb.xhci.endpoint_restart_failures");
klog::counter!(EP0_STUCK, "usb.xhci.ep0_stuck");
klog::counter!(STRAY_TRANSFERS, "usb.xhci.stray_transfer_events");

/// A control transfer waits at most this long: USB 2.0 §9.2.6.4 gives a device up to 5 s for a request with a
/// data stage, and Linux waits that long (`USB_CTRL_GET_TIMEOUT`, its initial descriptor read included). V1's
/// 500 ms cut slow sticks off and sent them through a whole port reset.
pub const CONTROL_NS: u64 = 5000 * MS;

pub(crate) struct CtlTd {
    setup: u64,
    data: Option<u64>,
    status: u64,
    trbs: usize,
    residual: u32,
    done: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipeState {
    Armed,
    /// The endpoint halted on an error: the hotplug worker restarts it and arms again.
    Repair,
    /// Given up or stopped: no new TDs; the worker clears what is still queued.
    Stopped,
}

/// An interrupt IN pipe: `bufs.len()` transfers of `len` bytes kept queued (one TD per buffer).
pub(crate) struct Pipe<M: DmaMem> {
    pub(crate) bufs: Vec<M>,
    pub(crate) len: usize,
    pub(crate) sink: TimedSink,
    pub(crate) errors: u32,
    pub(crate) state: PipeState,
    pub(crate) last_code: u8,
    /// The worker tried to take a stopped pipe's TDs off once (a stuck endpoint is not retried every round; the
    /// pipe and its buffers stay while its TDs might still be used).
    pub(crate) cleanup_tried: bool,
}

/// One endpoint's ring and what is queued on it.
pub(crate) struct EpRing<M: DmaMem> {
    pub(crate) ring: ProducerRing<M>,
    pub(crate) ep: Option<Endpoint>,
    pub(crate) pipe: Option<Pipe<M>>,
    /// TDs in ring order (bulk/interrupt endpoints; endpoint 0 uses `Xfer::ctl`).
    pub(crate) tds: VecDeque<Td>,
    /// A cancel, reset or abort is working on the endpoint: `submit` queues but leaves the doorbell to it.
    pub(crate) busy: bool,
    /// An error halted the endpoint; queued TDs wait for a reset.
    pub(crate) halted: bool,
    /// Sequence number of the next TD queued.
    pub(crate) next_seq: u64,
    /// An isochronous endpoint's request bookkeeping (`isoch.rs`).
    pub(crate) iso: Option<IsoEp>,
}

impl<M: DmaMem> EpRing<M> {
    fn new(ring: ProducerRing<M>, ep: Option<Endpoint>) -> Self {
        let iso = ep.filter(|d| d.kind() == EpKind::Isoch).map(|_| IsoEp::default());
        EpRing { ring, ep, pipe: None, tds: VecDeque::new(), busy: false, halted: false, next_seq: 0, iso }
    }
    /// Appends a TD pushed onto the ring (in ring order) with the next sequence number.
    pub(crate) fn queue(&mut self, mut td: Td) {
        td.seq = self.next_seq;
        self.next_seq += 1;
        self.tds.push_back(td);
    }
    /// The first TD the controller still has to run (zombies before it are skipped), else the enqueue pointer.
    pub(crate) fn next_live(&self) -> (u64, bool) {
        self.tds.iter().find(|t| t.state != TdState::Zombie).map(|t| t.first()).unwrap_or_else(|| self.ring.enqueue_ptr())
    }
    pub(crate) fn has_live(&self) -> bool {
        self.tds.iter().any(|t| t.state != TdState::Zombie)
    }
    /// The oldest TRB the controller may still have to run (`ProducerRing::fits`, `grow`).
    pub(crate) fn oldest(&self) -> Option<u64> {
        self.tds.front().and_then(|t| t.trbs.first()).map(|t| t.0)
    }
    /// The controller's dequeue pointer now stands at `next_live()`: every TD before the first live one is off
    /// the ring (its TRBs free again).
    pub(crate) fn retire_leading(&mut self) {
        while self.tds.front().is_some_and(|t| t.state == TdState::Zombie) {
            let t = self.tds.pop_front().unwrap();
            self.ring.consumed(t.trbs.len());
            t.set_stuck(false);
        }
    }
    /// Every live TD ends with `why` and stays on the ring as a zombie (memory kept until the ring is past it or
    /// the slot is gone); pipes stop. Returns what to deliver.
    pub(crate) fn end_all(&mut self, why: UsbError, out: &mut Vec<Deferred>) {
        self.end_first(self.tds.len(), why, out);
    }
    /// `end_all` for the first `cut` TDs only (an abort ends what was queued when it fixed its skip target).
    pub(crate) fn end_first(&mut self, cut: usize, why: UsbError, out: &mut Vec<Deferred>) {
        for t in self.tds.iter_mut().take(cut).filter(|t| t.state != TdState::Zombie) {
            t.state = TdState::Zombie;
            let res = XferResult::failed(why, t.stopped_bytes.unwrap_or(0));
            match &t.owner {
                Owner::Urb { urb, .. } => out.push(Deferred::Done(urb.clone(), res)),
                Owner::Iso { .. } => {
                    if let Some(iso) = self.iso.as_mut() {
                        iso.end(t, res, Some(why), out);
                    }
                }
                Owner::Pipe(_) => {}
            }
        }
        if let Some(p) = self.pipe.as_mut().filter(|p| p.state != PipeState::Stopped) {
            p.state = PipeState::Stopped;
            out.push(Deferred::Stopped(p.sink.clone(), why));
        }
    }
}

/// The rings of a slot by DCI (an xHCI device has 31 endpoint contexts; the table is the hardware's, not a limit).
pub(crate) struct Xfer<M: DmaMem> {
    pub(crate) eps: Vec<Option<EpRing<M>>>,
    pub(crate) ctl: Option<CtlTd>,
}

/// A pipe event or a transfer's end, handed on once every host lock is dropped. Data and Error carry the time of
/// the interrupt whose drain found them (`Controller::irq_stamp`); Stopped is stamped when it is handed on.
pub(crate) enum Deferred {
    Data(TimedSink, Vec<u8>, u64),
    Error(TimedSink, UsbError, u64),
    Stopped(TimedSink, UsbError),
    Done(Arc<Urb>, XferResult),
    /// An isochronous request with every packet's result.
    Iso(Arc<IsoUrb>, IsoResult),
}

fn deliver_one(x: Deferred, now: &dyn Fn() -> u64) {
    match x {
        Deferred::Data(s, v, at) => s(PipeEvent::Data(&v), at),
        Deferred::Error(s, e, at) => s(PipeEvent::Error(e), at),
        Deferred::Stopped(s, e) => s(PipeEvent::Stopped(e), now()),
        Deferred::Done(u, r) => {
            u.complete(r);
        }
        Deferred::Iso(u, r) => {
            u.complete(r);
        }
    }
}

/// One delivery queue and its one delivering context at a time. A controller has two (US-ev, B4): pipe events
/// (input), handed on by the KernelRt event thread, and transfer ends (storage callbacks), handed on by the
/// Normal completion thread or by the waiter that needs them.
pub(crate) struct Lane {
    q: SpinLock<VecDeque<Deferred>>,
    pub(crate) busy: AtomicBool,
}

impl Lane {
    pub(crate) fn new(name: &'static str) -> Self {
        Lane { q: SpinLock::named(name, VecDeque::new()), busy: AtomicBool::new(false) }
    }
    fn extend(&self, items: Vec<Deferred>) {
        if !items.is_empty() {
            self.q.lock().extend(items);
        }
    }
    /// Something queued that nobody is handing on (a held queue lock counts as something: a push is under way).
    pub(crate) fn ready(&self) -> bool {
        !self.busy.load(Ordering::SeqCst) && self.q.try_lock().is_none_or(|q| !q.is_empty())
    }
    /// Hands queued items on, in queue order. If another context is delivering, it takes these too.
    pub(crate) fn deliver(&self, now: &dyn Fn() -> u64) {
        loop {
            if self.busy.swap(true, Ordering::SeqCst) {
                return;
            }
            loop {
                let next = self.q.lock().pop_front();
                let Some(d) = next else { break };
                deliver_one(d, now);
            }
            self.busy.store(false, Ordering::SeqCst);
            // Something queued after our last look while we still held the turn: its pusher saw us busy.
            if self.q.lock().is_empty() {
                return;
            }
        }
    }
}

/// A Transfer Event no transfer waits for (after a timeout, a stop, a spurious success after a short packet).
pub(crate) fn stray_event(t: &Trb) {
    STRAY_TRANSFERS.inc();
    klog::event!(Debug, "usb", "xhci.transfer_stray", slot = t.slot(), dci = t.endpoint(), code = t.code(), trb = klog::Hex(t.param));
}

pub(crate) fn code_err(code: u8) -> UsbError {
    match code {
        cc::STALL => UsbError::Stall,
        cc::BABBLE => UsbError::Babble,
        cc::USB_TRANSACTION => UsbError::Transaction,
        c => UsbError::Transfer(c),
    }
}

impl<M: DmaMem> Xfer<M> {
    pub(crate) fn new(ep0: ProducerRing<M>) -> Self {
        let mut eps: Vec<Option<EpRing<M>>> = (0..32).map(|_| None).collect();
        eps[1] = Some(EpRing::new(ep0, None));
        Xfer { eps, ctl: None }
    }
    /// DCIs of the configured endpoints (not endpoint 0).
    pub(crate) fn configured(&self) -> Vec<u8> {
        (2..32u8).filter(|&d| self.eps[d as usize].is_some()).collect()
    }
    /// Drops every endpoint but 0 (a new configuration replaced them; the controller no longer runs them):
    /// their transfers end with `Cancelled`, their pipes stop.
    pub(crate) fn drop_endpoints(&mut self, out: &mut Vec<Deferred>) {
        for e in self.eps.iter_mut().skip(2) {
            if let Some(mut r) = e.take() {
                r.end_all(UsbError::Invalid("endpoint dropped by a new configuration"), out);
            }
        }
    }
    /// One endpoint re-added with new settings (IN-2b interval): the old ring's transfers end, the new ring
    /// takes its place.
    pub(crate) fn replace_endpoint(&mut self, ep: Endpoint, ring: ProducerRing<M>, out: &mut Vec<Deferred>) {
        if let Some(mut r) = self.eps[ep.dci() as usize].take() {
            r.end_all(UsbError::Invalid("endpoint reconfigured"), out);
        }
        self.set_endpoint(ep, ring);
    }
    pub(crate) fn set_endpoint(&mut self, ep: Endpoint, ring: ProducerRing<M>) {
        self.eps[ep.dci() as usize] = Some(EpRing::new(ring, Some(ep)));
    }
    /// The device is gone (or the controller forgot it): every transfer ends with `why`, pipes stop. The TDs stay
    /// (their buffers alive) until the slot entry itself goes after Disable Slot.
    pub(crate) fn stop_all(&mut self, why: UsbError, out: &mut Vec<Deferred>) {
        for r in self.eps.iter_mut().skip(2).flatten() {
            r.end_all(why, out);
        }
    }
    /// Endpoint 0's ring position (pointer, cycle) for an input context naming where the controller continues.
    pub(crate) fn ep0_dequeue(&self) -> Option<(u64, bool)> {
        self.eps.get(1)?.as_ref().map(|r| r.ring.enqueue_ptr())
    }
    pub(crate) fn ring(&mut self, dci: u8) -> Option<&mut ProducerRing<M>> {
        self.eps.get_mut(dci as usize)?.as_mut().map(|r| &mut r.ring)
    }
    pub(crate) fn ep_ring(&mut self, dci: u8) -> Option<&mut EpRing<M>> {
        self.eps.get_mut(dci as usize)?.as_mut()
    }
}

impl<E: Env> Controller<E> {
    /// Queues results for delivery. Called under the lock that decided them (the slot's `xhci.xfer`), so the
    /// order of each queue is the order things happened: a pipe's reports stay in order, nothing follows its
    /// `Stopped`, a callback that submits sees its own completion before a later one. Pipe events and transfer
    /// ends go to separate queues (`Lane`): input never waits behind a storage callback.
    pub(crate) fn defer(&self, items: Vec<Deferred>) {
        if items.is_empty() {
            return;
        }
        let (done, pipe): (Vec<_>, Vec<_>) = items.into_iter().partition(|d| matches!(d, Deferred::Done(..) | Deferred::Iso(..)));
        self.reports.extend(pipe);
        self.completions.extend(done);
    }

    /// Hands queued results on outside every host lock, each queue by one context at a time (sinks of a
    /// controller never run concurrently or out of order, nor do URB callbacks). `Thread`: the event thread
    /// delivers pipe events itself and wakes the completion thread. `Waiter` (anyone else): pipe events go to the
    /// event thread if it runs (input stays on the KernelRt path), transfer ends are delivered here (the waiter
    /// needs them now). Without a thread its queue is delivered by whoever comes.
    pub(crate) fn hand_on(&self, src: Source) {
        let now = || self.now();
        let events_thread = self.thread_alive.load(Ordering::Acquire);
        if src == Source::Thread || !events_thread {
            self.reports.deliver(&now);
        } else if self.reports.ready() {
            self.ev_wq.wake_one();
        }
        if src == Source::Thread && self.done_alive.load(Ordering::Acquire) {
            if self.completions.ready() {
                let _ = self.done_woke.compare_exchange(0, self.now(), Ordering::AcqRel, Ordering::Relaxed);
                self.done_wq.wake_one();
            }
        } else {
            self.completions.deliver(&now);
        }
    }

    /// What a context other than the event thread does after queueing results (`hand_on(Waiter)`).
    pub(crate) fn deliver_pending(&self) {
        self.hand_on(Source::Waiter);
    }

    /// One round of the completion thread: transfer ends to their URBs (usb-msd's callbacks run here, Normal).
    pub fn completion_round(&self) {
        let woke = self.done_woke.swap(0, Ordering::AcqRel);
        if woke != 0 {
            let lag = self.now().saturating_sub(woke);
            self.stats.done_wakes.fetch_add(1, Ordering::Relaxed);
            self.stats.done_lag_sum.fetch_add(lag, Ordering::Relaxed);
            self.stats.done_lag_max.fetch_max(lag, Ordering::Relaxed);
        }
        self.completions.deliver(&|| self.now());
        // The callbacks queued the next stages; a fast device has already answered them. Before the split the
        // event thread found those events itself on its way back to sleep; now it would wait for the poller.
        if self.event_pending() {
            self.interrupt();
        }
    }

    pub(crate) fn ring_doorbell(&self, slot: u8, dci: u8) {
        self.wr(self.caps.doorbell(slot), dci as u32);
    }

    /// Sleeps until `cond` holds for the slot's transfer state, the device is gone or the controller stops, at
    /// most `timeout_ns`. Drains the event ring itself when no event thread answers (early boot).
    fn wait_slot(&self, e: &SlotEntry<E::Mem>, timeout_ns: u64, cond: impl Fn(&Xfer<E::Mem>) -> bool) -> bool {
        let start = self.now();
        let deadline = start.saturating_add(timeout_ns);
        let over = || e.gone.load(Ordering::Acquire) || self.state() != State::Running;
        loop {
            if cond(&e.xfer.lock()) {
                return true;
            }
            if over() {
                return false;
            }
            let now = self.now();
            if now >= deadline {
                self.process_events(Source::Waiter);
                return cond(&e.xfer.lock());
            }
            let mut c = || {
                if !self.thread_alive.load(Ordering::Acquire) || self.now().saturating_sub(start) >= FALLBACK_NS {
                    self.process_events(Source::Waiter);
                }
                cond(&e.xfer.lock()) || over()
            };
            self.env.wait(&e.wq, &mut c, (deadline - now).min(FALLBACK_NS));
        }
    }

    /// A control transfer on endpoint 0 (see the module doc).
    pub(crate) fn control_transfer(&self, h: HcSlot, setup: Setup, mut data: Data<'_>) -> Result<usize, UsbError> {
        let len = setup.length as usize;
        let shape_ok = match &data {
            Data::None => len == 0,
            Data::In(b) => setup.is_in() && b.len() >= len,
            Data::Out(b) => !setup.is_in() && b.len() == len,
        };
        if !shape_ok {
            return Err(UsbError::Invalid("data stage does not match the setup packet"));
        }
        let e = self.entry(h)?;
        let mut op = self.lock_op(&e)?;
        if len > 0 && op.data.as_ref().is_none_or(|d| d.size() < len) {
            op.data = Some(self.alloc_mem(len, 64)?);
        }
        let buf = op.data.as_ref().map(|d| d.dev_addr()).unwrap_or(0);
        if let (Data::Out(b), Some(d)) = (&data, op.data.as_ref())
            && !d.write_bytes(0, b)
        {
            return Err(UsbError::NoMemory);
        }
        let dir_in = setup.is_in();
        // TRBs of an earlier transfer whose endpoint restart failed still count as queued: one more restart
        // before giving up, so endpoint 0 recovers once the controller answers again.
        let full = e.xfer.lock().ring(1).is_some_and(|r| !r.has_room(3));
        if full && !self.restart_ep(&e, &op, 1) {
            EP0_STUCK.inc();
            klog::event!(Error, "usb", "xhci.ep0_stuck", ctrl = self.name.as_str(), slot = e.id;
                "endpoint 0 ring full and the endpoint does not restart; the device needs a replug");
            return Err(UsbError::Busy);
        }
        {
            let mut x = e.xfer.lock();
            if e.gone.load(Ordering::Acquire) {
                return Err(UsbError::Gone);
            }
            let ring = x.ring(1).ok_or(UsbError::Invalid("no endpoint 0 ring"))?;
            if !ring.has_room(3) {
                return Err(UsbError::Busy);
            }
            let trt = if len == 0 { TRT_NONE } else if dir_in { TRT_IN } else { TRT_OUT };
            let r = (|| {
                let s = ring.push(Trb::setup_stage(setup.to_u64(), trt))?;
                let d = if len > 0 { Some(ring.push(Trb::data_stage(buf, len as u32, dir_in))?) } else { None };
                let st = ring.push(Trb::status_stage(len == 0 || !dir_in))?;
                Ok::<_, crate::ring::RingError>((s, d, st))
            })();
            let (s, d, st) = r.map_err(|_| UsbError::Invalid("endpoint 0 ring"))?;
            x.ctl = Some(CtlTd { setup: s, data: d, status: st, trbs: 2 + d.is_some() as usize, residual: 0, done: None });
        }
        CONTROL_TRANSFERS.inc();
        self.ring_doorbell(e.id, 1);
        self.wait_slot(&e, CONTROL_NS, |x| x.ctl.as_ref().is_some_and(|t| t.done.is_some()));
        let td = {
            let mut x = e.xfer.lock();
            let td = x.ctl.take();
            if let Some(t) = td.as_ref().filter(|t| matches!(t.done, Some(cc::SUCCESS | cc::SHORT_PACKET)))
                && let Some(r) = x.ring(1)
            {
                r.consumed(t.trbs);
            }
            td
        };
        match td.as_ref().and_then(|t| t.done.map(|c| (c, t.residual))) {
            Some((cc::SUCCESS | cc::SHORT_PACKET, residual)) => {
                let n = len - (residual as usize).min(len);
                if let (Data::In(b), Some(d)) = (&mut data, op.data.as_ref())
                    && !d.read_bytes(0, &mut b[..n])
                {
                    return Err(UsbError::NoMemory);
                }
                Ok(n)
            }
            Some((code, _)) => {
                CONTROL_FAILURES.inc();
                let _ = self.restart_ep(&e, &op, 1);
                Err(code_err(code))
            }
            None if e.gone.load(Ordering::Acquire) => Err(UsbError::Gone),
            None if self.state() != State::Running => Err(UsbError::NotRunning),
            None => {
                CONTROL_TIMEOUTS.inc();
                klog::event!(Warn, "usb", "xhci.control_timeout", ctrl = self.name.as_str(), slot = e.id,
                    request = klog::Hex(setup.to_u64() & 0xFFFF), waited_ms = CONTROL_NS / MS);
                let _ = self.restart_ep(&e, &op, 1);
                Err(UsbError::Timeout)
            }
        }
    }

    /// Brings endpoint `dci` back after an error or timeout: Reset Endpoint (halted) or Stop Endpoint (running),
    /// then its dequeue pointer past everything queued. Needs the slot's operation lock. False if the controller
    /// did not confirm it: the queued TRBs then stay counted, and the next transfer on endpoint 0 tries again.
    pub(crate) fn restart_ep(&self, e: &SlotEntry<E::Mem>, op: &SlotOp<E::Mem>, dci: u8) -> bool {
        if dci == 1 {
            e.xfer.lock().ctl = None;
        }
        EP_RESTARTS.inc();
        let st = self.ep_state(op, dci);
        let r = match st {
            ep_state::HALTED => self.cmd(Trb::reset_endpoint(e.id, dci)).map(|_| ()),
            ep_state::RUNNING => self.cmd(Trb::stop_endpoint(e.id, dci)).map(|_| ()),
            _ => Ok(()),
        };
        let Some((ptr, cycle)) = e.xfer.lock().ring(dci).map(|r| r.enqueue_ptr()) else { return false };
        let r = r.and_then(|_| self.cmd(Trb::set_tr_dequeue(e.id, dci, ptr, cycle)).map(|_| ()));
        match r {
            Ok(()) => {
                if let Some(ring) = e.xfer.lock().ring(dci) {
                    ring.skipped();
                }
                true
            }
            Err(err) => {
                EP_RESTART_FAILURES.inc();
                klog::event!(Warn, "usb", "xhci.ep_restart_failed", ctrl = self.name.as_str(), slot = e.id, dci = dci,
                    ep_state = st, error = err.as_str());
                false
            }
        }
    }

    /// A Transfer Event, from the event ring (under its lock): completes the control transfer or the TD it
    /// names; results are queued for delivery after the locks are dropped.
    pub(crate) fn transfer_event(&self, t: &Trb) {
        let (id, dci, code) = (t.slot(), t.endpoint(), t.code());
        let stray = || stray_event(t);
        let Some(e) = self.slots.lock().get(id as usize).cloned().flatten() else { return stray() };
        if t.control & EVENT_DATA != 0 {
            return stray();
        }
        let mut x = e.xfer.lock();
        if dci == 1 {
            let Some(td) = x.ctl.as_mut() else { return stray() };
            let ptr = t.param;
            if td.data == Some(ptr) {
                match classify_transfer(t) {
                    TransferStatus::Complete => {}
                    TransferStatus::Short { residual } => td.residual = residual,
                    TransferStatus::Error(c) => td.done = Some(c),
                }
            } else if ptr == td.status || (ptr == td.setup && code != cc::SUCCESS) {
                td.done = Some(code);
            } else {
                drop(x);
                return stray();
            }
            let done = td.done.is_some();
            drop(x);
            if done {
                e.wq.wake_all();
            }
            return;
        }
        drop(x);
        self.ep_event(&e, t);
    }
}
