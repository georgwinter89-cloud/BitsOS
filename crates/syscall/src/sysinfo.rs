//! sysinfo(2) (AB-5; linux-6.6/kernel/sys.c `do_sysinfo` :2802, `SYSCALL_DEFINE1(sysinfo)`) and the load
//! average behind it (kernel/sched/loadavg.c: `calc_load`, EXP_1/5/15, every LOAD_FREQ = 5 s).
//!
//! - uptime: seconds of the boot clock, rounded up when there is a fraction (`tp.tv_sec + (tp.tv_nsec ? 1 :
//!   0)`); V2's BOOTTIME is its monotonic clock.
//! - loads: `avenrun` << (SI_LOAD_SHIFT - FSHIFT), from `loadavg`'s sampler thread (class Background) that
//!   counts the runnable threads every 5 s like Linux's `calc_global_load` (each CPU's queue plus the thread it
//!   runs, without the sampler itself; V2 has no uninterruptible sleep state to add).
//! - totalram/freeram: the frame allocator's numbers; sharedram: tmpfs and /dev/shm pages (Linux NR_SHMEM);
//!   bufferram 0 (no block-device page cache count); no swap, no high memory. Linux shifts the units to bytes
//!   when they fit (always on 64-bit): mem_unit 1.
//! - procs: every scheduler thread (Linux `nr_threads` counts kernel threads too).

use crate::SysCtx;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;

const FSHIFT: u32 = 11;
const FIXED_1: u64 = 1 << FSHIFT;
const EXP_1: u64 = 1884;
const EXP_5: u64 = 2014;
const EXP_15: u64 = 2037;
/// `SI_LOAD_SHIFT`.
const SI_LOAD_SHIFT: u32 = 16;
/// LOAD_FREQ: 5 s (+1 tick on Linux).
const LOAD_FREQ_NS: u64 = 5_000_000_000 + 1_000_000;
/// `sizeof(struct sysinfo)` on x86-64.
pub(crate) const SYSINFO_SIZE: usize = 112;

static AVENRUN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// `calc_load` (fixed point, FSHIFT bits): one step of the exponential average.
pub(crate) fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut newload = load * exp + active * (FIXED_1 - exp);
    if active >= load {
        newload += FIXED_1 - 1;
    }
    newload / FIXED_1
}

/// Runnable threads now: each online CPU's queue plus the thread it runs (`nr_running`), minus the caller.
fn nr_active() -> u64 {
    let n: u64 = sched::cpu_stats().iter().filter(|c| c.online).map(|c| c.queued as u64 + if c.idle { 0 } else { 1 }).sum();
    n.saturating_sub(1)
}

/// One sample (`calc_global_load`).
pub(crate) fn sample(active: u64) {
    let a = active * FIXED_1;
    for (slot, exp) in AVENRUN.iter().zip([EXP_1, EXP_5, EXP_15]) {
        let old = slot.load(Ordering::Relaxed);
        slot.store(calc_load(old, exp, a), Ordering::Relaxed);
    }
}

/// The three load averages in FSHIFT fixed point (for /proc/loadavg: FS change request).
pub fn avenrun() -> [u64; 3] {
    [AVENRUN[0].load(Ordering::Relaxed), AVENRUN[1].load(Ordering::Relaxed), AVENRUN[2].load(Ordering::Relaxed)]
}

/// Starts the sampler (kernel init, from `syscall::init`).
pub(crate) fn start_sampler() {
    let r = sched::spawn("loadavg", sched::Class::Background, || loop {
        sched::sleep_ns(LOAD_FREQ_NS);
        sample(nr_active());
    });
    if r.is_err() {
        klog::event!(Warn, "syscall", "loadavg.no_thread"; "no thread for the load average: sysinfo reports 0 loads");
    }
}

/// `struct sysinfo` from its parts (pure, host-tested).
#[allow(clippy::too_many_arguments)]
pub(crate) fn sysinfo_bytes(uptime_ns: u64, loads: [u64; 3], total: u64, free: u64, shared: u64, procs: u64) -> [u8; SYSINFO_SIZE] {
    let mut b = [0u8; SYSINFO_SIZE];
    let mut put = |off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    let secs = uptime_ns / 1_000_000_000 + if uptime_ns % 1_000_000_000 != 0 { 1 } else { 0 };
    put(0, secs);
    for (i, l) in loads.iter().enumerate() {
        put(8 + i * 8, l << (SI_LOAD_SHIFT - FSHIFT));
    }
    put(32, total);
    put(40, free);
    put(48, shared);
    // bufferram 56, totalswap 64, freeswap 72: 0.
    // procs (u16) at 80: Linux stores nr_threads into an unsigned short (it wraps).
    b[80..82].copy_from_slice(&(procs as u16).to_le_bytes());
    // totalhigh 88, freehigh 96: 0; mem_unit (u32) at 104: bytes.
    b[104..108].copy_from_slice(&1u32.to_le_bytes());
    b
}

fn sys_sysinfo(c: &mut SysCtx<'_>) -> i64 {
    let (total, free) = mm_core::frames().map(|f| f.stats()).map_or((0, 0), |s| (s.total_bytes, s.free_bytes));
    let shared = vfs::inode::tmpfs_pages_used() * 4096;
    let b = sysinfo_bytes(time::now_ns(), avenrun(), total, free, shared, sched::thread_count() as u64);
    c.proc.mm.copy_to_user(c.args[0], &b).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall_nr!(uapi::ab::nr_more::SYSINFO, "sysinfo", sys_sysinfo);

#[cfg(test)]
mod tests {
    use super::*;

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn layout_as_x86_64() {
        let b = sysinfo_bytes(5_000_000_001, [FIXED_1, 2 * FIXED_1, 0], 1 << 35, 1 << 30, 4096, 70_000);
        assert_eq!(u64_at(&b, 0), 6, "uptime rounds a fraction up");
        assert_eq!(u64_at(&b, 8), 1 << 16, "1.00 in SI_LOAD_SHIFT");
        assert_eq!(u64_at(&b, 16), 2 << 16);
        assert_eq!((u64_at(&b, 32), u64_at(&b, 40), u64_at(&b, 48)), (1 << 35, 1 << 30, 4096));
        assert_eq!(u16::from_le_bytes([b[80], b[81]]), (70_000u64 as u16), "procs wraps like an unsigned short");
        assert_eq!(u32::from_le_bytes(b[104..108].try_into().unwrap()), 1);
        assert_eq!(sysinfo_bytes(3_000_000_000, [0; 3], 0, 0, 0, 0)[0], 3, "whole seconds stay");
    }

    #[test]
    fn calc_load_as_linux() {
        // One runnable task from idle: the 1-minute average after one step is 1 - EXP_1/FIXED_1, rounded up.
        let l = calc_load(0, EXP_1, FIXED_1);
        assert_eq!(l, (FIXED_1 * (FIXED_1 - EXP_1) + FIXED_1 - 1) / FIXED_1);
        // Steady load converges to it; idle decays to 0.
        let mut x = 0;
        for _ in 0..200 {
            x = calc_load(x, EXP_1, 3 * FIXED_1);
        }
        assert_eq!(x, 3 * FIXED_1);
        for _ in 0..400 {
            x = calc_load(x, EXP_1, 0);
        }
        assert_eq!(x, 0);
    }
}
