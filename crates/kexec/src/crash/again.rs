//! CRASHDISK: panics of other CPUs while one CPU reports. The kernel's panic handler stops such a CPU quietly
//! (its message would interleave with the report); before that it hands the message here, into lock-free slots,
//! and the report puts them into the record after the other CPUs. A hang the watchdog ends becomes a panic with
//! the `HANG` mark (`hang_panic`, fch-wdt's reporter).

use super::text::{Buf, OneLine};
use core::fmt::Write;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const SLOTS: usize = 8;
const LEN: usize = 240;

struct Slot {
    /// 0 free, 1 being written, 2 ready.
    state: AtomicU8,
    cpu: AtomicUsize,
    len: AtomicUsize,
    bytes: [AtomicU8; LEN],
}

static AGAIN: [Slot; SLOTS] = [const { Slot { state: AtomicU8::new(0), cpu: AtomicUsize::new(0), len: AtomicUsize::new(0), bytes: [const { AtomicU8::new(0) }; LEN] } }; SLOTS];
/// Panics seen on other CPUs (also those beyond the slots).
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// The CPU that turned a hang into a panic, plus 1 (0: none): only that CPU's report is a hang's. Another CPU's
/// panic that wins the report meanwhile is an ordinary one.
static HANG_CPU: AtomicUsize = AtomicUsize::new(0);

/// Keeps a panic message of `cpu` (another CPU than the reporting one). No lock, no heap.
pub fn note(cpu: usize, info: &core::panic::PanicInfo<'_>) {
    let n = COUNT.fetch_add(1, Ordering::AcqRel);
    let Some(s) = AGAIN.get(n) else { return };
    if s.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    let mut b = Buf::<LEN>::new();
    let _ = write!(OneLine(&mut b), "{info}");
    for (d, &x) in s.bytes.iter().zip(b.as_bytes()) {
        d.store(x, Ordering::Relaxed);
    }
    s.cpu.store(cpu, Ordering::Relaxed);
    s.len.store(b.len(), Ordering::Relaxed);
    s.state.store(2, Ordering::Release);
}

/// Each kept message as a record line; the count of panics seen on other CPUs.
pub fn lines(f: &mut dyn FnMut(&str)) -> usize {
    for s in AGAIN.iter().filter(|s| s.state.load(Ordering::Acquire) == 2) {
        let mut raw = [0u8; LEN];
        let len = s.len.load(Ordering::Relaxed).min(LEN);
        for (d, x) in raw.iter_mut().zip(&s.bytes) {
            *d = x.load(Ordering::Relaxed);
        }
        let msg = core::str::from_utf8(&raw[..len]).unwrap_or("(not text)");
        let mut l = Buf::<320>::new();
        let _ = write!(l, "again: cpu {} panicked too: {msg}", s.cpu.load(Ordering::Relaxed));
        f(l.as_str());
    }
    COUNT.load(Ordering::Acquire)
}

/// The panic `cpu` reports is a hang the watchdog would have ended.
pub fn hang(cpu: usize) -> bool {
    HANG_CPU.load(Ordering::Acquire) == cpu + 1
}

/// Marks `cpu`'s coming panic as a hang's (the first mark stays).
pub(super) fn mark_hang(cpu: usize) {
    let _ = HANG_CPU.compare_exchange(0, cpu + 1, Ordering::AcqRel, Ordering::Acquire);
}

/// fch-wdt: a stall the watchdog's reset is about to end becomes a panic, so its report (every CPU by NMI, the
/// log) is written like any panic's, to RAM and the boot ESP (Linux 7.3.1 kernel/watchdog.c: `hardlockup_panic`).
pub fn hang_panic(why: &str) -> ! {
    mark_hang(hal::percpu::this_cpu());
    panic!("watchdog: {why}")
}
