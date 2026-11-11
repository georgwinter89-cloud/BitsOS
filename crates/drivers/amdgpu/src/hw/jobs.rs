//! GX-8b: the job half of the hardware backend. `submit` only queues (it runs under the CS locks); one kernel thread,
//! `amdgpu-jobs`, does everything else: it picks what is ready (`FakeSched`'s rules: dependencies, an entity's order,
//! then priority), writes frames on the rings (`run`), looks at fences, read pointers and GFXHUB's fault registers, ends
//! jobs, and handles a hang (`lost`). One writer per ring, so no lock is held from a VMID's grant to its frame, and
//! `VmJob`s drop in thread context as they must.
//!
//! The thread is Normal, not KernelRt or Interactive (B4): ending a job frees objects and takes a VM's lock, which has
//! no bound of its own. It sleeps while nothing is on a ring (bounded, a submit or a dependency wakes it) and backs off
//! while a long job runs, so an idle GPU costs one look per `IDLE_NS` and no spinning.
//!
//! GX-7n: with the IH ring running the fences raise interrupts and the thread sleeps until one wakes it, with a look every
//! `irq_check_ns` that keeps hang detection and a lost interrupt from waiting on anything (`ihwatch`). What a look reads
//! is the same with or without them: the fence interface does not change. Without a ring it polls as before.

use super::chain::Chain;
use super::frame::{RingType, check_ibs};
use super::heap::Heaps;
use super::ih::IrqFeed;
use super::ihwatch::{IrqWatch, Told, Wait};
use super::rings::{HwRing, OnRing, fence_slot, ring_cfg};
use super::vm::VmIds;
use crate::gpucore::vmring::{self, VmInputs};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::recover::{IpVersion, Policy, ResetDomain};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use drm::fence::{self, FenceCb, Registration, Waiter};
use drm::job::{SchedPriority, Submission};
use drm::uapi::AMDGPU_IB_FLAGS_SECURE;
use ksync::{Mutex, SpinLock};
use linux::ab::errno_more::ECANCELED;
use linux::errno::{EINVAL, ENODEV, ENOMEM};
use vfs::Errno;

/// How long the thread sleeps with nothing on a ring and nothing ready.
const IDLE_NS: u64 = 1_000_000_000;

/// The numbers a test may change; the default is what the chip runs.
#[derive(Clone, Copy, Debug)]
pub struct JobCfg {
    /// Per `RingType::index`; None: Linux's by class (gr-GR-3.md).
    pub timeouts_us: Option<[u64; 3]>,
    /// The poll's period right after progress and at its longest.
    pub poll_min_ns: u64,
    pub poll_max_ns: u64,
    /// How long a compute queue may show no progress after a VM fault of its head job before it counts as stopped there.
    pub stall_grace_us: u64,
    /// How long `stop` lets jobs on the rings end.
    pub close_us: u64,
    /// GX-7n: the thread's look while jobs run and interrupts are trusted: the most a lost one can cost.
    pub irq_check_ns: u64,
    /// GX-9a: the mode-2 rung is open, on this proof (GX-D9; CONFIG `gx_mode2=on`, `boot::mode2_open`).
    pub mode2: Option<&'static str>,
    /// GX-9a: the first rung a hang may use: `Rung`, lower ones answer `Failed` (the tool's `until=ladder`).
    pub ladder_from: Rung,
    /// GX-9a: instead of `Policy`'s: how long a re-init may take, and how long after that `expire` waits for its answer.
    pub reinit_us: Option<u64>,
    pub expire_slack_us: Option<u64>,
}

/// The rungs by rank, for `until=ladder` and the counters. `None`: above all of them: a hang ends the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    Soft = 0,
    Queue = 1,
    Gc = 2,
    Mode2 = 3,
    None = 4,
}

impl Rung {
    pub const COUNT: usize = 5;

    pub fn name(self) -> &'static str {
        match self {
            Rung::Soft => "the kill",
            Rung::Queue => "the queue reset",
            Rung::Gc => "GC's re-init",
            Rung::Mode2 => "mode 2",
            Rung::None => "nothing (a hang ends the device)",
        }
    }

    pub fn from_u8(v: u8) -> Rung {
        match v {
            0 => Rung::Soft,
            1 => Rung::Queue,
            2 => Rung::Gc,
            3 => Rung::Mode2,
            _ => Rung::None,
        }
    }

    pub fn of(s: amdgpu_fmt::recover::Scope) -> Rung {
        use amdgpu_fmt::recover::Scope;
        match s {
            Scope::Soft => Rung::Soft,
            Scope::Queue => Rung::Queue,
            Scope::Gc => Rung::Gc,
            Scope::Mode2 => Rung::Mode2,
        }
    }
}

impl Default for JobCfg {
    fn default() -> JobCfg {
        JobCfg { timeouts_us: None, poll_min_ns: 100_000, poll_max_ns: 2_000_000, stall_grace_us: 5_000, close_us: 2_000_000, irq_check_ns: 20_000_000, mode2: None, ladder_from: Rung::Soft, reinit_us: None, expire_slack_us: None }
    }
}

pub(super) struct Pend {
    pub sub: Submission,
    pub prio: SchedPriority,
    pub regs: Vec<Registration>,
    /// No room or no VMID in this round's pump.
    pub later: bool,
}

/// What every thread may read or change.
pub struct Shared {
    pub(super) pending: SpinLock<Vec<Pend>>,
    wake: Arc<Waiter>,
    /// No more submissions; the thread ends when the rings are empty.
    pub(super) closing: AtomicBool,
    /// The device is lost until the next `until=stop` and bring-up (GR-3's `NeedsReboot`).
    pub lost: AtomicBool,
    pub reset_counter: AtomicU32,
    pub vram_lost: AtomicU32,
    pub in_reset: AtomicBool,
    pub rounds: AtomicU64,
    pub jobs_done: AtomicU64,
    pub faults: AtomicU64,
    pub hangs: AtomicU64,
    pub stalls: AtomicU64,
    /// GX-7n: how often a look found a fence moved.
    pub fences_moved: AtomicU64,
    /// GX-9a: recoveries by the rung that ended them; `Rung::None`'s: hangs that ended the device.
    pub recovered: [AtomicU64; Rung::COUNT],
    /// GX-9a: `Rung` as a number: what the tool's `until=ladder` set.
    pub ladder_from: AtomicU8,
}

impl Shared {
    /// The job thread's sleep ends: a thread of the job half has something to say.
    pub(super) fn kick_thread(&self) {
        self.wake.call();
    }
}

/// A device's job half.
pub struct Jobs {
    pub shared: Arc<Shared>,
    pub(super) inner: Mutex<super::run::Inner>,
    pub(super) cfg: JobCfg,
    thread: SpinLock<Option<sched::JoinHandle>>,
    /// GX-7n: the IH side, when a ring runs.
    feed: Option<Arc<IrqFeed>>,
    watch: SpinLock<IrqWatch>,
}

/// Jobs the hardware may still run: what they reach stays until the engines are halted.
pub struct Held(pub(super) Vec<OnRing>);

impl Held {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// The poll's verdict for the thread.
pub(super) enum Round {
    Idle,
    Busy { moved: bool, stalling: bool },
    Exit,
}

impl Jobs {
    /// On a kept chain: the queues read idle, their fence slots at 0. `max_wave_id`: gfx_v11_0.c:6267.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(chain: Arc<dyn Chain>, vmi: VmInputs, max_wave_id: u32, cfg: JobCfg, feed: Option<Arc<IrqFeed>>, heaps: Arc<Heaps>, ids: Arc<VmIds>) -> Result<Arc<Jobs>, String> {
        let mut found = Err(String::from("the chain's owner is gone or busy"));
        chain.with(&mut |o| {
            found = vmring::kept_queues(o.io(), &vmi).map_err(|e| format!("a kept queue: {e:?}"));
        });
        let queues = found?;
        let mut domain = ResetDomain::new(policy(&cfg)?);
        let mut rings = Vec::new();
        for (q, ty) in queues.into_iter().zip(RingType::ALL) {
            let id = domain.add_ring(ring_cfg(ty, &q, &cfg)).map_err(|e| format!("{} in the reset domain: {e:?}", ty.name()))?;
            let fence = fence_slot(&vmi, ty);
            rings.push(HwRing { ty, id, queue: q, fence, jobs: Default::default(), current_ctx: None, stall: None });
        }
        let mut ok = false;
        chain.with(&mut |o| ok = rings.iter().all(|r| HwRing::clear_slots(o.io(), r.fence)));
        if !ok {
            return Err(String::from("a fence slot could not be written"));
        }
        let rings: [HwRing; 3] = rings.try_into().map_err(|_| String::from("not three rings"))?;
        let shared = Arc::new(Shared {
            pending: SpinLock::named("amdgpu.hw.jobs_pending", Vec::new()),
            wake: Waiter::new(),
            closing: AtomicBool::new(false),
            lost: AtomicBool::new(false),
            reset_counter: AtomicU32::new(0),
            vram_lost: AtomicU32::new(0),
            in_reset: AtomicBool::new(false),
            rounds: AtomicU64::new(0),
            jobs_done: AtomicU64::new(0),
            faults: AtomicU64::new(0),
            hangs: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            fences_moved: AtomicU64::new(0),
            recovered: Default::default(),
            ladder_from: AtomicU8::new(cfg.ladder_from as u8),
        });
        let inner = super::run::Inner::new(chain, vmi, domain, rings, max_wave_id, cfg, shared.clone(), feed.is_some(), heaps, ids);
        if let Some(f) = &feed {
            f.attach(shared.wake.clone());
        }
        let watch = IrqWatch::new(0, feed.as_ref().map_or(0, |f| f.fence_events.load(Ordering::Acquire)));
        let jobs = Arc::new(Jobs { shared, inner: Mutex::named("amdgpu.hw.jobs", inner), cfg, thread: SpinLock::named("amdgpu.hw.jobs_thread", None), feed, watch: SpinLock::named("amdgpu.hw.irq_watch", watch) });
        jobs.start()?;
        Ok(jobs)
    }

    #[cfg(not(test))]
    fn start(self: &Arc<Self>) -> Result<(), String> {
        let me = self.clone();
        let t = sched::spawn("amdgpu-jobs", sched::Class::Normal, move || me.thread_main()).map_err(|e| format!("the job thread: {e:?}"))?;
        *self.thread.lock() = Some(t);
        Ok(())
    }
    /// Host tests have no threads: `kick` does a round.
    #[cfg(test)]
    fn start(self: &Arc<Self>) -> Result<(), String> {
        Ok(())
    }

    #[cfg(not(test))]
    fn thread_main(&self) {
        klog::event!(Debug, "gx", "hw.jobs_thread_up");
        let mut nap = self.cfg.poll_min_ns;
        while let Some(n) = self.next_nap(nap) {
            nap = n;
            self.shared.wake.sleep(nap);
        }
        if let Some(f) = &self.feed {
            f.detach();
        }
        klog::event!(Debug, "gx", "hw.jobs_thread_end");
    }

    /// A round, then how long to sleep; None: the thread ends.
    fn next_nap(&self, nap: u64) -> Option<u64> {
        #[cfg(test)]
        if let Some(f) = &self.feed {
            f.host_pump();
        }
        // Before the round's look at the fences (`ihwatch`).
        let events = self.feed.as_ref().map(|f| f.fence_events.load(Ordering::Acquire));
        let round = self.round();
        let wait = events.and_then(|e| self.irq_wait(e));
        match round {
            Round::Exit => None,
            Round::Idle if wait == Some(Wait::Settle) => Some(self.cfg.poll_max_ns),
            Round::Idle => Some(IDLE_NS),
            Round::Busy { stalling: false, .. } if wait == Some(Wait::Interrupts) => Some(self.cfg.irq_check_ns),
            Round::Busy { stalling: false, .. } if wait == Some(Wait::Settle) => Some(self.cfg.poll_max_ns),
            Round::Busy { moved: true, .. } => Some(self.cfg.poll_min_ns),
            Round::Busy { moved: false, .. } => Some(nap.saturating_mul(2).clamp(self.cfg.poll_min_ns, self.cfg.poll_max_ns)),
        }
    }

    /// How interrupts stand after a round; None: no ring, or they are switched off by the tool.
    fn irq_wait(&self, events: u64) -> Option<Wait> {
        let f = self.feed.as_ref()?;
        let fences = self.shared.fences_moved.load(Ordering::Relaxed);
        let (wait, told) = self.watch.lock().after_round(fences, events, super::clock_ns());
        self.tell(f, told, fences);
        if !f.running.load(Ordering::Acquire) || f.force_poll.load(Ordering::Acquire) {
            return None;
        }
        Some(wait)
    }

    fn tell(&self, f: &IrqFeed, t: Told, fences: u64) {
        if t.lost {
            f.lost.fetch_add(1, Ordering::Relaxed);
            klog::event!(Debug, "gx", "ih.lost", fences = fences);
            // With RPTR_REARM a missed entry also holds the next message back until the read pointer moves: a look now.
            f.look_now();
        }
        if t.fell_back {
            f.fell_back.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "gx", "ih.fallback_poll", lost = f.lost.load(Ordering::Relaxed); "fences move with no interrupt: the job thread polls again");
        }
        if t.came_back {
            klog::event!(Info, "gx", "ih.interrupts_back"; "fences come with their interrupts again");
        }
    }

    /// How the thread waits now, for the tool and the tests.
    pub fn waits_on_interrupts(&self) -> bool {
        self.feed.as_ref().is_some_and(|f| f.running.load(Ordering::Acquire) && !f.force_poll.load(Ordering::Acquire) && !self.watch.lock().polling())
    }

    /// The job thread's CPU time so far.
    pub fn thread_cpu_ns(&self) -> Option<u64> {
        let id = self.thread.lock().as_ref()?.id();
        Some(sched::cputime::of_thread(id)?.total_ns())
    }

    /// The IH side's numbers.
    pub fn feed(&self) -> Option<&Arc<IrqFeed>> {
        self.feed.as_ref()
    }

    /// One pass of the thread.
    pub(super) fn round(&self) -> Round {
        self.shared.rounds.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().round()
    }

    /// The trait's `submit`.
    pub fn submit(&self, sub: Submission) -> Result<(), Errno> {
        let s = &self.shared;
        if s.lost.load(Ordering::Acquire) || s.closing.load(Ordering::Acquire) {
            sub.cancel(ENODEV);
            return Err(ENODEV);
        }
        // TMZ: no trusted memory is set up here (INFO has its bit off, GX-8a decision 10), and a secure IB of a
        // program that asks all the same must not run in the clear. Linux answers a ring without it the same way
        // (amdgpu_ib.c:178).
        let bad = sub.jobs.iter().any(|j| {
            j.ibs.iter().any(|b| b.flags & AMDGPU_IB_FLAGS_SECURE != 0) || RingType::of_ip(j.entity.ip_type).is_none_or(|t| check_ibs(t, &j.ibs).is_err())
        });
        if bad {
            klog::event!(Debug, "gx", "hw.job_refused", jobs = sub.jobs.len(); "a secure IB, an unaligned IB or an IP without a ring");
            sub.cancel(EINVAL);
            return Err(EINVAL);
        }
        let mut regs = Vec::new();
        if regs.try_reserve_exact(sub.deps.len()).is_err() {
            sub.cancel(ENOMEM);
            return Err(ENOMEM);
        }
        let wake: Arc<dyn FenceCb> = s.wake.clone();
        for d in &sub.deps {
            if let Some(r) = fence::add_callback(d, wake.clone()) {
                regs.push(r);
            }
        }
        let prio = sub.jobs.iter().map(|j| j.entity.priority).max().unwrap_or(SchedPriority::Normal);
        let mut q = s.pending.lock();
        // Again under the lock: the thread drains after `closing` is set and then leaves, so whoever pushes now sees it or is
        // still drained. A push after the last drain would never signal, and WAIT_CS would wait for good.
        if s.lost.load(Ordering::Acquire) || s.closing.load(Ordering::Acquire) {
            drop(q);
            sub.cancel(ENODEV);
            return Err(ENODEV);
        }
        if q.try_reserve(1).is_err() {
            drop(q);
            sub.cancel(ENOMEM);
            return Err(ENOMEM);
        }
        q.push(Pend { sub, prio, regs, later: false });
        Ok(())
    }

    /// The trait's `kick`: the CS dropped its locks.
    pub fn kick(&self) {
        self.shared.wake.call();
        // Host tests have no thread.
        #[cfg(test)]
        {
            let _ = self.next_nap(self.cfg.poll_min_ns);
        }
    }

    /// Ends the thread: no new jobs, what is queued ends unrun, what is on the rings gets `close_us`. What the hardware
    /// may still run comes back: the caller keeps it until the engines are halted.
    pub fn stop(&self) -> Held {
        self.shared.closing.store(true, Ordering::Release);
        self.kick();
        // A recovery in its full reset ends first: `expire` bounds that, its steps' deadlines added up.
        let bound = self.cfg.close_us.saturating_mul(1000).saturating_add(60_000_000_000);
        let t = self.thread.lock().take();
        if let Some(t) = t
            && !t.join_timeout(bound)
        {
            klog::event!(Error, "gx", "hw.jobs_thread_stuck"; "the job thread did not end: its jobs stay held");
        }
        #[cfg(test)]
        {
            let _ = bound;
            while self.next_nap(self.cfg.poll_min_ns).is_some() {}
        }
        match self.inner.lock_timeout(1_000_000_000) {
            Some(mut g) => Held(g.take_grave()),
            None => Held(Vec::new()),
        }
    }

    /// Tests: while the answer lives no round runs, so several submissions meet one pick.
    pub(crate) fn no_round(&self) -> ksync::MutexGuard<'_, super::run::Inner> {
        self.inner.lock()
    }

    /// Asks the thread to end, waiting for nothing (a device being dropped).
    pub fn leave(&self) {
        self.shared.closing.store(true, Ordering::Release);
        self.kick();
    }

    /// The tool's lines: each ring's position, what went wrong, the GX-D9 watch.
    pub fn describe(&self) -> Vec<String> {
        let mut v = match self.inner.lock_timeout(1_000_000_000) {
            Some(g) => g.describe(),
            None => alloc::vec![String::from("backend jobs: the job thread holds its state (busy)")],
        };
        match &self.feed {
            Some(f) => {
                v.extend(f.describe());
                v.push(format!("backend interrupts: the job thread {}", if f.force_poll.load(Ordering::Acquire) { "polls (until=irq_poll)" } else if self.waits_on_interrupts() { "waits on interrupts" } else { "polls (interrupts did not come)" }));
            }
            None => v.push(String::from("backend interrupts: no IH ring, the job thread polls")),
        }
        v
    }
}

/// GR-3's policy for this chip, what the chip's boot and the tests say on top.
fn policy(cfg: &JobCfg) -> Result<Policy, String> {
    let mp1 = IpVersion(13, 0, 4);
    let mut p = Policy::for_mp1(mp1);
    if let Some(proof) = cfg.mode2 {
        p.open_mode2(mp1, proof).map_err(String::from)?;
    }
    if let Some(us) = cfg.reinit_us {
        p.reinit_us = us;
    }
    if let Some(us) = cfg.expire_slack_us {
        p.expire_slack_us = us;
    }
    Ok(p)
}

/// ECANCELED for a queued job when the device is lost or going.
pub(super) const CANCELED: Errno = ECANCELED;
