//! BL-2: dispatching in the submitter's context (Linux 7.x block/blk-mq.c `blk_mq_submit_bio` :3104 and the plug,
//! `blk_mq_flush_plug_list` :2951; mq-deadline's priority and aging as the elevator's rules, block/mq-deadline.c
//! `__dd_dispatch_request` :322). A waiter whose request is still queued takes a slot of the device, picks the next
//! batch (`Elevator::pick_with`, beside what the device has) and calls the device; reads into frames go beside it
//! while slots are free, all in one `read_frames` call. A stacked disk runs only its caller's own requests.
//!
//! Hand-off: whoever gives a slot back wakes one sleeping waiter whose request is queued (`Req::kick`), so a free
//! slot never waits for a poll; every waiter also looks by itself every `POLL_NS` (B4: it never sleeps without a
//! deadline, and a lost wake costs at most that).

use super::*;
use crate::frames::Concat;
use crate::queue::{Batch, Limits};

/// What a dispatcher took out of the queue.
enum Step {
    /// Batches with their requests: the first as the elevator chose it, the others reads into frames beside it.
    Run(Vec<(Batch, Vec<Arc<Req>>)>),
    /// No memory to build a batch: its requests were failed with ENOMEM (logged by `dispatch`, outside the lock).
    NoMemory { kind: Kind, lba: u64, blocks: u64, members: usize },
}

/// Slots of the device held by one dispatcher; given back on drop, and a sleeping waiter is woken.
struct Slots<'a> {
    disk: &'a Disk,
    n: u32,
}

impl Drop for Slots<'_> {
    fn drop(&mut self) {
        if self.n > 0 {
            self.disk.active.fetch_sub(self.n, Ordering::SeqCst);
            self.disk.hand_off();
        }
    }
}

/// A byte read straight into its caller's buffer (`Disk::read`): counted at the device until dropped.
pub(super) struct Direct<'a> {
    disk: &'a Disk,
    id: u64,
    slot: bool,
}

impl Drop for Direct<'_> {
    fn drop(&mut self) {
        {
            let mut q = self.disk.q.lock();
            q.direct = q.direct.saturating_sub(1);
            if self.slot {
                q.inflight.retain(|e| e.id != self.id);
            }
        }
        if self.slot {
            self.disk.progress.fetch_add(1, Ordering::SeqCst);
            self.disk.active.fetch_sub(1, Ordering::SeqCst);
            self.disk.hand_off();
        }
    }
}

/// A failed call over several requests: try each again on its own only for an error that belongs to the data
/// (a bad block, a range the device refused); a timeout, a gone device or one that cannot do it fails them all.
fn alone_again(e: BlockError) -> bool {
    !matches!(e, BlockError::Timeout | BlockError::Gone | BlockError::Unsupported)
}

fn one<T>(x: T) -> Option<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(1).ok()?;
    v.push(x);
    Some(v)
}

impl Disk {
    fn take_slot(&self) -> bool {
        let mut a = self.active.load(Ordering::SeqCst);
        loop {
            if a >= self.slots {
                return false;
            }
            match self.active.compare_exchange_weak(a, a + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(x) => a = x,
            }
        }
    }

    fn slot_free(&self) -> bool {
        self.active.load(Ordering::SeqCst) < self.slots
    }

    /// Wakes one waiter whose request is queued and who sleeps (a slot came free, or the device answered).
    fn hand_off(&self) {
        let next = {
            let q = self.q.lock();
            if q.elev.is_empty() { None } else { q.reqs.iter().find(|r| r.queued.load(Ordering::SeqCst) && r.sleeping.load(Ordering::SeqCst)).cloned() }
        };
        if let Some(r) = next {
            r.kick.wake_one();
        }
    }

    /// A byte read the caller may run at once, straight into its buffer: on a stacked disk while it is open, at the
    /// bottom only when nothing is queued, a slot is free and nothing the device has orders before it (so the
    /// elevator's priorities and order are what they would be). None: through a request of its own.
    pub(super) fn direct(&self, lba: u64, blocks: u64, prio: IoPriority) -> Option<Direct<'_>> {
        let mut q = self.q.lock();
        if q.stopped || q.closed {
            return None;
        }
        if self.passthrough {
            q.direct += 1;
            return Some(Direct { disk: self, id: 0, slot: false });
        }
        let e = Entry { id: self.next_id.fetch_add(1, Ordering::Relaxed), kind: Kind::Read, lba, blocks, prio, at_ns: now() };
        if !q.elev.is_empty() || crate::queue::held_by(&q.inflight, &e) || q.inflight.try_reserve(1).is_err() || !self.take_slot() {
            return None;
        }
        q.inflight.push(e);
        q.direct += 1;
        Some(Direct { disk: self, id: e.id, slot: true })
    }

    /// The ticket's end: waits (dispatching), takes the request out of the queue on a timeout, gives the data back.
    pub(super) fn settle_req(&self, req: &Arc<Req>, timeout_ns: u64) -> Result<Data, BlockError> {
        let done = self.wait_req(req, timeout_ns);
        if !done && !self.cancel(req) && !req.done.is_done() {
            TIMEOUTS.inc();
            self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            self.timed_out.store(true, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.timeout", disk = self.name.clone(), op = req.kind.as_str(), lba = req.lba,
                blocks = req.blocks, timeout_ms = timeout_ns / 1_000_000; "the device still has it: its answer will be dropped");
            return Err(BlockError::Timeout);
        }
        let r = req.result.lock().take().unwrap_or(Err(BlockError::Timeout));
        r.map(|()| core::mem::take(&mut *req.data.lock()))
    }

    /// Waits for `req`, dispatching while it is queued and a slot is free. True when it completed.
    fn wait_req(&self, req: &Arc<Req>, timeout_ns: u64) -> bool {
        let start = ksync::now_ns();
        let mut polls: u64 = 0;
        // The progress count when this waiter last found nothing it could take (the order rule held it back).
        let mut stuck: Option<u64> = None;
        loop {
            if req.done.is_done() {
                return true;
            }
            // A stacked disk that could not hand its request down (no memory for two pointers): the next try
            // only after a poll, never at once (it would spin in the caller until its deadline).
            let mut idle = false;
            if req.queued.load(Ordering::SeqCst) {
                if self.passthrough {
                    if self.run_own(core::slice::from_ref(req)) {
                        continue;
                    }
                    idle = true;
                } else if self.take_slot() {
                    let seen = self.progress.load(Ordering::SeqCst);
                    if self.dispatch(Slots { disk: self, n: 1 }, Some(req.id)) {
                        stuck = None;
                        continue;
                    }
                    stuck = Some(seen);
                }
            }
            // Without a clock (host) the deadline counts polls instead.
            let elapsed = match (start, ksync::now_ns()) {
                (Some(s), Some(n)) => n.saturating_sub(s),
                _ => {
                    polls += 1;
                    polls.saturating_mul(POLL_NS)
                }
            };
            if elapsed >= timeout_ns {
                return req.done.is_done();
            }
            req.sleeping.store(true, Ordering::SeqCst);
            req.kick.wait_until_timeout(
                || {
                    req.done.is_done()
                        || (!idle
                            && req.queued.load(Ordering::SeqCst)
                            && (self.passthrough || self.slot_free())
                            && stuck.is_none_or(|g| self.progress.load(Ordering::SeqCst) != g))
                },
                POLL_NS.min(timeout_ns - elapsed),
            );
            req.sleeping.store(false, Ordering::SeqCst);
        }
    }

    /// A stacked disk: the caller hands its own queued requests down itself, side by side when they are reads
    /// into frames. True when it took any.
    pub(super) fn run_own(&self, own: &[Arc<Req>]) -> bool {
        let mut group: Vec<(Batch, Vec<Arc<Req>>)> = Vec::new();
        if group.try_reserve_exact(own.len()).is_err() {
            return false;
        }
        {
            let mut q = self.q.lock();
            for r in own {
                if !r.queued.load(Ordering::SeqCst) {
                    continue;
                }
                let Some(e) = q.elev.remove(r.id) else { continue };
                r.queued.store(false, Ordering::SeqCst);
                match (one(e), one(r.clone())) {
                    (Some(members), Some(reqs)) => group.push((Batch { kind: e.kind, lba: e.lba, blocks: e.blocks, prio: e.prio, members, aged: false }, reqs)),
                    _ => {
                        // No memory for two pointers: back where it was (the next look takes it).
                        let _ = q.elev.push(e).map(|()| r.queued.store(true, Ordering::SeqCst));
                    }
                }
            }
        }
        if group.is_empty() {
            return false;
        }
        self.execute_group(group);
        true
    }

    /// Dispatches in the caller until nothing queued can go (Linux `blk_mq_run_hw_queues`): for a caller without
    /// a request of its own (a test that holds the device busy, a ticket's owner that went away).
    pub fn run_queue(&self) {
        loop {
            if self.q.lock().elev.is_empty() {
                return;
            }
            if self.passthrough {
                let all: Vec<Arc<Req>> = self.q.lock().reqs.iter().filter(|r| r.queued.load(Ordering::SeqCst)).cloned().collect();
                if !self.run_own(&all) {
                    return;
                }
                continue;
            }
            if !self.take_slot() || !self.dispatch(Slots { disk: self, n: 1 }, None) {
                return;
            }
        }
    }

    /// Runs what the elevator gives the slots held. False: it gave nothing. `own`: the dispatcher's own queued
    /// request; it only runs what goes at its own class or above (B4: an Interactive waiter is never stuck in a
    /// Background device call; that one's own waiter runs it, and aging lifts it up in the end).
    fn dispatch(&self, mut slots: Slots<'_>, own: Option<u64>) -> bool {
        let Some(step) = self.next(&mut slots, own) else { return false };
        match step {
            Step::Run(group) => {
                // More queued and a slot free: the next sleeper starts beside this call.
                if self.slot_free() {
                    self.hand_off();
                }
                self.execute_group(group);
            }
            Step::NoMemory { kind, lba, blocks, members } => {
                ERRORS.add(members as u64);
                self.stats.errors.fetch_add(members as u64, Ordering::Relaxed);
                klog::event!(Warn, "block", "req.nomem", disk = self.name.clone(), op = kind.as_str(), lba = lba, blocks = blocks,
                    members = members; "no memory to build the batch: its requests fail with ENOMEM");
            }
        }
        drop(slots);
        true
    }

    fn limits(&self) -> Limits {
        let info = self.dev.info();
        Limits {
            max_blocks: (info.max_blocks_per_request as u64).max(1),
            max_discard_blocks: u64::MAX >> 1,
            age_ns: self.age_ns.load(Ordering::Relaxed),
        }
    }

    /// The next batch, and beside it more reads into frames while `slots` can grow. None: nothing can go.
    /// Every step takes at least one request out of the queue, so a dispatcher never spins on a queue it cannot
    /// serve (no memory: the request fails with ENOMEM).
    fn next(&self, slots: &mut Slots<'_>, own: Option<u64>) -> Option<Step> {
        let lim = self.limits();
        let now = now();
        let mut failed: Vec<Arc<Req>> = Vec::new();
        let mut group: Vec<(Batch, Vec<Arc<Req>>)> = Vec::new();
        let mut q = self.q.lock();
        if let Some(mine) = own.and_then(|id| q.elev.class_of(id, now, lim.age_ns)) {
            if q.elev.best_class(now, lim.age_ns, &q.inflight).is_none_or(|best| best > mine) {
                return None;
            }
        }
        loop {
            let first = group.is_empty();
            if !first && !self.take_slot() {
                break;
            }
            let picked = {
                let Queue { elev, reqs, inflight, .. } = &mut *q;
                // Beside the first batch: reads into frames of its class only (the device hears one priority for
                // the call; a Background readahead must not ride along at Interactive, mq-deadline keeps it per
                // request).
                let class = group.first().map(|(b, _)| crate::queue::class(b.prio));
                let frames = |e: &Entry| {
                    e.kind == Kind::Read
                        && class.is_some_and(|c| crate::queue::effective(e, now, lim.age_ns) == c)
                        && reqs.iter().find(|r| r.id == e.id).is_some_and(|r| matches!(&*r.data.lock(), Data::Frames(_)))
                };
                if first { elev.pick_with(now, &lim, inflight, &|_| true) } else { elev.pick_with(now, &lim, inflight, &frames) }
            };
            if !first {
                slots.n += 1;
            }
            let b = match picked {
                None => {
                    if !first {
                        // The extra slot goes back at once (`Slots` counts it, `drop` would wake too late).
                        slots.n -= 1;
                        self.active.fetch_sub(1, Ordering::SeqCst);
                    }
                    break;
                }
                Some(Err(e)) => {
                    // No memory for the elevator's working lists: the chosen request fails with ENOMEM.
                    if let Some(i) = q.reqs.iter().position(|r| r.id == e.id) {
                        let r = q.reqs.remove(i);
                        r.queued.store(false, Ordering::SeqCst);
                        let _ = failed.try_reserve(1).map(|()| failed.push(r));
                    }
                    if first {
                        drop(q);
                        for r in &failed {
                            self.finish(r, Err(BlockError::NoMemory));
                        }
                        return Some(Step::NoMemory { kind: e.kind, lba: e.lba, blocks: e.blocks, members: 1 });
                    }
                    break;
                }
                Some(Ok(b)) => b,
            };
            let mut reqs = Vec::new();
            let room = reqs.try_reserve_exact(b.members.len()).is_ok() && q.inflight.try_reserve(b.members.len()).is_ok() && group.try_reserve(1).is_ok();
            if !room {
                // No memory for a list of pointers: the members fail with ENOMEM (nothing allocates here).
                for m in &b.members {
                    if let Some(i) = q.reqs.iter().position(|r| r.id == m.id) {
                        let r = q.reqs.remove(i);
                        r.queued.store(false, Ordering::SeqCst);
                        let _ = failed.try_reserve(1).map(|()| failed.push(r));
                    }
                }
                if first {
                    drop(q);
                    for r in &failed {
                        self.finish(r, Err(BlockError::NoMemory));
                    }
                    return Some(Step::NoMemory { kind: b.kind, lba: b.lba, blocks: b.blocks, members: b.members.len() });
                }
                break;
            }
            for m in &b.members {
                if let Some(r) = q.reqs.iter().find(|r| r.id == m.id) {
                    r.queued.store(false, Ordering::SeqCst);
                    reqs.push(r.clone());
                }
                q.inflight.push(*m);
            }
            // Only reads into frames go beside the first batch.
            let more = !first || (b.kind == Kind::Read && reqs.iter().all(|r| matches!(&*r.data.lock(), Data::Frames(_))));
            group.push((b, reqs));
            if !more {
                break;
            }
        }
        drop(q);
        for r in &failed {
            ERRORS.inc();
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            self.finish(r, Err(BlockError::NoMemory));
        }
        if group.is_empty() { None } else { Some(Step::Run(group)) }
    }

    /// Host tests: the next `n` picks of the elevator find no memory.
    #[cfg(test)]
    pub(crate) fn inject_pick_oom(&self, n: u32) {
        self.q.lock().elev.oom_picks = n;
    }

    /// The frames of a batch's members as one transfer (one member: its own; more: one after another).
    fn frames_of(reqs: &[Arc<Req>]) -> Option<IoBuf> {
        let mut parts: Vec<IoBuf> = Vec::new();
        parts.try_reserve_exact(reqs.len()).ok()?;
        for r in reqs {
            match &*r.data.lock() {
                Data::Frames(b) => parts.push(b.clone()),
                _ => return None,
            }
        }
        if parts.len() == 1 {
            return parts.pop();
        }
        Some(IoBuf::new(Arc::new(Concat::new(parts)?)))
    }

    /// Runs a group: one batch as `execute`; reads into frames side by side in one `read_frames` call (on failure
    /// each batch again on its own, so an error stays with the requests that hold it).
    fn execute_group(&self, group: Vec<(Batch, Vec<Arc<Req>>)>) {
        if let [(b, reqs)] = &group[..] {
            self.execute(b, reqs);
            return;
        }
        let mut ios: Vec<FrameIo> = Vec::new();
        let ok = ios.try_reserve_exact(group.len()).is_ok()
            && group.iter().all(|(b, reqs)| match Self::frames_of(reqs) {
                Some(buf) if b.kind == Kind::Read => {
                    ios.push(FrameIo { lba: b.lba, buf });
                    true
                }
                _ => false,
            });
        if ok {
            GROUPS.inc();
            let prio = group.iter().map(|(b, _)| b.prio).min().unwrap_or_default();
            let r = {
                let _op = InFlight::new(&self.stats, group.len() as u64);
                self.dev.read_frames(&ios, prio).map_err(BlockError::from_dev)
            };
            drop(ios);
            if r.is_ok() {
                for (b, reqs) in &group {
                    self.count_dispatch(b, reqs.len());
                    for req in reqs {
                        self.done(req, Ok(()));
                    }
                }
                return;
            }
            let err = r.err().unwrap_or(BlockError::Io);
            if !alone_again(err) {
                // The device timed out, went away or cannot do it: every request of the call fails with that,
                // none is sent again (it would repeat the timeout once per batch; Linux fails each bio).
                klog::event!(Warn, "block", "req.group_failed", disk = self.name.clone(), batches = group.len(),
                    error = err.as_str(); "every batch of the call fails, none is tried again");
                for (b, reqs) in &group {
                    self.count_dispatch(b, reqs.len());
                    self.report(b.kind, b.lba, b.blocks, Err(err));
                    for req in reqs {
                        self.done(req, Err(err));
                    }
                }
                return;
            }
            klog::event!(Info, "block", "req.group_failed", disk = self.name.clone(), batches = group.len(),
                error = err.as_str(); "each batch tried alone");
        }
        for (b, reqs) in &group {
            self.execute(b, reqs);
        }
    }

    fn count_dispatch(&self, b: &Batch, members: usize) {
        DISPATCHES.inc();
        self.stats.dispatches.fetch_add(1, Ordering::Relaxed);
        if members > 1 {
            MERGED.add(members as u64 - 1);
            self.stats.merged.fetch_add(members as u64 - 1, Ordering::Relaxed);
        }
        if b.aged {
            AGED.inc();
            self.stats.aged.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn execute(&self, b: &Batch, reqs: &[Arc<Req>]) {
        self.count_dispatch(b, reqs.len());
        let r = self.run(b.kind, b.lba, b.blocks, b.prio, reqs);
        if r.is_err_and(alone_again) && reqs.len() > 1 && b.kind != Kind::Flush {
            SPLIT_RETRIES.inc();
            self.stats.merge_retries.fetch_add(1, Ordering::Relaxed);
            klog::event!(Info, "block", "req.merged_failed", disk = self.name.clone(), op = b.kind.as_str(), lba = b.lba, blocks = b.blocks,
                members = reqs.len(), error = r.err().map_or("", |e| e.as_str()); "each member tried alone");
            for req in reqs {
                let one = self.run(req.kind, req.lba, req.blocks, b.prio, core::slice::from_ref(req));
                self.report(req.kind, req.lba, req.blocks, one);
                self.done(req, one);
            }
        } else {
            self.report(b.kind, b.lba, b.blocks, r);
            for req in reqs {
                self.done(req, r);
            }
        }
    }

    pub(super) fn report(&self, kind: Kind, lba: u64, blocks: u64, r: Result<(), BlockError>) {
        if let Err(e) = r {
            ERRORS.inc();
            self.stats.errors.fetch_add(1, Ordering::Relaxed);
            self.timed_out.store(e == BlockError::Timeout, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.failed", disk = self.name.clone(), op = kind.as_str(), lba = lba, blocks = blocks, error = e.as_str());
        }
    }

    fn done(&self, req: &Arc<Req>, r: Result<(), BlockError>) {
        if r.is_ok() {
            self.note_done(req.kind);
            let bytes = req.blocks * self.block_size as u64;
            match req.kind {
                Kind::Read => self.stats.read_bytes.fetch_add(bytes, Ordering::Relaxed),
                Kind::Write { .. } => self.stats.write_bytes.fetch_add(bytes, Ordering::Relaxed),
                Kind::Discard | Kind::Flush => 0,
            };
        }
        {
            let mut q = self.q.lock();
            q.reqs.retain(|x| x.id != req.id);
            q.inflight.retain(|e| e.id != req.id);
        }
        self.progress.fetch_add(1, Ordering::SeqCst);
        self.finish(req, r);
    }

    fn finish(&self, req: &Arc<Req>, r: Result<(), BlockError>) {
        req.queued.store(false, Ordering::SeqCst);
        *req.result.lock() = Some(r);
        req.done.complete();
        req.kick.wake_all();
    }

    /// Takes a request that has not reached the device out of the queue. True when it was still queued.
    fn cancel(&self, req: &Arc<Req>) -> bool {
        let removed = {
            let mut q = self.q.lock();
            let r = q.elev.remove(req.id).is_some();
            if r {
                q.reqs.retain(|x| x.id != req.id);
            }
            let before = q.held.len();
            q.held.retain(|(e, _)| e.id != req.id);
            r || q.held.len() != before
        };
        if removed {
            TIMEOUTS.inc();
            self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
            self.timed_out.store(true, Ordering::Relaxed);
            klog::event!(Warn, "block", "req.timeout", disk = self.name.clone(), op = req.kind.as_str(), lba = req.lba, blocks = req.blocks;
                "taken out of the queue before the device got it");
            self.finish(req, Err(BlockError::Timeout));
        }
        removed
    }

    /// One device operation for `reqs` (one request, or the members of a merged batch in LBA order).
    fn run(&self, kind: Kind, lba: u64, blocks: u64, prio: IoPriority, reqs: &[Arc<Req>]) -> Result<(), BlockError> {
        let dev = |r: Result<(), DevError>| r.map_err(BlockError::from_dev);
        let _op = InFlight::new(&self.stats, 1);
        match kind {
            Kind::Flush => {
                let before = self.write_gen.load(Ordering::Acquire);
                let r = dev(self.dev.flush());
                // Clean only if no write completed beside it (several slots).
                if r.is_ok() && self.write_gen.load(Ordering::Acquire) == before {
                    self.dirty.store(false, Ordering::Release);
                }
                r
            }
            Kind::Discard => dev(self.dev.discard(lba, blocks)),
            Kind::Read | Kind::Write { .. } => {
                let (write, fua) = match kind {
                    Kind::Write { fua, preflush } => {
                        if preflush {
                            dev(self.dev.flush())?;
                        }
                        (true, fua)
                    }
                    _ => (false, false),
                };
                // Frames go down as they are (BL-2: no bounce, the device below reads straight into them).
                if let Some(buf) = Self::frames_of(reqs) {
                    return dev(if write { self.dev.write_frames(lba, &buf, fua, prio) } else { self.dev.read_frames(&[FrameIo { lba, buf }], prio) });
                }
                // The members' memory, taken out for the time of the device call.
                let mut datas: Vec<Data> = Vec::new();
                datas.try_reserve_exact(reqs.len()).map_err(|_| BlockError::NoMemory)?;
                for r in reqs {
                    datas.push(core::mem::take(&mut *r.data.lock()));
                }
                let res = self.move_data(write, fua, lba, blocks, prio, reqs, &mut datas);
                for (r, d) in reqs.iter().zip(datas) {
                    *r.data.lock() = d;
                }
                res
            }
        }
    }

    /// Byte requests: one plain buffer straight through; a merged batch of byte buffers (or bytes beside frames)
    /// through a bounce buffer of at least `CHUNK_BYTES` (`block.copy_bytes.bounce`).
    #[allow(clippy::too_many_arguments)]
    fn move_data(&self, write: bool, fua: bool, lba: u64, blocks: u64, prio: IoPriority, reqs: &[Arc<Req>], datas: &mut [Data]) -> Result<(), BlockError> {
        let bs = self.block_size as u64;
        if let [Data::Bytes(b)] = datas {
            let r = if write { self.dev.write_blocks(lba, b, fua, prio) } else { self.dev.read_blocks(lba, b, prio) };
            return r.map_err(BlockError::from_dev);
        }
        // The bounce buffer is `CHUNK_BYTES` (the device splits a larger transfer itself) and halves while the heap
        // cannot give it (FS-STALL round 4: it was the device's whole transfer when that was larger, 16 MB and more
        // in one piece; on a machine whose free memory is in
        // pieces every write-back of the disk's cache then failed with ENOMEM and its dirty pages could never go).
        let mut chunk = (CHUNK_BYTES / bs).max(1).min(blocks.max(1));
        let mut bounce: Vec<u8> = Vec::new();
        loop {
            let cap = usize::try_from(chunk * bs).map_err(|_| BlockError::NoMemory)?;
            if bounce.try_reserve_exact(cap).is_ok() {
                break;
            }
            if chunk == 1 {
                return Err(BlockError::NoMemory);
            }
            chunk /= 2;
            BOUNCE_SHRUNK.inc();
        }
        let end = lba + blocks;
        let mut at = lba;
        while at < end {
            let n = chunk.min(end - at);
            let len = (n * bs) as usize;
            bounce.clear();
            bounce.resize(len, 0);
            let overlap = |r: &Req| -> Option<(u64, u64)> {
                let (lo, hi) = (r.lba.max(at), r.end().min(at + n));
                (lo < hi).then_some((lo, hi))
            };
            if write {
                for (r, d) in reqs.iter().zip(datas.iter()) {
                    if let Some((lo, hi)) = overlap(r) {
                        d.copy_out(((lo - r.lba) * bs) as usize, &mut bounce[((lo - at) * bs) as usize..((hi - at) * bs) as usize]);
                    }
                }
                self.dev.write_blocks(at, &bounce, fua, prio).map_err(BlockError::from_dev)?;
            } else {
                self.dev.read_blocks(at, &mut bounce, prio).map_err(BlockError::from_dev)?;
                for (r, d) in reqs.iter().zip(datas.iter_mut()) {
                    if let Some((lo, hi)) = overlap(r) {
                        d.copy_in(((lo - r.lba) * bs) as usize, &bounce[((lo - at) * bs) as usize..((hi - at) * bs) as usize]);
                        COPY_BOUNCE.add((hi - lo) * bs);
                        self.stats.copied.fetch_add((hi - lo) * bs, Ordering::Relaxed);
                    }
                }
            }
            at += n;
        }
        Ok(())
    }

    /// Ends the disk: queued requests fail with `Gone`, new ones are refused. Requests the device has already
    /// finish normally.
    pub fn stop(&self) {
        {
            let mut q = self.q.lock();
            if q.stopped {
                return;
            }
            q.stopped = true;
        }
        self.fail_queued();
        let held = core::mem::take(&mut self.q.lock().held);
        for (_, r) in &held {
            self.finish(r, Err(BlockError::Gone));
        }
    }

    fn fail_queued(&self) -> usize {
        let failed: Vec<Arc<Req>> = {
            let mut q = self.q.lock();
            let ids: Vec<u64> = q.elev.entries().iter().map(|e| e.id).collect();
            let mut out = Vec::new();
            for id in ids {
                q.elev.remove(id);
                if let Some(i) = q.reqs.iter().position(|r| r.id == id) {
                    out.push(q.reqs.remove(i));
                }
            }
            out
        };
        for r in &failed {
            self.finish(r, Err(BlockError::Gone));
        }
        failed.len()
    }

    // ---- RB-14: the storage stop before a reset (`crate::shutdown`)

    /// What a completed request leaves for the stop: a write makes the device cache dirty (a FUA write is durable
    /// itself but leaves the others as they were); a flush cleans it in `run` (unless a write completed beside it).
    fn note_done(&self, kind: Kind) {
        if let Kind::Write { fua, .. } = kind {
            self.written.store(true, Ordering::Release);
            self.last_write_ns.store(now(), Ordering::Release);
            if !fua {
                self.write_gen.fetch_add(1, Ordering::AcqRel);
                self.dirty.store(true, Ordering::Release);
            }
        }
    }

    /// Holds new requests back from now on (they wait for `reopen`); what is queued still runs. Returns
    /// (queued, at the device).
    pub fn close(&self) -> (usize, usize) {
        let mut q = self.q.lock();
        q.closed = true;
        let queued = q.elev.entries().len();
        (queued, q.reqs.len().saturating_sub(queued) + q.direct)
    }

    /// Takes requests again (a Reload that did not happen, ktests): the held ones are queued in their order and
    /// their waiters woken (they dispatch them).
    pub fn reopen(&self) {
        let mut lost: Vec<Arc<Req>> = Vec::new();
        let mut back: Vec<Arc<Req>> = Vec::new();
        {
            let mut q = self.q.lock();
            q.closed = false;
            let held = core::mem::take(&mut q.held);
            let _ = back.try_reserve_exact(held.len());
            for (e, r) in held {
                if q.reqs.try_reserve(1).is_err() || q.elev.push(e).is_err() {
                    let _ = lost.try_reserve(1).map(|()| lost.push(r));
                    continue;
                }
                r.queued.store(true, Ordering::SeqCst);
                q.reqs.push(r.clone());
                if back.len() < back.capacity() {
                    back.push(r);
                }
            }
        }
        for r in &lost {
            self.finish(r, Err(BlockError::NoMemory));
        }
        for r in &back {
            r.kick.wake_all();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.q.lock().closed
    }

    /// (queued, at the device): requests not answered yet (without the ones held back by `close`).
    pub fn pending(&self) -> (usize, usize) {
        let q = self.q.lock();
        let queued = q.elev.entries().len();
        (queued, q.reqs.len().saturating_sub(queued) + q.direct)
    }

    /// Fails what is still queued (not at the device, not held) with `Gone`; returns how many.
    pub fn fail_waiting(&self) -> usize {
        self.fail_queued()
    }

    /// (a write completed since boot, a write completed after the last flush, when the last write completed).
    pub fn write_state(&self) -> (bool, bool, u64) {
        (self.written.load(Ordering::Acquire), self.dirty.load(Ordering::Acquire), self.last_write_ns.load(Ordering::Acquire))
    }

    /// Requests held back by `close`.
    pub fn held(&self) -> usize {
        self.q.lock().held.len()
    }

    /// The stop's cache flush (Linux `sd_shutdown` -> `sd_sync_cache`): straight to the device, past the closed
    /// queue. Only after `pending` read (0, 0): nothing of this disk is at the device then.
    pub fn shutdown_flush(&self) -> Result<(), BlockError> {
        let r = self.dev.flush().map_err(BlockError::from_dev);
        if r.is_ok() {
            self.dirty.store(false, Ordering::Release);
        }
        r
    }
}
