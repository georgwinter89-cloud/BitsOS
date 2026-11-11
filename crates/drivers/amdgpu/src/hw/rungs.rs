//! GX-9a: the steps of GR-3's ladder on the chain: `lost::hung` walks, this does.
//!
//! The order is Linux's (amdgpu_job.c:34): the job's waves killed (amdgpu_ring.c:425; only GFX has it, gfx_v11_0.c:6128), then
//! what 6.6 does not have and 7.1 does, the queue reset through MES (`vmrecover`), then the full reset. BitsOS's full reset is
//! GC's bring-up again from the PSP, which is the stop and the bring-up the tool runs; mode 2 comes behind it, closed unless
//! CONFIG `gx_mode2=on` (GX-D9: what it does to DCN is unmeasured). Every step ends: an inline one by its own bound, a full
//! reset's by `ResetDomain::expire` on the job thread, which gives up on a thread that does not answer.
//!
//! The kill and the queue steps run on the job thread: at worst MES's 2.1 s per frame, with the owner's lock taken as for any
//! other step. The full reset's steps replace the owner, for seconds, on a thread of their own (`Long`): the job thread goes
//! on looking at the clock, nothing else touches the chain until the answer is in.

use super::bo::VramBo;
use super::chain::{Chain, Rebuilt, Reinit};
use super::frame::RingType;
use super::jobs::Rung;
use super::run::{Ended, Inner, finish, let_go, now_us};
use crate::gpucore::gmc;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::recover::{Action, Next, Reply, Step};
use core::sync::atomic::{AtomicBool, Ordering};
use drm::dma_buf::Exporter;
use ksync::SpinLock;

/// A pattern for the page `Magic` keeps. Its only job is to read back the same.
const MAGIC: [u8; 32] = *b"BitsOS reset magic: VRAM holds.\n";

/// amdgpu_device_fill_reset_magic (amdgpu_device.c:2489): memory written before the reset still reads the same after it.
/// Linux reads the GART table, which BitsOS's bring-up writes again, so a page of its own, for this recovery alone.
pub(super) struct Magic(Arc<VramBo>);

impl Magic {
    fn holds(&self) -> bool {
        let mut b = [0u8; MAGIC.len()];
        self.0.read(0, &mut b) && b == MAGIC
    }
}

/// What a full reset's step answered.
pub(super) struct Answer {
    reply: LongReply,
    lines: Vec<String>,
}

enum LongReply {
    Suspended(bool),
    Mode2(Option<bool>),
    Reinit(Option<Reinit>),
}

/// The step that runs on the reset thread, and where its answer lands.
pub(super) struct Long {
    ring: usize,
    step: Step,
    slot: Slot,
}

/// Where a reset thread's answer lands. `abandoned`: the clock gave up on it first, so nobody reads the answer.
pub(super) struct LongSlot {
    answer: SpinLock<Option<Answer>>,
    abandoned: AtomicBool,
}

pub(super) type Slot = Arc<LongSlot>;

impl LongSlot {
    fn new() -> Slot {
        Arc::new(LongSlot { answer: SpinLock::named("amdgpu.hw.reset_answer", None), abandoned: AtomicBool::new(false) })
    }

    /// Whichever of the two comes second sees the other: the step's answer and the clock's giving up.
    fn say_late(&self, action: Action) {
        let passed = self.answer.lock().as_ref().map(Answer::passed);
        if let Some(passed) = passed {
            klog::event!(Error, "gx", "hw.reset_late_answer", action = format!("{action:?}").as_str(), passed = passed; "the ladder went on without this step: its answer means nothing");
        }
    }
}

impl Answer {
    fn passed(&self) -> bool {
        match &self.reply {
            LongReply::Suspended(ok) => *ok,
            LongReply::Mode2(a) => a.is_some(),
            LongReply::Reinit(a) => a.is_some(),
        }
    }
}

fn run_long(chain: &dyn Chain, action: Action) -> Answer {
    let mut lines = Vec::new();
    let reply = match action {
        Action::SuspendGc => LongReply::Suspended(chain.suspend_gc(&mut lines)),
        Action::Mode2Reset => LongReply::Mode2(chain.mode2_reset(&mut lines)),
        _ => LongReply::Reinit(chain.reinit_gc(&mut lines)),
    };
    Answer { reply, lines }
}

impl Inner {
    /// Walks the ladder from `next` until a full reset's step goes to its thread, or the ladder's end.
    pub(super) fn walk(&mut self, i: usize, mut next: Next) {
        loop {
            match next {
                Next::Done(f) => {
                    self.apply(i, f);
                    return;
                }
                Next::Do(step) => {
                    self.publish();
                    let reply = match step.action {
                        a @ (Action::SuspendGc | Action::Mode2Reset | Action::ReinitGc) => {
                            if self.rung_allowed(a) && self.start_long(i, step) {
                                return;
                            }
                            Reply::Failed
                        }
                        a => self.step(i, a, step.deadline_us),
                    };
                    match self.domain.reply(reply, now_us()) {
                        Ok(n) => next = n,
                        Err(e) => {
                            self.give_up(i, format!("{e:?}").as_str());
                            return;
                        }
                    }
                }
            }
        }
    }

    /// The counters as they stand: a waiter, a CS or QUERY_STATE2 asks while a recovery runs (Linux reads them live). Before
    /// any fence of the step that follows.
    fn publish(&self) {
        let c = self.domain.counters();
        self.shared.reset_counter.store(c.reset, Ordering::Release);
        self.shared.vram_lost.store(c.vram_lost, Ordering::Release);
        self.shared.in_reset.store(c.in_reset, Ordering::Release);
    }

    /// The tool's `until=ladder`: rungs below the one the desk wants answer `Failed`, so it can reach each on the chip.
    fn rung_allowed(&mut self, a: Action) -> bool {
        let from = self.shared.ladder_from.load(Ordering::Acquire);
        let rung = match a {
            Action::KillWaves { .. } => Rung::Soft,
            Action::Unmap { .. } | Action::ResetQueue { .. } | Action::Map { .. } | Action::RestartSdma { .. } => Rung::Queue,
            Action::SuspendGc | Action::Mode2Reset => Rung::Mode2,
            Action::ReinitGc if self.mode2_rung => Rung::Mode2,
            Action::ReinitGc => Rung::Gc,
            Action::Blame { .. } | Action::TestRing { .. } => return true,
        };
        if (rung as u8) < from {
            klog::event!(Warn, "gx", "hw.rung_skipped", rung = rung.name(); "until=ladder: this rung answers Failed");
            return false;
        }
        true
    }

    /// One short step; `lost` holds what is a report, this what touches the queue.
    pub(super) fn step(&mut self, i: usize, a: Action, deadline_us: u64) -> Reply {
        let allowed = self.rung_allowed(a);
        match a {
            Action::Blame { entity, .. } => {
                let now = self.domain.counters().reset;
                let j = self.rings.iter().flat_map(|r| r.jobs.iter()).map(|on| &on.hold.sub.jobs[on.member]).find(|j| j.entity.id() == entity);
                if let Some(j) = j {
                    j.entity.blame(now);
                }
                Reply::Ok
            }
            Action::KillWaves { .. } if !allowed => Reply::Failed,
            Action::KillWaves { vmid, seq, errno, .. } => self.kill_waves(i, vmid, seq, errno, deadline_us),
            _ if !allowed => Reply::Failed,
            Action::Unmap { .. } => self.rebuild_queue(i),
            Action::RestartSdma { .. } => self.restart_sdma(i),
            // `rebuild_queue` did unmap, reset and map in one go: the first answers for all three.
            Action::ResetQueue { .. } | Action::Map { .. } if self.fresh[i].is_some() => Reply::Ok,
            Action::TestRing { ring, fence_value } => match self.rings.iter().position(|r| r.id == ring) {
                Some(k) if self.fresh[k].is_some() => self.test_fresh(k, fence_value),
                _ => Reply::Failed,
            },
            _ => Reply::Failed,
        }
    }

    /// amdgpu_ring_soft_recovery: the kill repeated while the fence is watched, until the job ended or the deadline. The fence
    /// decides, not this answer. What ends here ends through the ring's own progress; the job that was hung with `errno`.
    fn kill_waves(&mut self, i: usize, vmid: u8, seq: u64, errno: i32, deadline_us: u64) -> Reply {
        let Some(a) = self.vmi.addrs.hqd else { return Reply::Failed };
        let id = self.rings[i].id;
        let mut ended: Vec<Ended> = Vec::new();
        let mut kills = 0u32;
        loop {
            let rptr = self.domain.ring(id).map_or(0, |w| self.rings[i].queue.kind().hw_ptr(w.rptr()));
            let (mut wrote, mut sample) = (false, None);
            let r = &self.rings[i];
            self.chain.with(&mut |o| {
                wrote = o.kill_waves(&a, vmid);
                sample = r.sample(o, rptr);
                o.io().delay_us(10);
            });
            kills += 1;
            if !wrote {
                klog::event!(Warn, "gx", "hw.kill_not_written", ring = self.rings[i].ty.name(), vmid = vmid);
            }
            if let Some(p) = sample.and_then(|s| self.domain.poll(id, now_us(), s)) {
                self.note_polled(i, &p, &mut ended);
            }
            let done = self.domain.ring(id).is_some_and(|w| w.timeline().is_signalled(seq));
            if done || !wrote || now_us() >= deadline_us {
                klog::event!(Info, "gx", "hw.kill_waves", ring = self.rings[i].ty.name(), vmid = vmid, kills = kills, ended = done);
                break;
            }
        }
        let mut gone = Vec::new();
        for mut e in ended {
            if e.on.seq == seq {
                e.errno = Some(errno);
            }
            gone.push(finish(e, &self.shared));
        }
        let_go(gone);
        Reply::Ok
    }

    fn rebuild_queue(&mut self, i: usize) -> Reply {
        let vmi = self.vmi;
        let ty = self.rings[i].ty;
        let mut out: Option<Rebuilt> = None;
        self.chain.with(&mut |o| {
            out = Some(match ty {
                RingType::Gfx => o.rebuild_gfx(&vmi),
                _ => o.rebuild_compute(&vmi),
            })
        });
        self.took(i, out)
    }

    fn restart_sdma(&mut self, i: usize) -> Reply {
        let vmi = self.vmi;
        let mut out: Option<Rebuilt> = None;
        self.chain.with(&mut |o| out = Some(o.restart_sdma(&vmi)));
        self.took(i, out)
    }

    fn took(&mut self, i: usize, out: Option<Rebuilt>) -> Reply {
        let Some(out) = out else { return Reply::Failed };
        let ty = self.rings[i].ty.name();
        klog::event!(Info, "gx", "hw.queue_rebuilt", ring = ty, detail = out.detail.as_str(), stranded = out.stranded);
        if out.stranded {
            self.chain.note_stranded();
        }
        match out.queue {
            Ok(q) => {
                self.fresh[i] = Some(q);
                Reply::Ok
            }
            Err(why) => {
                klog::event!(Error, "gx", "hw.queue_rebuild_failed", ring = ty, why = why);
                Reply::Failed
            }
        }
    }

    fn test_fresh(&mut self, i: usize, fence_value: u64) -> Reply {
        let Some(q) = self.fresh[i].take() else { return Reply::Failed };
        let us = self.domain.policy().test_us;
        let r = &mut self.rings[i];
        r.queue = q;
        let mut out = None;
        self.chain.with(&mut |o| out = Some(r.test(o, fence_value, us)));
        let hub = self.vmi.addrs.hub;
        let (a, flush) = (self.vmi.addrs, self.vmi.flush_us);
        // A status the first clear left (the stalled client kept it set) goes now that the client is gone.
        self.chain.with(&mut |o| {
            if gmc::read_fault(o.io(), &hub).status.is_some_and(|s| s != 0 && s != u32::MAX) {
                let _ = o.clear_fault(&a, flush);
            }
        });
        match out {
            Some(Ok(ptr)) => Reply::Ring { ptr },
            Some(Err(why)) => {
                klog::event!(Error, "gx", "hw.ring_test_failed", ring = self.rings[i].ty.name(), why = why.as_str());
                Reply::Failed
            }
            None => Reply::Failed,
        }
    }

    /// The page the full reset's end compares, written now; None says so (a full reset without it reads as intact: the
    /// carve-out is system memory, and the page is a check, not the reason to give up).
    fn fill_magic(&mut self) {
        if self.magic.is_some() {
            return;
        }
        let m = VramBo::new(&self.heaps, 1, 1);
        self.magic = match m {
            Ok(b) if b.write(0, &MAGIC) => Some(Magic(b)),
            other => {
                klog::event!(Warn, "gx", "hw.reset_magic_none", errno = other.err().unwrap_or(0); "no page for the reset magic: this reset proves nothing about VRAM");
                None
            }
        };
    }

    /// A full reset's step on its thread. False: not started (an earlier step's thread never ended and may hold the chain).
    fn start_long(&mut self, i: usize, step: Step) -> bool {
        self.mode2_rung |= matches!(step.action, Action::SuspendGc | Action::Mode2Reset);
        if self.stale_long.as_ref().is_some_and(|s| s.answer.lock().is_none()) {
            klog::event!(Error, "gx", "hw.reset_thread_stuck"; "an earlier step of this reset has not ended: no new one starts");
            return false;
        }
        self.stale_long = None;
        self.fill_magic();
        // Past the check above no thread of an earlier step runs, so nothing else reads this.
        self.chain.reset_abandoned(false);
        let slot = LongSlot::new();
        let long = Long { ring: i, step, slot: slot.clone() };
        let (chain, wake, action) = (self.chain.clone(), self.shared.clone(), step.action);
        let work = move || {
            let a = run_long(&*chain, action);
            *slot.answer.lock() = Some(a);
            if slot.abandoned.load(Ordering::Acquire) {
                slot.say_late(action);
            }
            wake.kick_thread();
        };
        #[cfg(test)]
        work();
        #[cfg(not(test))]
        if let Err(e) = sched::spawn("amdgpu-reset", sched::Class::Normal, work) {
            klog::event!(Error, "gx", "hw.reset_no_thread", error = format!("{e:?}").as_str());
            return false;
        }
        self.long = Some(long);
        true
    }

    /// The thread's answer, or the clock's: `ResetDomain::expire` gives up on a step nobody answered. True when the ladder moved.
    pub(super) fn long_round(&mut self, now: u64) -> bool {
        let Some(long) = self.long.as_ref() else { return false };
        let ring = long.ring;
        let answer = long.slot.answer.lock().take();
        let Some(answer) = answer else {
            let Some(next) = self.domain.expire(now) else { return false };
            let action = long.step.action;
            klog::event!(Error, "gx", "hw.reset_step_expired", action = format!("{action:?}").as_str(); "the thread did not answer in time: the ladder moves on without it");
            // Before the walk: a step that still runs ends at its next look, and starts no stage on a chain the ladder has
            // given up on. It keeps the rungs out while it runs (`start_long`), so no full reset can pass beside it.
            self.chain.reset_abandoned(true);
            self.stale_long = self.long.take().map(|l| l.slot);
            if let Some(s) = &self.stale_long {
                s.abandoned.store(true, Ordering::Release);
                s.say_late(action);
            }
            self.walk(ring, next);
            return true;
        };
        let Some(long) = self.long.take() else { return false };
        klog::event!(Warn, "gx", "hw.reset_step", action = format!("{:?}", long.step.action).as_str(), passed = answer.passed(), lines = answer.lines.len());
        self.reset_lines.extend(answer.lines);
        // `waiting`'s contract. The clock's way above takes the step along, so this cannot differ; if it ever does, no answer
        // of this thread means anything and nobody else would drive the ladder.
        if self.domain.waiting() != Some(long.step) {
            self.give_up(ring, "a reset thread answered a step that is not the current one");
            return true;
        }
        let reply = self.reply_of(answer.reply);
        match self.domain.reply(reply, now_us()) {
            Ok(next) => self.walk(ring, next),
            Err(e) => self.give_up(ring, format!("{e:?}").as_str()),
        }
        true
    }

    fn reply_of(&mut self, r: LongReply) -> Reply {
        let vram_intact = self.magic.as_ref().is_none_or(Magic::holds);
        match r {
            LongReply::Suspended(true) => Reply::Ok,
            LongReply::Suspended(false) | LongReply::Mode2(None) | LongReply::Reinit(None) => Reply::Failed,
            LongReply::Mode2(Some(display_equal)) => Reply::Reset { vram_intact, display_equal },
            LongReply::Reinit(Some(n)) => {
                let display_equal = n.display_equal;
                self.take_reinit(n);
                Reply::Reset { vram_intact, display_equal }
            }
        }
    }

    /// The chain came back: its queues for the ring tests, its addresses for the fences, and no VMID holds a flush any more.
    fn take_reinit(&mut self, n: Reinit) {
        self.vmi = n.vmi;
        for (k, q) in n.queues.into_iter().enumerate() {
            self.rings[k].fence = super::rings::fence_slot(&self.vmi, self.rings[k].ty);
            self.fresh[k] = Some(q);
        }
        self.ids.reset_all();
        self.forget_flushes();
    }
}
