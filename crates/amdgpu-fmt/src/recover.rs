//! The recovery ladder: what to do about a hung ring, as data. No hardware access: the driver performs each
//! [`Action`] and answers with a [`Reply`], so the fakes and the chip run the same decisions (GX-8, GX-9a).
//!
//! Linux tries soft recovery, then a full reset (amdgpu_job.c:34, amdgpu_device.c:5160). BitsOS puts a queue reset
//! between them, so a hung process need not stop the others, and its full reset is GC's re-init from the PSP, which
//! kept the picture in every run on the chip. Mode 2 is Linux's method for MP1 13.0.4 (soc21.c:365) but stays
//! refused until GX-9a has measured what it does to DCN (GX-D9). Mode 1 and BACO are no actions at all. When every
//! allowed rung failed the device is dead until a reboot; the picture stays, and programs get their errors.
//!
//! Every wait has a deadline and the ladder only moves up, so a recovery ends in a bounded number of steps (B4).
//! The driver gives up on a step at its deadline and answers `Reply::Failed`; if no answer comes at all (a wedged
//! wait, a bug in the glue), [`ResetDomain::expire`], called from the same poll loop, gives up for it.
//!
//! `Policy.recovery` off is Linux's `amdgpu.gpu_recovery=0` (amdgpu_job.c:54,70): no kill, no reset, the ring stays
//! as it hung for reading. Linux's job then never signals; here it ends, because every job must in bounded time (B4).

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use crate::errno;
use crate::fence::FenceError;
use crate::told::{Counters, Fate, JobFate};
use crate::watch::{
    Cause, HangReport, IbWords, JobMeta, Owner, Pending, Polled, QueueReset, RingCfg, RingId, RingState, RingWatch, Sample,
};

/// Why mode 2 is refused by default.
pub const MODE2_UNMEASURED: &str = "mode 2 is refused on SMU 13.0.4 until GX-9a has measured what it does to the display engine (GX-D9)";
/// Why it is refused for good in this boot.
pub const MODE2_MOVED_PICTURE: &str = "mode 2 is refused for this boot: a reset changed the display engine's state (GX-D9)";
/// Why GC's re-init was skipped: a reset moved the picture earlier in this boot.
pub const REINIT_MOVED_PICTURE: &str = "GC re-init is refused for this boot: a reset changed the display engine's state (GX-D9)";
/// Why GC's re-init was skipped: too many in the window.
pub const REINIT_LOOP: &str = "GC re-init is refused: the reset limit of its time window is used up";
/// Why it can never open on this chip.
pub const MODE2_WRONG_CHIP: &str = "this chip's Linux reset method is mode 1 or BACO, which BitsOS never uses";

/// An IP version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpVersion(pub u8, pub u8, pub u8);

/// Mode 2 is Linux's method only on these MP1 versions (soc21.c:377-380).
pub fn linux_uses_mode2(mp1: IpVersion) -> bool {
    matches!(mp1, IpVersion(13, 0, 4) | IpVersion(13, 0, 11))
}

/// Whether mode 2 may run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode2Gate {
    Refused(&'static str),
    /// `proof` names the measurement (GX-9a's run).
    Measured { proof: &'static str },
}

/// How many re-inits or mode-2 resets a time window may hold: a program restarting into the same hang must not
/// keep GC in a reset loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReinitLimit {
    pub count: usize,
    pub window_us: u64,
}

/// The ladder's settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// false: a hung ring only gets its report and is frozen (`Outcome::RecoveryOff`). Linux's `gpu_recovery`.
    pub recovery: bool,
    pub mode2: Mode2Gate,
    pub reinit_limit: ReinitLimit,
    /// Linux's 10 ms (amdgpu_ring.c:438).
    pub kill_us: u64,
    pub queue_us: u64,
    pub test_us: u64,
    pub reinit_us: u64,
    pub mode2_us: u64,
    /// How long after a step's deadline [`ResetDomain::expire`] still waits for the driver's own answer.
    pub expire_slack_us: u64,
}

impl Policy {
    /// The default for a chip: mode 2 closed. `queue_us` covers MES's API wait of 2.1 s (ipmod/mes11.rs), `reinit_us`
    /// a bring-up that took 0.7 s on the chip with room for its SMU and PSP waits.
    pub fn for_mp1(mp1: IpVersion) -> Policy {
        Policy {
            recovery: true,
            mode2: Mode2Gate::Refused(if linux_uses_mode2(mp1) { MODE2_UNMEASURED } else { MODE2_WRONG_CHIP }),
            reinit_limit: ReinitLimit { count: 3, window_us: 60_000_000 },
            kill_us: 10_000,
            queue_us: 2_500_000,
            test_us: 1_000_000,
            reinit_us: 10_000_000,
            mode2_us: 5_000_000,
            expire_slack_us: 1_000_000,
        }
    }
    /// Opens mode 2 after GX-9a's measurement. Refuses a chip whose Linux method is not mode 2.
    pub fn open_mode2(&mut self, mp1: IpVersion, proof: &'static str) -> Result<(), &'static str> {
        if !linux_uses_mode2(mp1) {
            return Err(MODE2_WRONG_CHIP);
        }
        self.mode2 = Mode2Gate::Measured { proof };
        Ok(())
    }
}

/// How far a recovery had to go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The job's waves were killed; the ring went on.
    Soft,
    /// One queue was reset.
    Queue,
    /// GC ran its bring-up again.
    Gc,
    /// Mode 2, then the bring-up.
    Mode2,
}

/// One step for the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Set the job's error to `errno` (ENODATA) and repeat the SQ_CMD kill for `vmid` (gfx_v11_0.c:5693), polling
    /// the ring, until `seq` signals or the deadline passes. The fence decides, not the reply.
    KillWaves { ring: RingId, vmid: u8, seq: u64, errno: i32 },
    /// Set the context's guilty flag now, with [`Counters::blame`]: a new submission must fail before the reset ends.
    Blame { ctx: u64, entity: u64 },
    /// MES REMOVE_QUEUE of the ring's queue. A process queue uses `mesapi::remove_process_queue`.
    Unmap { ring: RingId, queue: QueueReset },
    /// Rebuild the queue's state with its pointers and fence slot.
    ResetQueue { ring: RingId, queue: QueueReset },
    /// Map the queue again (also a process queue after GC's re-init).
    Map { ring: RingId, queue: QueueReset },
    /// Stop and start an SDMA ring.
    RestartSdma { ring: RingId },
    /// Linux's IP suspend before mode 2 (see the driver's `MODE2_REFUSED`).
    SuspendGc,
    /// GfxDeviceDriverReset(2). Only with [`Mode2Gate::Measured`].
    Mode2Reset,
    /// Run GC's bring-up again (`again: true`). Answer with `Reply::Reset`.
    ReinitGc,
    /// Write `fence_value` to the ring's fence slot (amdgpu_fence_write in force_completion), test the ring and
    /// answer with `Reply::Ring`.
    TestRing { ring: RingId, fence_value: u64 },
}

/// The driver's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    Ok,
    /// Timed out or refused.
    Failed,
    /// A ring test passed and both pointers stand at `ptr` (dword counter). The hardware decides that value: SDMA
    /// keeps its pointers through a restart (crates/drivers/amdgpu/src/gpucore/sdmafake.rs).
    Ring { ptr: u64 },
    /// GC is back. `vram_intact`: the reset magic held (amdgpu_device.c:2489). `display_equal`: the DCN snapshot
    /// taken before the recovery still matches.
    Reset { vram_intact: bool, display_equal: bool },
}

/// A step with the time by which the driver must have given up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub action: Action,
    pub deadline_us: u64,
}

/// How a recovery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Recovered(Scope),
    /// Nothing more is allowed: reboot. `mode2` says why that rung was not there, if it was not.
    NeedsReboot { mode2: Option<&'static str> },
    /// `Policy.recovery` is off: the ring's jobs ended and it stays frozen until a reboot. The other rings go on,
    /// nobody is blamed and no counter moved.
    RecoveryOff,
}

/// The end of a recovery: everything the caller still has to tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finish {
    pub outcome: Outcome,
    pub ring: RingId,
    pub seq: u64,
    /// Jobs taken off rings, each exactly once. Jobs not listed end through `Polled::advanced`.
    pub fates: Vec<JobFate>,
    /// Contexts to mark guilty (the same ones `Action::Blame` named).
    pub guilty: Vec<u64>,
    pub counters: Counters,
    pub vram_lost: bool,
    /// Why the re-init rung was skipped, if it was.
    pub reinit_refused: Option<&'static str>,
    /// GX-D9 was broken: log it. Re-init and mode 2 are closed for this boot.
    pub display_changed: bool,
    /// Replies that came after their deadline.
    pub overruns: u32,
    /// Steps nobody answered, given up by [`ResetDomain::expire`]. Not 0 means a bug in the driver's glue: log it.
    pub expired: u32,
}

/// What to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Do(Step),
    Done(Finish),
}

/// Why a call was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverError {
    NoRing,
    /// The ring moved since the report, or was never hung.
    NotHung,
    /// Another recovery runs, or a reply came with none running.
    Busy,
    Idle,
    Dead,
    /// The ring's configuration is invalid (see [`RingWatch::new`]).
    Invalid,
}

/// Why a submission was refused. The scheduler holds a job while the answer is EBUSY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    NoRing,
    Full(FenceError),
    Recovering,
    Dead,
}

impl SubmitError {
    pub fn errno(&self) -> i32 {
        match self {
            SubmitError::NoRing => errno::EINVAL,
            SubmitError::Full(e) => e.errno(),
            SubmitError::Recovering => errno::EBUSY,
            SubmitError::Dead => errno::ENODEV,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rung {
    Soft,
    Queue,
    Gc,
    Mode2,
}

#[derive(Debug)]
struct Recovery {
    ring: RingId,
    bad: Pending,
    rungs: VecDeque<Rung>,
    /// The rung whose actions are running.
    open: Option<Scope>,
    todo: VecDeque<Action>,
    waiting: Action,
    deadline_us: u64,
    fates: Vec<JobFate>,
    guilty: Vec<u64>,
    blamed: bool,
    full_counted: bool,
    vram_lost: bool,
    reinit_refused: Option<&'static str>,
    display_changed: bool,
    overruns: u32,
    expired: u32,
}

/// All rings that one GPU reset can touch, with the device's counters.
#[derive(Debug)]
pub struct ResetDomain {
    policy: Policy,
    rings: BTreeMap<RingId, RingWatch>,
    next_id: u32,
    counters: Counters,
    rec: Option<Recovery>,
    dead: bool,
    /// Rings a hang left alone with `Policy.recovery` off.
    frozen: BTreeSet<RingId>,
    /// A reset changed the picture: no more of them.
    gc_closed: bool,
    reinits: VecDeque<u64>,
}

impl ResetDomain {
    pub fn new(policy: Policy) -> ResetDomain {
        ResetDomain {
            policy,
            rings: BTreeMap::new(),
            next_id: 0,
            counters: Counters::default(),
            rec: None,
            dead: false,
            frozen: BTreeSet::new(),
            gc_closed: false,
            reinits: VecDeque::new(),
        }
    }
    pub fn counters(&self) -> Counters {
        self.counters
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn is_dead(&self) -> bool {
        self.dead
    }
    pub fn ring(&self, id: RingId) -> Option<&RingWatch> {
        self.rings.get(&id)
    }
    /// Refused during a recovery (`Busy`, the caller tries again): the ring would miss the recovery's map and test
    /// steps. An id still in use after the counter wrapped is skipped: an insert would drop that ring's jobs.
    pub fn add_ring(&mut self, cfg: RingCfg) -> Result<RingId, RecoverError> {
        if self.dead {
            return Err(RecoverError::Dead);
        }
        if self.rec.is_some() {
            return Err(RecoverError::Busy);
        }
        let mut id = self.next_id;
        while self.rings.contains_key(&RingId(id)) {
            id = id.wrapping_add(1);
        }
        let watch = RingWatch::new(RingId(id), cfg).ok_or(RecoverError::Invalid)?;
        self.next_id = id.wrapping_add(1);
        self.rings.insert(RingId(id), watch);
        Ok(RingId(id))
    }
    #[cfg(test)]
    pub(crate) fn set_next_id(&mut self, id: u32) {
        self.next_id = id;
    }
    /// Removes an idle ring (a process queue that ended). Refused during a recovery or with jobs on it.
    pub fn remove_ring(&mut self, id: RingId) -> Result<(), RecoverError> {
        if self.rec.is_some() {
            return Err(RecoverError::Busy);
        }
        let w = self.rings.get(&id).ok_or(RecoverError::NoRing)?;
        if w.jobs().next().is_some() {
            return Err(RecoverError::Busy);
        }
        self.rings.remove(&id);
        self.frozen.remove(&id);
        Ok(())
    }

    fn ring_blocked(&self, id: RingId) -> Option<RingState> {
        if self.dead || self.frozen.contains(&id) {
            return Some(RingState::Dead);
        }
        if self.counters.in_reset {
            return Some(RingState::InReset);
        }
        let r = self.rec.as_ref()?;
        (r.ring == id).then_some(RingState::Recovering)
    }

    /// A job whose frame was written to `start..end` on ring `id`; returns its fence value.
    pub fn submit(&mut self, id: RingId, meta: JobMeta, start: u64, end: u64, now_us: u64) -> Result<u64, SubmitError> {
        match self.ring_blocked(id) {
            Some(RingState::Dead) => return Err(SubmitError::Dead),
            Some(_) => return Err(SubmitError::Recovering),
            None => {}
        }
        let w = self.rings.get_mut(&id).ok_or(SubmitError::NoRing)?;
        w.submit(meta, start, end, now_us).map_err(SubmitError::Full)
    }

    /// Takes back the last job of a ring (its frame was undone before the engine saw it).
    pub fn undo_submit(&mut self, id: RingId) -> Option<Pending> {
        if self.ring_blocked(id).is_some() {
            return None;
        }
        self.rings.get_mut(&id)?.undo_submit()
    }

    /// Judges a sample of ring `id`. While the ring's reset runs the sample means nothing, except during a soft
    /// recovery, whose success the fence shows.
    pub fn poll(&mut self, id: RingId, now_us: u64, s: Sample) -> Option<Polled> {
        self.judge(id, |w| w.poll(now_us, s))
    }

    /// The same for a fake's run report.
    pub fn poll_report(&mut self, id: RingId, now_us: u64, r: &crate::sim::Report, fence: u64) -> Option<Polled> {
        self.judge(id, |w| w.poll_report(now_us, r, fence))
    }

    fn judge(&mut self, id: RingId, look: impl FnOnce(&mut RingWatch) -> Polled) -> Option<Polled> {
        let soft = self.rec.as_ref().is_some_and(|r| r.ring == id && r.open == Some(Scope::Soft));
        let blocked = self.ring_blocked(id);
        let w = self.rings.get_mut(&id)?;
        Some(match blocked {
            None => look(w),
            Some(state) if soft => Polled { state, ..look(w) },
            Some(state) => Polled::only(state),
        })
    }

    /// See [`RingWatch::report`].
    pub fn report(&self, id: RingId, now_us: u64, ring: &[u32], ibs: &[IbWords<'_>]) -> Option<HangReport> {
        self.rings.get(&id)?.report(now_us, ring, ibs)
    }

    /// Starts the recovery of a hung ring. It takes the report so that the dump exists before anything changes;
    /// the caller logs it (B6). A ring that moved since the report is refused and nothing happens.
    pub fn begin(&mut self, report: &HangReport, now_us: u64) -> Result<Next, RecoverError> {
        if self.dead {
            return Err(RecoverError::Dead);
        }
        if self.rec.is_some() {
            return Err(RecoverError::Busy);
        }
        let w = self.rings.get(&report.ring).ok_or(RecoverError::NoRing)?;
        let hung: Option<Cause> = w.cause(now_us);
        let bad = *w.jobs().next().ok_or(RecoverError::NotHung)?;
        if hung.is_none() || bad.seq != report.job.seq {
            return Err(RecoverError::NotHung);
        }
        let mut rungs = VecDeque::new();
        if w.cfg().kill_waves && bad.meta.vmid != 0 && matches!(bad.meta.owner, Owner::Client { .. }) {
            rungs.push_back(Rung::Soft);
        }
        if w.cfg().queue != QueueReset::None {
            rungs.push_back(Rung::Queue);
        }
        rungs.push_back(Rung::Gc);
        rungs.push_back(Rung::Mode2);
        let rec = Recovery {
            ring: report.ring,
            bad,
            rungs,
            open: None,
            todo: VecDeque::new(),
            waiting: Action::SuspendGc,
            deadline_us: now_us,
            fates: Vec::new(),
            guilty: Vec::new(),
            blamed: false,
            full_counted: false,
            vram_lost: false,
            reinit_refused: None,
            display_changed: false,
            overruns: 0,
            expired: 0,
        };
        if !self.policy.recovery {
            return Ok(Next::Done(self.freeze(rec)));
        }
        Ok(self.drive(rec, now_us))
    }

    /// The step the driver owes an answer for. A worker that can outlive [`ResetDomain::expire`] compares its step
    /// with this one and drops its answer if they differ: `reply` always means the current step.
    pub fn waiting(&self) -> Option<Step> {
        self.rec.as_ref().map(|r| Step { action: r.waiting, deadline_us: r.deadline_us })
    }

    /// The answer to the last [`Step`]. A reply of the wrong kind counts as a failure.
    pub fn reply(&mut self, reply: Reply, now_us: u64) -> Result<Next, RecoverError> {
        let mut rec = self.rec.take().ok_or(RecoverError::Idle)?;
        if now_us > rec.deadline_us {
            rec.overruns = rec.overruns.saturating_add(1);
        }
        Ok(self.answer(rec, reply, now_us))
    }

    /// Gives up on a step nobody answered, once its deadline and `Policy.expire_slack_us` have passed, as
    /// `Reply::Failed` would: the ladder moves up. None if nothing is overdue. GX-8 and GX-9a call this from their
    /// poll loop; without it a lost answer would keep this ring, and in a full reset every ring, blocked for good.
    pub fn expire(&mut self, now_us: u64) -> Option<Next> {
        let rec = self.rec.as_ref()?;
        if now_us <= rec.deadline_us.saturating_add(self.policy.expire_slack_us) {
            return None;
        }
        let mut rec = self.rec.take()?;
        rec.expired = rec.expired.saturating_add(1);
        Some(self.answer(rec, Reply::Failed, now_us))
    }

    fn answer(&mut self, mut rec: Recovery, reply: Reply, now_us: u64) -> Next {
        let ok = match rec.waiting {
            Action::KillWaves { ring, seq, .. } => self.rings.get(&ring).is_some_and(|w| w.timeline().is_signalled(seq)),
            Action::Blame { .. } => true,
            Action::Unmap { .. } | Action::ResetQueue { .. } | Action::Map { .. } | Action::RestartSdma { .. } | Action::SuspendGc => {
                reply == Reply::Ok
            }
            Action::Mode2Reset | Action::ReinitGc => match reply {
                Reply::Reset { vram_intact, display_equal } => {
                    self.note_reset(&mut rec, vram_intact, display_equal);
                    true
                }
                _ => false,
            },
            Action::TestRing { ring, .. } => match reply {
                Reply::Ring { ptr } => {
                    if let Some(w) = self.rings.get_mut(&ring) {
                        w.resync(ptr);
                    }
                    true
                }
                _ => false,
            },
        };
        if !ok {
            rec.open = None;
            rec.todo.clear();
        }
        self.drive(rec, now_us)
    }

    fn note_reset(&mut self, rec: &mut Recovery, vram_intact: bool, display_equal: bool) {
        // Linux's vram_lost_counter moves once per reset.
        if !vram_intact && !rec.vram_lost {
            rec.vram_lost = true;
            self.counters.vram_lost = self.counters.vram_lost.wrapping_add(1);
        }
        if !display_equal {
            rec.display_changed = true;
            self.gc_closed = true;
            self.policy.mode2 = Mode2Gate::Refused(MODE2_MOVED_PICTURE);
        }
    }

    fn drive(&mut self, mut rec: Recovery, now_us: u64) -> Next {
        loop {
            if let Some(action) = rec.todo.pop_front() {
                let deadline_us = now_us.saturating_add(self.bound_us(&action));
                rec.waiting = action;
                rec.deadline_us = deadline_us;
                self.rec = Some(rec);
                return Next::Do(Step { action, deadline_us });
            }
            if let Some(scope) = rec.open {
                return Next::Done(self.finish(rec, Outcome::Recovered(scope), now_us));
            }
            match rec.rungs.pop_front() {
                Some(rung) => self.enter(&mut rec, rung, now_us),
                None => return Next::Done(self.finish_dead(rec)),
            }
        }
    }

    fn bound_us(&self, a: &Action) -> u64 {
        let p = &self.policy;
        match a {
            Action::KillWaves { .. } => p.kill_us,
            Action::Blame { .. } | Action::Unmap { .. } | Action::ResetQueue { .. } | Action::Map { .. } | Action::RestartSdma { .. } => p.queue_us,
            Action::SuspendGc | Action::Mode2Reset => p.mode2_us,
            Action::ReinitGc => p.reinit_us,
            Action::TestRing { .. } => p.test_us,
        }
    }

    /// Opens a rung. One that is not allowed now leaves `open` empty and the ladder moves on.
    fn enter(&mut self, rec: &mut Recovery, rung: Rung, now_us: u64) {
        match rung {
            Rung::Soft => {
                // Before the kill, and it stays if the kill fails (amdgpu_ring.c:447).
                self.counters.reset = self.counters.reset.wrapping_add(1);
                rec.open = Some(Scope::Soft);
                rec.todo.push_back(Action::KillWaves {
                    ring: rec.ring,
                    vmid: rec.bad.meta.vmid,
                    seq: rec.bad.seq,
                    errno: errno::ENODATA,
                });
            }
            Rung::Queue => self.enter_queue(rec),
            Rung::Gc => {
                if let Some(why) = self.reinit_refusal(now_us) {
                    rec.reinit_refused = Some(why);
                    return;
                }
                self.enter_full(rec, now_us);
                rec.open = Some(Scope::Gc);
                rec.todo.push_back(Action::ReinitGc);
                self.queue_ring_tests(rec);
            }
            Rung::Mode2 => {
                if matches!(self.policy.mode2, Mode2Gate::Refused(_)) || self.reinit_refusal(now_us).is_some() {
                    return;
                }
                self.enter_full(rec, now_us);
                rec.open = Some(Scope::Mode2);
                rec.todo.extend([Action::SuspendGc, Action::Mode2Reset, Action::ReinitGc]);
                self.queue_ring_tests(rec);
            }
        }
    }

    fn enter_queue(&mut self, rec: &mut Recovery) {
        let ring = rec.ring;
        let Some(w) = self.rings.get_mut(&ring) else { return };
        let queue = w.cfg().queue;
        let jobs = w.take_jobs();
        let fence_value = w.timeline().emitted();
        // A shared ring also loses other owners' jobs: then they have to learn it. A process queue loses only its
        // own, and a running game elsewhere must not see a reset that did not touch it (radv: device lost).
        let bad_owner = rec.bad.meta.owner;
        let others_hit = jobs.iter().any(|j| !same_ctx(j.meta.owner, bad_owner));
        take(rec, ring, jobs);
        if others_hit {
            self.counters.reset = self.counters.reset.wrapping_add(1);
        }
        blame(rec);
        rec.open = Some(Scope::Queue);
        match queue {
            QueueReset::SdmaRestart => rec.todo.push_back(Action::RestartSdma { ring }),
            _ => rec.todo.extend([Action::Unmap { ring, queue }, Action::ResetQueue { ring, queue }, Action::Map { ring, queue }]),
        }
        rec.todo.push_back(Action::TestRing { ring, fence_value });
    }

    fn enter_full(&mut self, rec: &mut Recovery, now_us: u64) {
        rec.todo.clear();
        self.reinits.push_back(now_us);
        self.counters.in_reset = true;
        if !rec.full_counted {
            rec.full_counted = true;
            self.counters.reset = self.counters.reset.wrapping_add(1);
        }
        self.take_all(rec);
        blame(rec);
    }

    fn reinit_refusal(&mut self, now_us: u64) -> Option<&'static str> {
        if self.gc_closed {
            return Some(REINIT_MOVED_PICTURE);
        }
        let lim = self.policy.reinit_limit;
        while self.reinits.front().is_some_and(|&t| now_us.saturating_sub(t) >= lim.window_us) {
            self.reinits.pop_front();
        }
        (self.reinits.len() >= lim.count).then_some(REINIT_LOOP)
    }

    fn take_all(&mut self, rec: &mut Recovery) {
        let ids: Vec<RingId> = self.rings.keys().copied().collect();
        for id in ids {
            if let Some(w) = self.rings.get_mut(&id) {
                let jobs = w.take_jobs();
                take(rec, id, jobs);
            }
        }
    }

    /// After GC's bring-up every ring is tested again; a process queue has to be mapped first, the bring-up
    /// knows only the kernel's queues.
    fn queue_ring_tests(&self, rec: &mut Recovery) {
        for (&ring, w) in &self.rings {
            if let QueueReset::MesProcess { .. } = w.cfg().queue {
                rec.todo.push_back(Action::Map { ring, queue: w.cfg().queue });
            }
            rec.todo.push_back(Action::TestRing { ring, fence_value: w.timeline().emitted() });
        }
    }

    fn finish(&mut self, rec: Recovery, outcome: Outcome, now_us: u64) -> Finish {
        self.counters.in_reset = false;
        if let Some(w) = self.rings.get_mut(&rec.ring) {
            w.rearm(now_us);
        }
        self.finish_common(rec, outcome)
    }

    /// Nothing is left to try. Programs must not wait for this reset to end, so in_reset goes off too.
    fn finish_dead(&mut self, mut rec: Recovery) -> Finish {
        self.take_all(&mut rec);
        if !rec.full_counted {
            rec.full_counted = true;
            self.counters.reset = self.counters.reset.wrapping_add(1);
        }
        blame(&mut rec);
        rec.todo.clear();
        self.counters.in_reset = false;
        self.dead = true;
        let mode2 = match self.policy.mode2 {
            Mode2Gate::Refused(why) => Some(why),
            Mode2Gate::Measured { .. } => None,
        };
        self.finish_common(rec, Outcome::NeedsReboot { mode2 })
    }

    /// `Policy.recovery` is off: the ring's jobs end, nothing else changes. Later jobs would wait behind the hung one
    /// for ever, so they end too.
    fn freeze(&mut self, mut rec: Recovery) -> Finish {
        let ring = rec.ring;
        if let Some(w) = self.rings.get_mut(&ring) {
            let jobs = w.take_jobs();
            take(&mut rec, ring, jobs);
        }
        self.frozen.insert(ring);
        self.finish_common(rec, Outcome::RecoveryOff)
    }

    fn finish_common(&mut self, rec: Recovery, outcome: Outcome) -> Finish {
        self.rec = None;
        Finish {
            outcome,
            ring: rec.ring,
            seq: rec.bad.seq,
            fates: rec.fates,
            guilty: rec.guilty,
            counters: self.counters,
            vram_lost: rec.vram_lost,
            reinit_refused: rec.reinit_refused,
            display_changed: rec.display_changed,
            overruns: rec.overruns,
            expired: rec.expired,
        }
    }
}

fn take(rec: &mut Recovery, ring: RingId, jobs: Vec<Pending>) {
    for job in jobs {
        let hung = ring == rec.ring && job.seq == rec.bad.seq;
        rec.fates.push(JobFate { ring, job, fate: if hung { Fate::Hung } else { Fate::Canceled } });
    }
}

fn blame(rec: &mut Recovery) {
    if rec.blamed {
        return;
    }
    rec.blamed = true;
    if let Owner::Client { ctx, entity } = rec.bad.meta.owner {
        rec.guilty.push(ctx);
        rec.todo.push_back(Action::Blame { ctx, entity });
    }
}

fn same_ctx(a: Owner, b: Owner) -> bool {
    match (a, b) {
        (Owner::Client { ctx: x, .. }, Owner::Client { ctx: y, .. }) => x == y,
        _ => false,
    }
}
