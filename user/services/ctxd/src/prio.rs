//! B4/B5: indexing is Background work. On V2 `SCHED_IDLE` is the Background class (SC-7a), on Linux the idle
//! policy: such a thread runs only when nothing else wants the processor, so a game in front does not feel
//! it. Threads and child processes inherit the policy, so the indexer thread passes it on to its helper threads
//! and extraction workers, and llama-server is started from a thread that took it first. `Quiet` makes the
//! embedding loop wait while the machine is busy with other work.

use std::time::{Duration, Instant};

/// The calling thread becomes Background. False when the kernel refused (the caller logs it once).
pub fn background() -> bool {
    usys::sched_setscheduler(0, usys::SCHED_IDLE, 0).is_ok()
}

/// Runs `f` on a fresh Background thread and waits for it: what `f` starts inherits the class.
pub fn in_background<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        let h = s.spawn(|| {
            if !background() {
                crate::event!("warn", "prio.refused", class = "background");
            }
            f()
        });
        match h.join() {
            Ok(v) => v,
            Err(p) => std::panic::resume_unwind(p),
        }
    })
}

/// How often the embedding loop looks whether the machine is busy, how long it looks, and how much of the
/// machine must be idle (while ctxd itself does nothing) for indexing to go on.
const CHECK_EVERY: Duration = Duration::from_secs(5);
const LOOK: Duration = Duration::from_millis(400);
const WAIT: Duration = Duration::from_secs(2);
const MIN_IDLE: f64 = 0.5;

/// Busy and idle jiffies of all CPUs together (the first line of /proc/stat; idle includes iowait).
fn cpu_times() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let f: Vec<u64> = s.lines().next()?.strip_prefix("cpu ")?.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    let idle = f.get(3)? + f.get(4).copied().unwrap_or(0);
    Some((f.iter().take(8).sum::<u64>(), idle))
}

/// The idle share of the machine over `LOOK` (None: /proc/stat unreadable or no tick passed).
fn idle_share() -> Option<f64> {
    let (t0, i0) = cpu_times()?;
    std::thread::sleep(LOOK);
    let (t1, i1) = cpu_times()?;
    (t1 > t0).then(|| (i1 - i0) as f64 / (t1 - t0) as f64)
}

/// B5 on top of the Background class: V2's balancing places threads by count (QEMU, 12.11.: two SCHED_IDLE
/// llama threads still took a whole CPU from four busy Normal shells on four CPUs), so the embedding loop also
/// steps aside by itself. Between two requests, at most every `CHECK_EVERY`, it looks while ctxd does nothing;
/// when less than half the machine is idle it waits until it is.
pub struct Quiet {
    last: std::sync::Mutex<Option<Instant>>,
}

pub static QUIET: Quiet = Quiet { last: std::sync::Mutex::new(None) };

impl Quiet {
    pub fn between(&self) {
        let Ok(mut last) = self.last.lock() else { return };
        if last.is_some_and(|l| l.elapsed() < CHECK_EVERY) {
            return;
        }
        let mut paused = None;
        while idle_share().is_some_and(|s| s < MIN_IDLE) {
            if paused.is_none() {
                paused = Some(Instant::now());
                crate::event!("info", "index.paused", reason = "machine_busy");
            }
            std::thread::sleep(WAIT);
        }
        if let Some(p) = paused {
            crate::event!("info", "index.resumed", waited_s = p.elapsed().as_secs());
        }
        *last = Some(Instant::now());
    }
}

/// The scheduling policy of a thread of this process as /proc shows it (field 41 of `stat`), for tests.
pub fn policy_of(tid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
    s.get(s.rfind(')')? + 2..)?.split(' ').nth(38)?.parse().ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_machine_s_idle_share_is_readable() {
        assert!(super::idle_share().is_some_and(|s| (0.0..=1.0).contains(&s)));
    }

    #[test]
    fn a_background_thread_keeps_the_class_for_what_it_starts() {
        let child_policy = super::in_background(|| {
            std::thread::spawn(|| {
                let tid = std::fs::read_link("/proc/thread-self").ok()?;
                super::policy_of(tid.file_name()?.to_str()?.parse().ok()?)
            })
            .join()
            .ok()
            .flatten()
        });
        assert_eq!(child_policy, Some(usys::SCHED_IDLE as u32));
        let tid: u32 = std::fs::read_link("/proc/thread-self").unwrap().file_name().unwrap().to_str().unwrap().parse().unwrap();
        assert_eq!(super::policy_of(tid), Some(0), "the caller's own thread stays normal");
    }
}
