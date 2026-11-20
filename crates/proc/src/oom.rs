//! MEMCRASH row 2: the OOM killer (linux-7.3.1/mm/oom_kill.c: `out_of_memory` :1117, `select_bad_process` :366,
//! `oom_evaluate_task` :309, `oom_badness` :201, `__oom_kill_process` :912, the reaper `oom_reaper` :652 with
//! `MAX_OOM_REAP_RETRIES` 10 and a tenth of a second between tries).
//!
//! mm-core asks it when a small allocation that must not fail found nothing for `MAX_RECLAIM_RETRIES` rounds
//! (`mm_core::oom`). It picks the program that gives back the most (`mm_core::oom::badness`: resident pages and
//! page tables, moved by `oom_score_adj`) and never the global init (pid 1), a program whose memory init shares,
//! a vfork child (its parent is chosen instead) or a program with oom_score_adj -1000, as Linux. A program that
//! is ending already and asks for memory is the victim itself (Linux `task_will_free_mem(current)`). The choice
//! is made in the caller (no allocation, nothing dropped but a weak reference: the caller is the allocator's slow
//! path); the SIGKILL to the program and to every other program sharing its memory, the log line and the
//! reaping are the job of the thread `mm.oom_reaper`, a memory helper (`mm_core::oom::mark_memalloc`; Linux's
//! reaper never allocates on this path). While a victim is still going away nobody else is chosen (Linux: an
//! OOM victim without MMF_OOM_SKIP ends the scan); the reaper sets that mark once it took the victim's memory or
//! gave up after ten tries (`mm.as` held all the time).

use crate::{Process, TABLE};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering::*};
use ksync::SpinLock;
use mm_core::oom::{badness, Verdict};
use uapi::ab::signal::SIGKILL;

/// Linux `MAX_OOM_REAP_RETRIES` and the pause between two tries.
const REAP_TRIES: u32 = 10;
const REAP_PAUSE_MS: u64 = 100;
/// How long an allocation waits for a victim the reaper gave up on once nothing else may be killed. Linux panics
/// at once there ("System is deadlocked on memory"); the victim may still be leaving, so it gets this long.
const SKIPPED_WAIT_NS: u64 = 5_000_000_000;
/// Programs sharing the victim's memory signalled per hold of `proc.table` (`kill`).
const KILL_BATCH: usize = 16;

struct Victim {
    /// Weak: the caller of `out_of_memory` must never drop a process image (the last reference to its address
    /// space would end it inside an allocation; W35 MM-2's rule).
    p: Weak<Process>,
    /// The reaper sends SIGKILL (false: the program was ending by itself).
    signal: bool,
    /// The reaper took it up.
    taken: bool,
    /// Linux MMF_OOM_SKIP: reaped, or given up; the next call may choose again.
    skip: bool,
    /// When `skip` was set (ns since boot).
    skip_ns: u64,
    /// Its badness when chosen, for the log.
    points: i64,
}

static VICTIM: SpinLock<Option<Victim>> = SpinLock::named("proc.oom", None);
static WORK: ksync::WaitQueue = ksync::WaitQueue::new();
static DUE: AtomicBool = AtomicBool::new(false);

klog::counter!(REAPED_PAGES, "mm.oom.reaped_pages");
klog::counter!(REAP_GAVE_UP, "mm.oom.reap_gave_up");
klog::counter!(SELF_VICTIM, "mm.oom.self_victim");

struct Killer;

/// What the caller needs of the victim, read under its lock (the table's lock is never taken inside it).
#[derive(Clone, Copy)]
struct Seen {
    ptr: *const Process,
    skip: bool,
    skip_ns: u64,
}

fn seen() -> Option<Seen> {
    VICTIM.lock().as_ref().map(|v| Seen { ptr: v.p.as_ptr(), skip: v.skip, skip_ns: v.skip_ns })
}

/// The program `ptr` has ended (or is gone from the table), looked up without a reference of our own.
fn gone(ptr: *const Process) -> bool {
    TABLE.lock().iter().find(|q| core::ptr::eq(Arc::as_ptr(q), ptr)).is_none_or(|q| q.is_finished())
}

fn now_ns() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

fn choose(p: Weak<Process>, signal: bool, points: i64) {
    let old = VICTIM.lock().replace(Victim { p, signal, taken: false, skip: false, skip_ns: 0, points });
    // Only a weak reference goes here: its drop never ends a process image.
    drop(old);
    DUE.store(true, Release);
    WORK.wake_one();
}

impl mm_core::oom::Killer for Killer {
    fn out_of_memory(&self, _order: u32) -> Verdict {
        let v = seen();
        // Linux `task_will_free_mem(current)`: a program that is ending already is the victim; its threads take
        // the reserves (`current_dying`) and the reaper takes its memory. Not once the reaper gave up on it.
        if let Some(me) = crate::current().filter(|_| mm_core::oom::current_dying()) {
            let mine = v.is_some_and(|v| core::ptr::eq(v.ptr, Arc::as_ptr(&me)));
            if mine && v.is_some_and(|v| !v.skip) {
                return Verdict::Pending;
            }
            if !mine && v.is_none_or(|v| v.skip || gone(v.ptr)) {
                SELF_VICTIM.inc();
                choose(Arc::downgrade(&me), false, 0);
                return Verdict::Pending;
            }
        }
        if let Some(v) = v
            && !v.skip
            && !gone(v.ptr)
        {
            return Verdict::Pending;
        }
        let Some((p, points)) = select(v.map(|v| v.ptr)) else {
            // Nothing else to choose, but the victim the reaper gave up on may still be leaving: its memory comes
            // back when it has left. Bounded: past that the machine is deadlocked on memory, as Linux says.
            if let Some(v) = v
                && !gone(v.ptr)
                && now_ns().saturating_sub(v.skip_ns) < SKIPPED_WAIT_NS
            {
                return Verdict::Pending;
            }
            return Verdict::NoVictim;
        };
        choose(p, true, points);
        Verdict::Killed
    }

    fn current_dying(&self) -> bool {
        crate::task::current_thread().is_some_and(|t| t.should_stop())
    }
}

static KILLER: Killer = Killer;

/// Linux `in_vfork`: a child that runs on its parent's memory until its execve or end (vfork, or CLONE_VM):
/// the parent, which owns that memory, is the one to choose.
fn in_vfork(table: &[Arc<Process>], p: &Process) -> bool {
    !p.vfork_done.is_done() && table.iter().any(|q| q.pid == p.ppid() && q.pid != p.pid && Arc::ptr_eq(&q.mm, &p.mm))
}

/// Linux `select_bad_process`: the program with the highest badness, under the table's lock and without an
/// allocation. `skipped`: the last victim (the reaper gave up on it, or it ended), passed over.
fn select(skipped: Option<*const Process>) -> Option<(Weak<Process>, i64)> {
    let total = mm_core::frames().map_or(0, |f| f.stats().total_bytes / mm_core::PAGE);
    let table = TABLE.lock();
    // Killing a program whose memory init shares gives nothing back (Linux never reaps it, oom_kill.c:969).
    let init_mm = table.iter().find(|p| p.pid == 1).map(|p| Arc::as_ptr(&p.mm));
    let mut best: Option<(&Arc<Process>, i64)> = None;
    for p in table.iter() {
        if p.pid == 1 || skipped.is_some_and(|s| core::ptr::eq(s, Arc::as_ptr(p))) || p.is_finished() || p.group_exited() {
            continue;
        }
        if init_mm == Some(Arc::as_ptr(&p.mm)) || in_vfork(&table, p) {
            continue;
        }
        let pt = p.mm.page_table_pages();
        let Some(points) = badness(p.mm.rss_pages(), pt, p.oom_score_adj(), total) else { continue };
        if best.is_none_or(|(_, b)| points > b) {
            best = Some((p, points));
        }
    }
    best.map(|(p, points)| (Arc::downgrade(p), points))
}

/// Installs the killer and starts its thread (`proc::init`).
pub(crate) fn start() {
    if !mm_core::oom::set_killer(&KILLER) {
        return;
    }
    if sched::spawn("mm.oom_reaper", sched::Class::Interactive, reaper).is_err() {
        klog::event!(Error, "proc", "oom.no_reaper"; "the OOM reaper thread did not start: a victim's memory waits for its end");
    }
}

fn reaper() {
    // Its allocations take the reserves and never wait for the killer (that is this thread).
    if !mm_core::oom::mark_memalloc() {
        klog::event!(Warn, "proc", "oom.reaper_unmarked"; "the OOM reaper is not marked as a memory helper");
    }
    loop {
        WORK.wait_until_timeout(|| DUE.load(Acquire), 1_000_000_000);
        if !DUE.swap(false, AcqRel) {
            continue;
        }
        let Some((w, points, signal)) = VICTIM.lock().as_mut().filter(|v| !v.taken).map(|v| {
            v.taken = true;
            (v.p.clone(), v.points, v.signal)
        }) else {
            continue;
        };
        let reaped = match w.upgrade() {
            Some(p) => {
                let init_shares = kill(&p, points, signal);
                // Linux `can_oom_reap = false`: init runs on that memory.
                if init_shares { Some(0) } else { reap(&p) }
            }
            None => Some(0),
        };
        if let Some(v) = VICTIM.lock().as_mut().filter(|v| Weak::ptr_eq(&v.p, &w)) {
            v.skip = true;
            v.skip_ns = now_ns();
        }
        match reaped {
            Some(pages) => REAPED_PAGES.add(pages),
            None => {
                REAP_GAVE_UP.inc();
                klog::event!(Warn, "proc", "oom.reap_gave_up", tries = REAP_TRIES;
                    "the ended program's memory was held all the time: it comes back when the program has left");
            }
        }
    }
}

/// Linux `__oom_kill_process`: SIGKILL to the victim and to every other program that shares its memory, never to
/// init (true: init shares it, so nothing is reaped). The table is walked under its lock in batches of a fixed
/// array, not copied (no allocation in the reaper).
fn kill(p: &Arc<Process>, points: i64, signal: bool) -> bool {
    if signal {
        let rss_kb = p.mm.rss_pages() * 4;
        klog::event!(Warn, "proc", "oom.kill", pid = p.pid, name = p.name.as_str(), rss_kb = rss_kb, score_adj = p.oom_score_adj(), points = points;
            "out of memory: the program with the most memory is ended so the kernel can go on");
        p.send_signal(SIGKILL);
    } else {
        klog::event!(Warn, "proc", "oom.self_victim", pid = p.pid, name = p.name.as_str();
            "out of memory: the program was ending already, its memory is taken back");
    }
    let mut init_shares = false;
    // Each pass signals up to a batch; a signalled program counts as ending in the next pass. Bounded.
    for _ in 0..64 {
        let mut batch: [Option<Arc<Process>>; KILL_BATCH] = [const { None }; KILL_BATCH];
        let mut n = 0;
        for o in TABLE.lock().iter() {
            if o.pid == p.pid || !Arc::ptr_eq(&o.mm, &p.mm) || o.is_finished() || o.group_exited() {
                continue;
            }
            if o.pid == 1 {
                init_shares = true;
                continue;
            }
            if n < KILL_BATCH {
                batch[n] = Some(o.clone());
                n += 1;
            }
        }
        for o in batch.iter().flatten() {
            o.send_signal(SIGKILL);
        }
        if n < KILL_BATCH {
            break;
        }
    }
    init_shares
}

/// Linux `oom_reap_task`: up to ten tries, a tenth of a second apart, while the program has not ended by itself.
fn reap(p: &Arc<Process>) -> Option<u64> {
    for _ in 0..REAP_TRIES {
        if p.is_finished() {
            return Some(0);
        }
        if let Some(pages) = p.mm.oom_reap() {
            return Some(pages);
        }
        sched::sleep_ms(REAP_PAUSE_MS);
    }
    None
}

/// Tests: the choice the killer would make now (pid and badness), without a kill.
#[cfg(not(bitsos_no_ktests))]
pub(crate) fn choice_for_test() -> Option<(u32, i64)> {
    select(None).and_then(|(p, s)| p.upgrade().map(|p| (p.pid, s)))
}

/// Tests: the victim now: its pid, whether the reaper took it up, and the reaper's mark.
#[cfg(not(bitsos_no_ktests))]
pub(crate) fn victim_for_test() -> Option<(u32, bool, bool)> {
    let (w, taken, skip) = VICTIM.lock().as_ref().map(|v| (v.p.clone(), v.taken, v.skip))?;
    w.upgrade().map(|p| (p.pid, taken, skip))
}

/// Tests: whether the killer would pass over `p` (init's memory, a vfork child).
#[cfg(not(bitsos_no_ktests))]
pub(crate) fn passed_over_for_test(p: &Arc<Process>) -> bool {
    let table = TABLE.lock();
    let init_mm = table.iter().find(|q| q.pid == 1).map(|q| Arc::as_ptr(&q.mm));
    init_mm == Some(Arc::as_ptr(&p.mm)) || in_vfork(&table, p)
}
