//! One watchdog: a register block, a CPU source, the per-CPU tracks and the kick decision. `check` is what the
//! kicker runs once a second; it allocates nothing, takes no lock and never logs (timer context), so it only
//! leaves news for the reporter (`take_news`). The system has one `Dog` on the real chip; the ktests build their
//! own on `regs::Fake` with the real CPUs or a fake CPU source.

use crate::live::{self, Live, Policy, Sample, Track};
use crate::regs::{self, ChipError, Regs};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

/// How often `exclusive` asks before it goes on without the kicker's flag (some ms of spinning).
const EXCLUSIVE_SPINS: u32 = 1 << 22;

/// The kicker's flag held by a chip write outside `check` (released on drop; `None`: the bound ran out).
struct Exclusive<'a>(Option<&'a AtomicBool>);

impl Drop for Exclusive<'_> {
    fn drop(&mut self) {
        if let Some(f) = self.0 {
            f.store(false, Ordering::Release);
        }
    }
}

/// Where the CPU samples come from (the scheduler in the kernel, a table in tests).
pub trait CpuSource: Send + Sync {
    /// CPUs to judge (0..count).
    fn count(&self) -> usize;
    /// The CPU running the check.
    fn this_cpu(&self) -> usize;
    fn sample(&self, cpu: usize) -> Sample;
    /// Sends a probe that moves the heartbeat of a CPU that takes interrupts; false if it could not be sent.
    fn probe(&self, cpu: usize) -> bool;
}

#[derive(Default)]
struct Cell {
    seen: AtomicBool,
    heartbeat: AtomicU64,
    moved_at: AtomicU64,
    probe_since: AtomicU64,
    last_probe: AtomicU64,
    live: AtomicU8,
    /// The reporter has logged the current stall.
    reported: AtomicBool,
}

impl Cell {
    fn load(&self) -> Track {
        Track {
            seen: self.seen.load(Ordering::Relaxed),
            last_heartbeat: self.heartbeat.load(Ordering::Relaxed),
            moved_at: self.moved_at.load(Ordering::Relaxed),
            probe_since: self.probe_since.load(Ordering::Relaxed),
            last_probe: self.last_probe.load(Ordering::Relaxed),
        }
    }
    fn store(&self, t: Track) {
        self.seen.store(t.seen, Ordering::Relaxed);
        self.heartbeat.store(t.last_heartbeat, Ordering::Relaxed);
        self.moved_at.store(t.moved_at, Ordering::Relaxed);
        self.probe_since.store(t.probe_since, Ordering::Relaxed);
        self.last_probe.store(t.last_probe, Ordering::Release);
    }
}

/// One round's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub judged: usize,
    pub stalled: usize,
    pub probes: usize,
    pub kicked: bool,
    /// Another check was running (this one did nothing).
    pub busy: bool,
}

/// A CPU as the last check saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuView {
    pub cpu: usize,
    pub live: Live,
    pub heartbeat: u64,
    /// Since the heartbeat last moved, at the time of the view.
    pub age_ns: u64,
    pub probe_pending_ns: Option<u64>,
    pub reported: bool,
}

pub struct Dog {
    regs: Box<dyn Regs>,
    cpus: Box<dyn CpuSource>,
    clock: fn() -> u64,
    cells: Vec<Cell>,
    policy: Policy,
    timeout_s: u32,
    armed: AtomicBool,
    checking: AtomicBool,
    news: AtomicBool,
    pub(crate) kicks: AtomicU64,
    pub(crate) withheld: AtomicU64,
    pub(crate) checks: AtomicU64,
    pub(crate) probes: AtomicU64,
    pub(crate) last_kick_ns: AtomicU64,
    pub(crate) last_check_ns: AtomicU64,
}

impl Dog {
    /// `cpus.count()` at creation sizes the tracks (R1: as many as the machine has).
    pub fn new(regs: Box<dyn Regs>, cpus: Box<dyn CpuSource>, clock: fn() -> u64, policy: Policy, timeout_s: u32) -> Dog {
        let n = cpus.count();
        Dog {
            regs,
            cpus,
            clock,
            cells: (0..n).map(|_| Cell::default()).collect(),
            policy,
            timeout_s,
            armed: AtomicBool::new(false),
            checking: AtomicBool::new(false),
            news: AtomicBool::new(false),
            kicks: AtomicU64::new(0),
            withheld: AtomicU64::new(0),
            checks: AtomicU64::new(0),
            probes: AtomicU64::new(0),
            last_kick_ns: AtomicU64::new(0),
            last_check_ns: AtomicU64::new(0),
        }
    }

    pub fn regs(&self) -> &dyn Regs {
        &*self.regs
    }
    pub fn policy(&self) -> Policy {
        self.policy
    }
    pub fn timeout_s(&self) -> u32 {
        self.timeout_s
    }
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Acquire)
    }
    pub fn cpus(&self) -> usize {
        self.cells.len()
    }

    /// Arms the chip with the timeout; from now on `check` kicks.
    pub fn arm(&self) -> Result<(), ChipError> {
        let _g = self.exclusive();
        regs::arm(&*self.regs, self.timeout_s)?;
        self.last_kick_ns.store((self.clock)(), Ordering::Release);
        self.armed.store(true, Ordering::Release);
        Ok(())
    }

    /// Stops the chip and the kicks. No lock, no log (Quiesce with interrupts off).
    pub fn stop(&self) {
        self.armed.store(false, Ordering::Release);
        let _g = self.exclusive();
        regs::stop(&*self.regs);
    }

    /// Keeps the chip armed with `secs` from now and stops the kicks (before a reboot: a safety net while the
    /// machine resets and the next kernel boots).
    pub fn grace(&self, secs: u32) {
        self.armed.store(false, Ordering::Release);
        let _g = self.exclusive();
        if regs::running(&*self.regs) {
            regs::set_count_and_kick(&*self.regs, secs);
        }
    }

    /// Arms the chip again without kicks: it resets the machine after the timeout (a Reload whose jump failed
    /// left the devices quiesced; only a reset brings the machine back).
    pub fn arm_unkicked(&self) -> Result<(), ChipError> {
        self.armed.store(false, Ordering::Release);
        let _g = self.exclusive();
        regs::arm(&*self.regs, self.timeout_s)
    }

    /// Takes the chip from the kicker: a kick is a read-modify-write of the control register, so a check that
    /// read RUN before a stop would write it back after the stop (and a late kick would overwrite the grace
    /// count). Bounded: a check holds it for microseconds; one that never lets go (its CPU parked by a Reload
    /// mid-check) writes nothing more either, so after the bound the caller goes on without it. No lock, no
    /// clock (Quiesce with interrupts off; host tests with a frozen clock).
    fn exclusive(&self) -> Exclusive<'_> {
        for _ in 0..EXCLUSIVE_SPINS {
            if !self.checking.swap(true, Ordering::AcqRel) {
                return Exclusive(Some(&self.checking));
            }
            core::hint::spin_loop();
        }
        Exclusive(None)
    }

    /// One round: judge every CPU, probe the quiet ones, kick if all are alive and the dog is armed.
    pub fn check(&self) -> Round {
        if self.checking.swap(true, Ordering::AcqRel) {
            return Round { busy: true, ..Round::default() };
        }
        let now = (self.clock)();
        let me = self.cpus.this_cpu();
        let n = self.cells.len().min(self.cpus.count());
        let mut r = Round::default();
        let mut all = true;
        for (c, cell) in self.cells.iter().enumerate().take(n) {
            let s = self.cpus.sample(c);
            let (mut t, l, want_probe) = live::judge(cell.load(), s, c == me, now, self.policy);
            if want_probe {
                let sent = self.cpus.probe(c);
                t = live::note_probe(t, sent, now);
                if sent {
                    r.probes += 1;
                }
            }
            cell.store(t);
            let before = Live::from_code(cell.live.swap(l.code(), Ordering::AcqRel));
            if before.alive() != l.alive() {
                self.news.store(true, Ordering::Release);
            }
            if l != Live::Offline {
                r.judged += 1;
            }
            if !l.alive() {
                r.stalled += 1;
                all = false;
            }
        }
        self.checks.fetch_add(1, Ordering::Relaxed);
        self.probes.fetch_add(r.probes as u64, Ordering::Relaxed);
        self.last_check_ns.store(now, Ordering::Release);
        if self.armed.load(Ordering::Acquire) {
            if all {
                regs::kick(&*self.regs);
                self.kicks.fetch_add(1, Ordering::AcqRel);
                self.last_kick_ns.store(now, Ordering::Release);
                r.kicked = true;
            } else {
                self.withheld.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.checking.store(false, Ordering::Release);
        r
    }

    /// Whether a CPU changed between alive and stalled since the last call (clears it).
    pub fn take_news(&self) -> bool {
        self.news.swap(false, Ordering::AcqRel)
    }
    pub fn has_news(&self) -> bool {
        self.news.load(Ordering::Acquire)
    }

    /// The last verdict on `cpu`.
    pub fn view(&self, cpu: usize) -> Option<CpuView> {
        let cell = self.cells.get(cpu)?;
        let t = cell.load();
        let now = (self.clock)();
        Some(CpuView {
            cpu,
            live: Live::from_code(cell.live.load(Ordering::Acquire)),
            heartbeat: t.last_heartbeat,
            age_ns: now.saturating_sub(t.moved_at),
            probe_pending_ns: (t.probe_since != 0).then(|| now.saturating_sub(t.probe_since)),
            reported: cell.reported.load(Ordering::Acquire),
        })
    }

    /// Marks the stall of `cpu` as reported (true if it was not yet) or, for a CPU alive again, clears the mark
    /// (returns false then).
    pub fn mark_reported(&self, cpu: usize, reported: bool) -> bool {
        self.cells.get(cpu).is_some_and(|c| c.reported.swap(reported, Ordering::AcqRel) != reported)
    }

    pub fn kicks(&self) -> u64 {
        self.kicks.load(Ordering::Acquire)
    }
    pub fn withheld(&self) -> u64 {
        self.withheld.load(Ordering::Acquire)
    }
    pub fn checks(&self) -> u64 {
        self.checks.load(Ordering::Acquire)
    }
    pub fn probes(&self) -> u64 {
        self.probes.load(Ordering::Acquire)
    }
    pub fn last_kick_ns(&self) -> u64 {
        self.last_kick_ns.load(Ordering::Acquire)
    }
    pub fn now(&self) -> u64 {
        (self.clock)()
    }
}
