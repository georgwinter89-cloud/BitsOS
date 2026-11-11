//! BL-1: the I/O path over the queues of `mq` (Linux 7.x: drivers/nvme/host/pci.c `nvme_queue_rq` :1442,
//! `nvme_map_data` :1251, `nvme_pci_setup_data_prp` :1028, `nvme_timeout` :1886). A command is placed on the caller's
//! CPU's queue under its submission lock, then waited for without a lock: the interrupt hook (or the waiter itself,
//! polling) reaps it. Many commands of one call are in flight at once (`pipeline`), up to the queue's tags.
//!
//! Data goes straight to or from the caller's memory when the device can reach it in place (`Hw::dma_target`:
//! contiguous in the direct map, dword aligned); otherwise through bounce pages of the queue's pool (counted in
//! `block.copy_bytes.nvme_bounce`).
//!
//! A timeout first reaps (a missed interrupt), then sends Abort and waits once more; a command still out then
//! resets the controller (every queue; the other commands retry on the new queues) and is placed once more. A
//! second failure gives the controller up (NV-2p: stopped, Failed until a restart). Whatever happens, a command's
//! pages and the caller's memory are given back only once it completed or the device stopped after it was placed.

use crate::cmd::{self, Cqe, Encode, IoCmd};
use crate::ctrl::{COPY_NVME_BOUNCE, CmdError, Controller, MS, NVME_DIRECT, QUIESCE_RDY_MAX_NS, QUIESCE_SHUTDOWN_NS, SEC, SLICE_NS, State};
use crate::hw::{DmaMem, Hw};
use crate::mq::{IoQueue, IoSet};
use crate::prp;
use crate::regs::{self, Cap, PAGE};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use devmodel::block::IoBuf;

/// The pages a command is built with (taken from its queue's pool).
pub(crate) struct Bufs<'a, M: DmaMem> {
    q: &'a IoQueue<M>,
    alloc: &'a dyn Fn() -> Option<M>,
    pub pages: Vec<M>,
}

impl<M: DmaMem> Bufs<'_, M> {
    /// One more page; its index in `pages`.
    pub fn page(&mut self) -> Result<usize, CmdError> {
        let p = self.q.take_page(self.alloc).ok_or(CmdError::NotReady)?;
        self.pages.push(p);
        Ok(self.pages.len() - 1)
    }
}

/// A placed command.
pub(crate) struct Ticket<M: DmaMem> {
    set: Arc<IoSet<M>>,
    qi: usize,
    tag: u16,
    seq: u16,
    cid: u16,
    pub op: u8,
    /// `stop_gen` before the doorbell.
    snap: u64,
    pub pages: Vec<M>,
    deadline: u64,
    aborted: bool,
}

enum End {
    Done(Cqe),
    /// The set died (a reset by another command, an abandon): the command is lost with the controller's queues.
    Lost,
    /// Quiesced or removed.
    Gone,
    /// The registers read all ones: the device is off the bus, it reaches no memory.
    Vanished,
    Fatal,
    Timeout,
}

enum Abort {
    Sent,
    /// The command's queue set died before the Abort could go out.
    SetDead,
    Failed,
}

/// How a placed command ended.
pub(crate) enum Fin<M: DmaMem> {
    /// Completed (the status may be an error); the ticket still holds its pages (a bounce read copies out).
    Done(Cqe, Ticket<M>),
    /// It ended without effect and the device holds nothing of it: place it once more.
    Retry(CmdError),
    Fail(CmdError),
}

/// Builds the command of request `k` of a pipeline into the bufs.
pub(crate) type Make<'a, M> = &'a dyn Fn(usize, &mut Bufs<M>) -> Result<IoCmd, CmdError>;
/// Takes request `k`'s completed data (the pages it was built with).
pub(crate) type Take<'a, M> = &'a mut dyn FnMut(usize, &[M]) -> Result<(), CmdError>;

impl<H: Hw> Controller<H> {
    /// A start or recovery holds a submitter at most this long (B4: the block layer's dispatcher is Interactive).
    fn ready_bound(&self) -> u64 {
        let rt = Cap { raw: self.cap_raw() }.ready_timeout_ns();
        self.cfg.io_timeout_ns.saturating_mul(2).saturating_add(rt.saturating_mul(2))
    }
    /// How long a command given up waits for the device's stop by someone else (a Quiesce, a start).
    fn stop_bound(&self) -> u64 {
        (QUIESCE_SHUTDOWN_NS + QUIESCE_RDY_MAX_NS + SEC).max(self.ready_bound())
    }

    /// The current queue set once the controller is Ready (waits, bounded, while a start or recovery runs).
    fn ready_set(&self) -> Result<Arc<IoSet<H::Mem>>, CmdError> {
        let end = self.hw.now_ns().saturating_add(self.ready_bound());
        loop {
            match self.state() {
                State::Ready => {
                    if let Some(s) = self.io.read().as_ref().filter(|s| !s.dead()) {
                        return Ok(s.clone());
                    }
                }
                State::Quiesced | State::Removed => return Err(CmdError::Gone),
                State::Starting => {}
                State::Down | State::Failed => return Err(CmdError::NotReady),
            }
            if self.hw.now_ns() >= end {
                klog::event!(Warn, "nvme", "io.ready_timeout", ctrl = self.name.as_str(), ms = self.ready_bound() / MS; "a start or recovery took too long");
                return Err(CmdError::Timeout);
            }
            self.hw.pause_us(1000, true);
        }
    }

    /// Places one command on queue `pick` (None: the caller's CPU's). `block`: wait for a free tag (bounded);
    /// otherwise None when the queue has none.
    pub(crate) fn place(&self, pick: Option<usize>, block: bool, k: usize, make: Make<'_, H::Mem>) -> Result<Option<Ticket<H::Mem>>, CmdError> {
        // A set that dies between the look and the doorbell is looked up again (bounded by `ready_set`).
        for _ in 0..3 {
            let set = self.ready_set()?;
            let qi = pick.map_or_else(|| set.queue_for(self.hw.cpu()), |p| p % set.queues.len().max(1));
            let Some(q) = set.queues.get(qi) else { return Err(CmdError::NotReady) };
            let tag = match q.tags.get() {
                Some(t) => t,
                None if !block => return Ok(None),
                None => match self.wait_tag(&set, q)? {
                    Some(t) => t,
                    None => continue,
                },
            };
            let alloc = || self.hw.alloc_dma(PAGE).ok();
            let mut b = Bufs { q, alloc: &alloc, pages: Vec::new() };
            let c = match make(k, &mut b) {
                Ok(c) => c,
                Err(e) => {
                    q.put_pages(&mut b.pages);
                    q.tags.put(tag);
                    return Err(e);
                }
            };
            let (seq, cid) = q.arm(tag);
            let snap = self.stop_gen.load(Ordering::Acquire);
            let bytes = c.encode(cid);
            let pushed = {
                let mut sq = q.sq.lock();
                if set.dead() || self.state() != State::Ready {
                    None
                } else {
                    sq.push(&bytes, q.sq_head.load(Ordering::Acquire)).ok().inspect(|&tail| {
                        self.stats.io[bytes[0] as usize].fetch_add(1, Ordering::Relaxed);
                        self.hw.wr32(q.sq_db, tail as u32);
                    })
                }
            };
            if pushed.is_none() {
                q.disarm(tag, seq);
                q.put_pages(&mut b.pages);
                q.tags.put(tag);
                if set.dead() {
                    continue;
                }
                return Err(if self.gone() { CmdError::Gone } else { CmdError::NotReady });
            }
            self.stats.placed();
            q.placed.fetch_add(1, Ordering::Relaxed);
            q.inflight.fetch_add(1, Ordering::Relaxed);
            // The queue's first wait; 0 (only `inject_timeouts` sets it) makes the deadline 0 = already passed,
            // which `wait_slot` reports before looking for the completion. No extra load on the normal path.
            let first = q.timeout_ns.load(Ordering::Relaxed);
            let deadline = if first == 0 { 0 } else { self.hw.now_ns().saturating_add(first) };
            let pages = core::mem::take(&mut b.pages);
            return Ok(Some(Ticket { set: set.clone(), qi, tag, seq, cid, op: bytes[0], snap, pages, deadline, aborted: false }));
        }
        Err(CmdError::NotReady)
    }

    /// A free tag of `q`, waited for (bounded); None: the set died meanwhile (look again).
    fn wait_tag(&self, set: &IoSet<H::Mem>, q: &IoQueue<H::Mem>) -> Result<Option<u16>, CmdError> {
        let end = self.hw.now_ns().saturating_add(self.ready_bound());
        loop {
            if let Some(t) = q.tags.get() {
                return Ok(Some(t));
            }
            if set.dead() {
                return Ok(None);
            }
            if self.gone() {
                return Err(CmdError::Gone);
            }
            if self.hw.now_ns() >= end {
                klog::event!(Warn, "nvme", "io.tag_timeout", ctrl = self.name.as_str(), qid = q.qid; "no free tag");
                return Err(CmdError::Timeout);
            }
            self.hw.sleep_on(&q.tags.wq, &mut || q.tags.free() > 0 || set.dead(), SLICE_NS);
        }
    }

    /// Waits for the ticket's completion: spins `spin_ns` reaping, then sleeps on its slot in slices (the hook
    /// wakes it), or polls without vectors; reaps and looks at the controller status after each slice.
    fn wait_slot(&self, t: &Ticket<H::Mem>) -> End {
        let q = &t.set.queues[t.qi];
        let spin_until = self.hw.now_ns().saturating_add(self.cfg.spin_ns);
        if t.deadline == 0 {
            return End::Timeout;
        }
        loop {
            if let Some(c) = q.done(t.tag, t.seq) {
                return End::Done(c);
            }
            if t.set.dead() {
                return End::Lost;
            }
            if self.gone() {
                return End::Gone;
            }
            q.reap(&self.hooks.bell, &self.stats.stale, false);
            if let Some(c) = q.done(t.tag, t.seq) {
                return End::Done(c);
            }
            let now = self.hw.now_ns();
            if now >= t.deadline {
                return End::Timeout;
            }
            if now < spin_until {
                core::hint::spin_loop();
                continue;
            }
            let csts = self.hw.rd32(regs::CSTS);
            if csts == !0 {
                return End::Vanished;
            }
            if csts & regs::CSTS_CFS != 0 {
                return End::Fatal;
            }
            let slice = (t.deadline - now).min(SLICE_NS);
            match q.vector {
                Some(_) => {
                    let s = q.slot(t.tag);
                    self.hw.sleep_on(&s.wq, &mut || q.done(t.tag, t.seq).is_some() || t.set.dead(), slice);
                }
                None => {
                    self.hw.relax();
                    self.hw.pause_us(10, false);
                }
            }
        }
    }

    /// The ticket's tag and pages back (it completed, or the device stopped since it was placed).
    pub(crate) fn release(&self, mut t: Ticket<H::Mem>) {
        let q = &t.set.queues[t.qi];
        q.disarm(t.tag, t.seq);
        q.inflight.fetch_sub(1, Ordering::Relaxed);
        self.stats.ended();
        q.put_pages(&mut t.pages);
        q.tags.put(t.tag);
    }

    fn stopped_since(&self, snap: u64) -> bool {
        self.stop_gen.load(Ordering::Acquire) != snap || matches!(self.state(), State::Removed) || (self.state() == State::Quiesced && self.quiesce_note() != 0)
    }

    /// Waits (bounded) until the device's DMA stopped after `snap`; false on time-out.
    fn wait_stopped(&self, snap: u64, bound: u64) -> bool {
        let end = self.hw.now_ns().saturating_add(bound);
        while !self.stopped_since(snap) {
            if self.hw.now_ns() >= end {
                return false;
            }
            self.hw.pause_us(1000, true);
        }
        true
    }

    /// The last word on a command given up: the device stops (bus master off) unless someone stopped it already.
    fn ensure_stopped(&self, snap: u64) {
        if !self.wait_stopped(snap, self.stop_bound()) {
            klog::event!(Error, "nvme", "io.forced_stop", ctrl = self.name.as_str(); "nobody stopped the device after a command was given up: bus master off");
            self.hw.pci_off();
            self.stop_gen.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Waits for a placed command and handles a timeout (abort, then reset) or a fatal status. `may_recover`:
    /// a reset is allowed (the first try); otherwise a command still out gives the controller up.
    pub(crate) fn finish(&self, mut t: Ticket<H::Mem>, may_recover: bool) -> Fin<H::Mem> {
        loop {
            match self.wait_slot(&t) {
                End::Done(c) => {
                    // An aborted command (Command Abort Requested) did nothing: once more.
                    if t.aborted && c.sct() == 0 && c.sc() == 0x07 {
                        self.release(t);
                        return Fin::Retry(CmdError::Timeout);
                    }
                    return Fin::Done(c, t);
                }
                End::Timeout => {
                    self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                    // Its queues were reset meanwhile (another command's recovery): the new controller has other
                    // commands under the same qid and cid, so no Abort; the reset ended this one.
                    if t.set.dead() {
                        return self.lost(t);
                    }
                    if !t.aborted {
                        match self.abort(&t) {
                            Abort::Sent => {
                                t.aborted = true;
                                t.deadline = self.hw.now_ns().saturating_add(self.cfg.io_timeout_ns);
                                continue;
                            }
                            Abort::SetDead => return self.lost(t),
                            Abort::Failed => {}
                        }
                    }
                    return self.reset_for(t, CmdError::Timeout, may_recover);
                }
                End::Fatal => return self.reset_for(t, CmdError::Fatal, may_recover),
                End::Lost => return self.lost(t),
                End::Gone => {
                    self.ensure_stopped(t.snap);
                    self.release(t);
                    return Fin::Fail(CmdError::Gone);
                }
                // Off the bus (or a passing all-ones read): given up like any other command, so the device is
                // stopped (stop_gen) before its pages and the caller's memory go back, the other queues' waiters
                // learn it from the dead set, and later I/O fails at once instead of timing out.
                End::Vanished => {
                    let e = CmdError::Gone;
                    klog::event!(Error, "nvme", "io.vanished", ctrl = self.name.as_str(), qid = t.set.queues[t.qi].qid; "registers read all ones: controller given up");
                    self.abandon(&e);
                    self.ensure_stopped(t.snap);
                    self.release(t);
                    return Fin::Fail(e);
                }
            }
        }
    }

    /// The ticket's set died (a reset or an abandon by someone else): place it once more once the device stopped
    /// after it was placed; give up when nobody stops it.
    fn lost(&self, t: Ticket<H::Mem>) -> Fin<H::Mem> {
        let snap = t.snap;
        if self.wait_stopped(snap, self.stop_bound()) {
            self.release(t);
            return Fin::Retry(CmdError::NotReady);
        }
        let e = CmdError::NotReady;
        self.abandon(&e);
        self.ensure_stopped(snap);
        self.release(t);
        Fin::Fail(e)
    }

    /// Linux `nvme_timeout`: the first timeout of a command sends Abort (the command may still complete; it is
    /// waited for once more). False: the abort could not be sent, the next step is a reset.
    fn abort(&self, t: &Ticket<H::Mem>) -> Abort {
        let qid = t.set.queues[t.qi].qid;
        klog::event!(Warn, "nvme", "cmd.timeout", ctrl = self.name.as_str(), qid = qid, cid = klog::Hex(t.cid as u64), opcode = klog::Hex(t.op as u64);
            "I/O command timed out: sending Abort");
        // Looked at again under the admin lock: a reset kills the set before it takes that lock to set the new
        // queues up, so an Abort sent here never reaches a controller that has new commands under this qid/cid.
        let r = self.with_admin(|a| {
            if t.set.dead() {
                return Ok(None);
            }
            self.admin_cmd(a, cmd::AdminCmd::abort(qid, t.cid)).map(Some).map_err(|e| e.text())
        });
        match r {
            Ok(Some(c)) => {
                self.stats.aborts.fetch_add(1, Ordering::Relaxed);
                klog::event!(Info, "nvme", "cmd.abort_sent", ctrl = self.name.as_str(), qid = qid, cid = klog::Hex(t.cid as u64), aborted = c.dw0 & 1 == 0);
                Abort::Sent
            }
            Ok(None) => {
                klog::event!(Info, "nvme", "cmd.abort_skipped", ctrl = self.name.as_str(), qid = qid, cid = klog::Hex(t.cid as u64); "its queues were reset meanwhile");
                Abort::SetDead
            }
            Err(e) => {
                if t.set.dead() {
                    return Abort::SetDead;
                }
                klog::event!(Warn, "nvme", "cmd.abort_failed", ctrl = self.name.as_str(), qid = qid, error = e.as_str(); "resetting instead");
                Abort::Failed
            }
        }
    }

    /// The command did not come back: reset and set up the controller again (then place it once more), or, when
    /// that is not allowed or a start by someone else does not stop the device, give the controller up.
    fn reset_for(&self, t: Ticket<H::Mem>, e: CmdError, may_recover: bool) -> Fin<H::Mem> {
        let snap = t.snap;
        if !may_recover || self.gone() {
            self.abandon(&e);
            self.ensure_stopped(snap);
            self.release(t);
            return Fin::Fail(e);
        }
        if self.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            klog::event!(Warn, "nvme", "ctrl.recover_skipped", ctrl = self.name.as_str(); "a start is running: its reset ends the command");
            if self.wait_stopped(snap, self.stop_bound()) {
                self.release(t);
                return Fin::Retry(e);
            }
            self.abandon(&e);
            self.ensure_stopped(snap);
            self.release(t);
            return Fin::Fail(e);
        }
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
        klog::event!(Warn, "nvme", "ctrl.recover", ctrl = self.name.as_str(), error = e.text(); "reset and set-up again, then one retry");
        let r = self.start_locked(None);
        self.starting.store(false, Ordering::Release);
        // The reset's disable (or the stop after a failed one) ended the command on the device.
        self.ensure_stopped(snap);
        self.release(t);
        match r {
            Ok(()) => {
                klog::event!(Info, "nvme", "ctrl.recovered", ctrl = self.name.as_str());
                Fin::Retry(e)
            }
            Err(why) => {
                klog::event!(Error, "nvme", "ctrl.recover_failed", ctrl = self.name.as_str(), error = why.as_str());
                Fin::Fail(e)
            }
        }
    }

    /// One command, waited for, with one retry after a recovery. `take` sees the pages before they go back.
    pub(crate) fn exec(&self, pick: Option<usize>, k: usize, make: Make<'_, H::Mem>, take: Take<'_, H::Mem>, tried: bool) -> Result<Cqe, CmdError> {
        let mut tried = tried;
        loop {
            let t = self.place(pick, true, k, make)?.ok_or(CmdError::NotReady)?;
            let op = t.op;
            match self.finish(t, !tried) {
                Fin::Done(c, t) => return self.take_done(c, t, k, take),
                Fin::Retry(e) if !tried => {
                    let _ = self.note(Err(e), "io", op);
                    tried = true;
                }
                Fin::Retry(e) | Fin::Fail(e) => return self.note(Err(e), "io", op),
            }
        }
    }

    fn take_done(&self, c: Cqe, t: Ticket<H::Mem>, k: usize, take: Take<'_, H::Mem>) -> Result<Cqe, CmdError> {
        let r = self.note(Ok(c), "io", t.op).and_then(|c| take(k, &t.pages).map(|()| c));
        self.release(t);
        r
    }

    /// `n` commands, up to `window` of them in flight at once (all of a queue's tags at most), each retried once
    /// on its own after a recovery; the first error is returned once every placed command ended. Returns the most
    /// in flight at once. A placement waits for a free tag only while this call holds none, and a retry goes back
    /// into the line instead of being waited for at once, so two calls that hold tags never wait for each other.
    pub(crate) fn pipeline(&self, pick: &dyn Fn(usize) -> Option<usize>, n: usize, window: usize, make: Make<'_, H::Mem>, take: Take<'_, H::Mem>) -> Result<usize, CmdError> {
        // (request, ticket, already retried once)
        let mut out: VecDeque<(usize, Ticket<H::Mem>, bool)> = VecDeque::new();
        let mut again: VecDeque<usize> = VecDeque::new();
        let (mut next, mut most) = (0usize, 0usize);
        let mut err: Option<CmdError> = None;
        loop {
            let want = again.front().map(|&k| (k, true)).or((next < n).then_some((next, false)));
            if let Some((k, retry)) = want
                && err.is_none()
                && out.len() < window.max(1)
            {
                match self.place(pick(k), out.is_empty(), k, make) {
                    Ok(Some(t)) => {
                        if retry {
                            again.pop_front();
                        } else {
                            next += 1;
                        }
                        out.push_back((k, t, retry));
                        most = most.max(out.len());
                        continue;
                    }
                    // No free tag: the oldest first.
                    Ok(None) => {}
                    Err(e) => err = Some(e),
                }
            }
            let Some((k, t, retried)) = out.pop_front() else { break };
            let op = t.op;
            let r = match self.finish(t, !retried) {
                Fin::Done(c, t) => self.take_done(c, t, k, take).map(|_| ()),
                Fin::Retry(e) if !retried => {
                    let _ = self.note(Err(e), "io", op);
                    again.push_back(k);
                    Ok(())
                }
                Fin::Retry(e) | Fin::Fail(e) => self.note(Err(e), "io", op).map(|_| ()),
            };
            if let Err(e) = r {
                err.get_or_insert(e);
            }
        }
        err.map_or(Ok(most), Err)
    }

    fn ns_geometry(&self, nsid: u32, check: bool) -> Result<(usize, u64, usize), CmdError> {
        let i = self.info.lock();
        match i.namespaces.iter().find(|n| n.nsid == nsid) {
            Some(n) => Ok((n.block_size() as usize, n.id.nsze, i.max_transfer)),
            None if !check => Ok((512, u64::MAX, i.max_transfer)),
            None => Err(CmdError::Invalid),
        }
    }

    /// Reads `buf.len() / block_size` blocks of `nsid` from `lba`, in transfers of at most `max_transfer`, many in
    /// flight. `check` = bounds of the namespace are checked (ktests pass false to see the device's own answer).
    pub fn read(&self, nsid: u32, lba: u64, buf: &mut [u8], check: bool) -> Result<(), CmdError> {
        let (bs, blocks, xfer) = self.ns_geometry(nsid, check)?;
        if buf.is_empty() || !buf.len().is_multiple_of(bs) || xfer < bs {
            return Err(CmdError::Invalid);
        }
        if check && lba.checked_add((buf.len() / bs) as u64).is_none_or(|e| e > blocks) {
            return Err(CmdError::Invalid);
        }
        let per = (xfer / bs).min(65536) * bs;
        let mut reqs: Vec<(u64, &mut [u8])> = buf.chunks_mut(per).enumerate().map(|(i, c)| (lba + (i * per / bs) as u64, c)).collect();
        self.read_reqs(nsid, bs, &mut reqs, &|_| None, None).map(|_| ())
    }

    /// BL-1: several reads of `nsid` at once, each (lba, buffer) one command of at most `max_transfer` bytes, on
    /// queue `pick(i)` (None: the caller's CPU's), up to `window` in flight (None: as many as the queue has tags
    /// when every buffer is reached in place, two through bounce pages); every byte lands in its buffer. Returns
    /// the most in flight.
    pub fn read_vectored(&self, nsid: u32, reqs: &mut [(u64, &mut [u8])], pick: &dyn Fn(usize) -> Option<usize>, window: Option<usize>) -> Result<usize, CmdError> {
        let (bs, blocks, xfer) = self.ns_geometry(nsid, true)?;
        for (lba, b) in reqs.iter() {
            if b.is_empty() || !b.len().is_multiple_of(bs) || b.len() > xfer || lba.checked_add((b.len() / bs) as u64).is_none_or(|e| e > blocks) {
                return Err(CmdError::Invalid);
            }
        }
        self.read_reqs(nsid, bs, reqs, pick, window)
    }

    fn read_reqs(&self, nsid: u32, bs: usize, reqs: &mut [(u64, &mut [u8])], pick: &dyn Fn(usize) -> Option<usize>, window: Option<usize>) -> Result<usize, CmdError> {
        // Where the device can write each buffer in place (None: bounce pages).
        let plan: Vec<(u64, usize, Option<u64>)> = reqs.iter().map(|(l, b)| (*l, b.len(), self.hw.dma_target(b).filter(|a| a.is_multiple_of(4)))).collect();
        // A bounce read holds its pages until it is copied out: two in flight per call keep the device busy while
        // one is copied; in place, as many as the queue has tags.
        let window = window.unwrap_or(if plan.iter().all(|p| p.2.is_some()) { usize::MAX } else { 2 });
        let make = |k: usize, b: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
            let (at, len, direct) = plan[k];
            let (p1, p2) = data_prps(b, len, direct.map(Direct::Contig), None)?;
            Ok(IoCmd::read(nsid, at, (len / bs) as u32, p1, p2))
        };
        let mut take = |k: usize, pages: &[H::Mem]| -> Result<(), CmdError> {
            let (_, len, direct) = plan[k];
            let dst = &mut *reqs[k].1;
            if direct.is_some() {
                NVME_DIRECT.add(len as u64);
            } else {
                for (i, chunk) in dst.chunks_mut(PAGE).enumerate() {
                    if !pages.get(i).is_some_and(|p| p.copy_out(0, chunk)) {
                        return Err(CmdError::NotReady);
                    }
                }
                COPY_NVME_BOUNCE.add(len as u64);
            }
            self.stats.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
            Ok(())
        };
        self.pipeline(pick, plan.len(), window, &make, &mut take)
    }

    /// BL-2: reads into frames: each `(lba, buf)` cut into commands of at most `max_transfer`, all in flight at
    /// once on the caller's CPU's queue (up to its tags). A command whose memory the PRP rules allow
    /// (`frame_pages`: page-cache pages always do) lands straight in the frames; any other through bounce pages,
    /// copied into the frames (`block.copy_bytes.nvme_bounce`). Returns the most in flight.
    pub fn read_frames(&self, nsid: u32, reqs: &[(u64, &IoBuf)]) -> Result<usize, CmdError> {
        let (bs, blocks, xfer) = self.ns_geometry(nsid, true)?;
        if xfer < bs {
            return Err(CmdError::Invalid);
        }
        let per = (xfer / bs).min(65536) * bs;
        let mut plan: Vec<(u64, IoBuf, Option<Vec<u64>>)> = Vec::new();
        for (lba, buf) in reqs {
            let len = buf.len();
            if len == 0 || !len.is_multiple_of(bs) || lba.checked_add((len / bs) as u64).is_none_or(|e| e > blocks) {
                return Err(CmdError::Invalid);
            }
            let mut off = 0usize;
            while off < len {
                let n = per.min(len - off);
                let piece = buf.slice(off, n).ok_or(CmdError::Invalid)?;
                let pages = frame_pages(&piece);
                plan.try_reserve(1).map_err(|_| CmdError::NotReady)?;
                plan.push((lba + (off / bs) as u64, piece, pages));
                off += n;
            }
        }
        // As `read_reqs`: in place, as many as the queue has tags; through bounce pages, two.
        let window = if plan.iter().all(|p| p.2.is_some()) { usize::MAX } else { 2 };
        let make = |k: usize, b: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
            let (at, ref piece, ref pages) = plan[k];
            let len = piece.len();
            let (p1, p2) = data_prps(b, len, pages.as_deref().map(Direct::Pages), None)?;
            Ok(IoCmd::read(nsid, at, (len / bs) as u32, p1, p2))
        };
        let mut scratch: Vec<u8> = Vec::new();
        let mut take = |k: usize, pages: &[H::Mem]| -> Result<(), CmdError> {
            let (_, ref piece, ref direct) = plan[k];
            let len = piece.len();
            if direct.is_some() {
                NVME_DIRECT.add(len as u64);
            } else {
                if scratch.len() < PAGE {
                    scratch.try_reserve_exact(PAGE).map_err(|_| CmdError::NotReady)?;
                    scratch.resize(PAGE, 0);
                }
                let mut off = 0usize;
                for p in pages.iter() {
                    if off >= len {
                        break;
                    }
                    let n = PAGE.min(len - off);
                    if !p.copy_out(0, &mut scratch[..n]) || !piece.write(off, &scratch[..n]) {
                        return Err(CmdError::NotReady);
                    }
                    off += n;
                }
                if off < len {
                    return Err(CmdError::NotReady);
                }
                COPY_NVME_BOUNCE.add(len as u64);
            }
            self.stats.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
            Ok(())
        };
        self.pipeline(&|_| None, plan.len(), window, &make, &mut take)
    }

    /// BL-2: the tags of all I/O queues now (the block layer's slots for this controller's disks); at least 1.
    pub fn queue_depth(&self) -> u32 {
        let g = self.io.read();
        let n: usize = g.as_ref().map_or(0, |s| s.queues.iter().map(|q| q.tag_count()).sum());
        u32::try_from(n).unwrap_or(u32::MAX).max(1)
    }

    /// Reads `bytes` from `lba` on into scratch pages only (the `nvme` tool's read speed: no copy), with as many
    /// commands in flight as the caller's CPU's queue has tags. Returns (bytes read, ns taken, most in flight).
    pub fn read_speed(&self, nsid: u32, lba: u64, bytes: u64) -> Result<(u64, u64, usize), CmdError> {
        let (bs, blocks, xfer) = {
            let i = self.info.lock();
            let n = i.namespaces.iter().find(|n| n.nsid == nsid && n.unusable.is_none()).ok_or(CmdError::Invalid)?;
            (n.block_size() as u64, n.id.nsze, i.max_transfer as u64)
        };
        let per = (xfer / bs).clamp(1, 65536);
        let total = (bytes / bs).min(blocks.saturating_sub(lba));
        let n = total.div_ceil(per) as usize;
        // One set of scratch pages and one list for every command: the data goes nowhere.
        let pages = ((per * bs) as usize).div_ceil(PAGE);
        let mut scratch: Vec<H::Mem> = Vec::with_capacity(pages + 1);
        for _ in 0..=pages {
            scratch.push(self.hw.alloc_dma(PAGE).map_err(|_| CmdError::NotReady)?);
        }
        let list = scratch[pages].dev_addr();
        for k in 1..pages {
            if !scratch[pages].wr64(8 * (k - 1), scratch[k].dev_addr()) {
                return Err(CmdError::NotReady);
            }
        }
        let make = |k: usize, _: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
            let at = lba + k as u64 * per;
            let nb = per.min(lba + total - at);
            let len = (nb * bs) as usize;
            let second = scratch.get(1).map_or(0, |p| p.dev_addr());
            let p = prp::plan(scratch[0].dev_addr(), len, second, list).ok_or(CmdError::Invalid)?;
            Ok(IoCmd::read(nsid, at, nb as u32, p.prp1, p.prp2))
        };
        let mut take = |k: usize, _: &[H::Mem]| -> Result<(), CmdError> {
            let at = lba + k as u64 * per;
            self.stats.bytes_read.fetch_add(per.min(lba + total - at) * bs, Ordering::Relaxed);
            Ok(())
        };
        let t0 = self.hw.now_ns();
        let most = self.pipeline(&|_| None, n, usize::MAX, &make, &mut take)?;
        // Every command ended (completed, or the device stopped): the scratch pages may go.
        drop(scratch);
        Ok((total * bs, self.hw.now_ns().saturating_sub(t0), most))
    }

    /// Writes `data` at `lba` of `nsid`, only inside the armed window (FUA: durable when each command completes).
    /// The data goes from the caller's memory or through bounce pages; a retry after a recovery builds again. The
    /// window is read again for every command placed: a recovery that found other namespaces has closed it.
    pub fn write(&self, nsid: u32, lba: u64, data: &[u8], fua: bool) -> Result<(), CmdError> {
        let Some(early) = self.window() else { return Err(self.outside("write", nsid, lba, 0)) };
        let (bs, xfer) = {
            let i = self.info.lock();
            let n = i.namespaces.iter().find(|n| n.nsid == nsid).ok_or(CmdError::Invalid)?;
            (n.block_size() as usize, i.max_transfer)
        };
        if data.is_empty() || !data.len().is_multiple_of(bs) || xfer < bs {
            return Err(CmdError::Invalid);
        }
        if !early.range_holds(nsid, lba, (data.len() / bs) as u64) {
            return Err(self.outside("write", nsid, lba, (data.len() / bs) as u64));
        }
        let per = (xfer / bs).min(65536);
        let mut done = 0usize;
        for chunk in data.chunks(per * bs) {
            let at = lba + (done / bs) as u64;
            let len = chunk.len();
            let direct = self.hw.dma_target(chunk).filter(|a| a.is_multiple_of(4));
            let make = |_: usize, b: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
                let w = self.window().ok_or(CmdError::OutsideWindow)?;
                let (p1, p2) = data_prps(b, len, direct.map(Direct::Contig), Some(chunk))?;
                IoCmd::write(&w, nsid, at, (len / bs) as u32, p1, p2, fua).ok_or(CmdError::OutsideWindow)
            };
            self.exec(None, 0, &make, &mut |_, _| Ok(()), false).map_err(|e| self.noted_outside(e, "write", nsid, at, (len / bs) as u64))?;
            if direct.is_some() {
                NVME_DIRECT.add(len as u64);
            }
            self.stats.bytes_written.fetch_add(len as u64, Ordering::Relaxed);
            done += len;
        }
        Ok(())
    }

    /// BL-2: writes `buf`'s frames at `lba` of `nsid`, straight from the frames where the PRP rules allow it
    /// (`frame_pages`: page-cache pages always do, Linux `nvme_pci_setup_data_prp`), else through bounce pages
    /// filled from them (`block.copy_bytes.nvme_bounce`). The window rules are `write`'s, unchanged: the whole range
    /// checked first, the window read again for every command placed, the builder refusing anything outside it.
    pub fn write_frames(&self, nsid: u32, lba: u64, buf: &IoBuf, fua: bool) -> Result<(), CmdError> {
        let Some(early) = self.window() else { return Err(self.outside("write", nsid, lba, 0)) };
        let (bs, xfer) = {
            let i = self.info.lock();
            let n = i.namespaces.iter().find(|n| n.nsid == nsid).ok_or(CmdError::Invalid)?;
            (n.block_size() as usize, i.max_transfer)
        };
        let total = buf.len();
        if total == 0 || !total.is_multiple_of(bs) || xfer < bs {
            return Err(CmdError::Invalid);
        }
        if !early.range_holds(nsid, lba, (total / bs) as u64) {
            return Err(self.outside("write", nsid, lba, (total / bs) as u64));
        }
        let per = (xfer / bs).min(65536) * bs;
        let mut done = 0usize;
        while done < total {
            let len = per.min(total - done);
            let at = lba + (done / bs) as u64;
            let piece = buf.slice(done, len).ok_or(CmdError::Invalid)?;
            let pages = frame_pages(&piece);
            let make = |_: usize, b: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
                let w = self.window().ok_or(CmdError::OutsideWindow)?;
                let (p1, p2) = data_prps(b, len, pages.as_deref().map(Direct::Pages), None)?;
                if pages.is_none() {
                    // Bounce pages 0.. hold the data in order (`data_prps`): filled from the frames a page at a time.
                    let mut tmp: Vec<u8> = Vec::new();
                    tmp.try_reserve_exact(PAGE.min(len)).map_err(|_| CmdError::NotReady)?;
                    tmp.resize(PAGE.min(len), 0);
                    let mut off = 0usize;
                    for pg in b.pages.iter().take(len.div_ceil(PAGE)) {
                        let n = PAGE.min(len - off);
                        if !piece.read(off, &mut tmp[..n]) || !pg.copy_in(0, &tmp[..n]) {
                            return Err(CmdError::NotReady);
                        }
                        off += n;
                    }
                }
                IoCmd::write(&w, nsid, at, (len / bs) as u32, p1, p2, fua).ok_or(CmdError::OutsideWindow)
            };
            self.exec(None, 0, &make, &mut |_, _| Ok(()), false).map_err(|e| self.noted_outside(e, "write", nsid, at, (len / bs) as u64))?;
            if pages.is_some() {
                NVME_DIRECT.add(len as u64);
            } else {
                COPY_NVME_BOUNCE.add(len as u64);
            }
            self.stats.bytes_written.fetch_add(len as u64, Ordering::Relaxed);
            done += len;
        }
        Ok(())
    }

    /// Deallocates `blocks` from `lba` of `nsid`, only inside the armed window (Dataset Management, one range per
    /// command in its own page; its NLB field holds 32 bits).
    pub fn deallocate(&self, nsid: u32, lba: u64, blocks: u64) -> Result<(), CmdError> {
        let Some(early) = self.window() else { return Err(self.outside("deallocate", nsid, lba, blocks)) };
        if !early.range_holds(nsid, lba, blocks) {
            return Err(self.outside("deallocate", nsid, lba, blocks));
        }
        let (mut at, end) = (lba, lba + blocks);
        while at < end {
            let n = (end - at).min(u32::MAX as u64) as u32;
            let make = |_: usize, b: &mut Bufs<H::Mem>| -> Result<IoCmd, CmdError> {
                let w = self.window().ok_or(CmdError::OutsideWindow)?;
                let i = b.page()?;
                let c = IoCmd::deallocate(&w, nsid, at, n, b.pages[i].dev_addr()).ok_or(CmdError::OutsideWindow)?;
                if !b.pages[i].copy_in(0, &cmd::dsm_range(at, n)) {
                    return Err(CmdError::NotReady);
                }
                Ok(c)
            };
            self.exec(None, 0, &make, &mut |_, _| Ok(()), false).map_err(|e| self.noted_outside(e, "deallocate", nsid, at, n as u64))?;
            at += n as u64;
        }
        Ok(())
    }

    /// The builder said no although the check above passed (the window closed or changed since, by a recovery): counted
    /// and logged as every other refusal.
    fn noted_outside(&self, e: CmdError, what: &str, nsid: u32, lba: u64, blocks: u64) -> CmdError {
        if e == CmdError::OutsideWindow { self.outside(what, nsid, lba, blocks) } else { e }
    }

    pub fn flush(&self, nsid: u32) -> Result<(), CmdError> {
        self.exec(None, 0, &|_, _| Ok(IoCmd::flush(nsid)), &mut |_, _| Ok(()), false).map(|_| ())
    }

    /// The I/O queues now (ktests, the tool): (qid, vector, cpu, tags, free tags, placed, completed, in flight,
    /// pool pages).
    pub fn queue_stats(&self) -> Vec<QueueStats> {
        let g = self.io.read();
        let Some(s) = g.as_ref() else { return Vec::new() };
        s.queues
            .iter()
            .map(|q| QueueStats {
                qid: q.qid,
                vector: q.vector,
                cpu: q.cpu,
                tags: q.tag_count(),
                free: q.tags.free(),
                placed: q.placed.load(Ordering::Relaxed),
                completed: q.completed.load(Ordering::Relaxed),
                inflight: q.inflight.load(Ordering::Relaxed),
                pool: q.pool_len(),
            })
            .collect()
    }

    /// ktests: while `on`, every command placed on queue index `qi` times out at once (its first wait is 0), so
    /// the abort path runs there while the other queues go on; off gives the queue its timeout back. It rides on the
    /// queue's timeout word, which every placement reads anyway, so it costs nothing in normal use. A reset makes
    /// new queues without it. False: no such queue.
    pub fn inject_timeouts(&self, qi: usize, on: bool) -> bool {
        let g = self.io.read();
        match g.as_ref().and_then(|s| s.queues.get(qi)) {
            Some(q) => {
                q.timeout_ns.store(if on { 0 } else { self.cfg.io_timeout_ns.max(1) }, Ordering::Release);
                true
            }
            None => false,
        }
    }
}

/// One I/O queue's numbers (`Controller::queue_stats`).
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueStats {
    pub qid: u16,
    pub vector: Option<u16>,
    pub cpu: Option<u32>,
    pub tags: usize,
    pub free: usize,
    pub placed: u64,
    pub completed: u64,
    pub inflight: u32,
    pub pool: usize,
}

/// Where a transfer's memory is for the device: one contiguous piece, or (BL-2) the pages of a request's frames.
#[derive(Clone, Copy)]
pub(crate) enum Direct<'a> {
    Contig(u64),
    /// PRP entries: the first byte (dword aligned), then each following page (page aligned).
    Pages(&'a [u64]),
}

/// BL-2: the PRP entries of `buf` read in place, when its memory allows it (Linux 7.x
/// drivers/nvme/host/pci.c `nvme_pci_setup_data_prp` :1028): the first piece starts dword aligned, every later
/// piece starts on a page, every piece but the last ends on one, and one list page holds them. None: through
/// bounce pages.
pub(crate) fn frame_pages(buf: &IoBuf) -> Option<Vec<u64>> {
    let (len, page) = (buf.len(), PAGE as u64);
    let mut out: Vec<u64> = Vec::new();
    let mut off = 0usize;
    while off < len {
        let (a, run) = buf.phys(off)?;
        if (off == 0 && !a.is_multiple_of(4)) || (off != 0 && !a.is_multiple_of(page)) {
            return None;
        }
        let end = off.checked_add(run)?;
        if end < len && !a.checked_add(run as u64)?.is_multiple_of(page) {
            return None;
        }
        let n = prp::pages(a, run);
        out.try_reserve(n).ok()?;
        out.push(a);
        out.extend((1..n).map(|k| prp::contig_page(a, k)));
        off = end;
    }
    (!out.is_empty() && out.len() - 1 <= PAGE / 8).then_some(out)
}

/// PRP1/PRP2 of a data transfer of `len` bytes: in place at `direct` (contiguous, or pages), else on bounce pages taken into
/// `b` (pages 0..n hold the data in order; `fill` is copied into them for a write). A list page joins when the
/// transfer spans more than two pages.
fn data_prps<M: DmaMem>(b: &mut Bufs<M>, len: usize, direct: Option<Direct<'_>>, fill: Option<&[u8]>) -> Result<(u64, u64), CmdError> {
    let (first, n) = match direct {
        Some(Direct::Contig(a)) => (a, prp::pages(a, len)),
        // BL-2: the caller's frames, one entry per page (`frame_pages` checked the PRP rules).
        Some(Direct::Pages(p)) => {
            let first = *p.first().ok_or(CmdError::Invalid)?;
            if p.len() != prp::pages(first, len) {
                return Err(CmdError::Invalid);
            }
            (first, p.len())
        }
        None => {
            let n = len.div_ceil(PAGE);
            for _ in 0..n {
                b.page()?;
            }
            (b.pages[0].dev_addr(), n)
        }
    };
    let page_at = |b: &Bufs<M>, k: usize| match direct {
        Some(Direct::Contig(a)) => prp::contig_page(a, k),
        Some(Direct::Pages(p)) => p[k],
        None => b.pages[k].dev_addr(),
    };
    let list = if n > 2 { Some(b.page()?) } else { None };
    let second = if n >= 2 { page_at(b, 1) } else { 0 };
    let p = prp::plan(first, len, second, list.map_or(0, |i| b.pages[i].dev_addr())).ok_or(CmdError::Invalid)?;
    if let Some(li) = list {
        for k in 1..=p.list_len {
            let a = page_at(b, k);
            if !b.pages[li].wr64(8 * (k - 1), a) {
                return Err(CmdError::Invalid);
            }
        }
    }
    if direct.is_none()
        && let Some(src) = fill
    {
        for (i, chunk) in src.chunks(PAGE).enumerate() {
            if !b.pages[i].copy_in(0, chunk) {
                return Err(CmdError::NotReady);
            }
        }
    }
    Ok((p.prp1, p.prp2))
}
