//! FD-1 measuring build only (`--cfg bitsos_fd_measure`): a user-mode program-counter sampler. Every interrupt
//! that arrives while user code of the target address space runs (CR3 = `TARGET`) records the interrupted
//! user rip and FS base (one per thread) into a ring; `sched::prof_kick` sends the interrupts. Never in a
//! release build.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const RING: usize = 1 << 16;
/// The FS base recorded for samples that interrupted kernel code (on behalf of the target space).
pub const KERNEL: u64 = u64::MAX;
static TARGET: AtomicU64 = AtomicU64::new(0);
static HEAD: AtomicUsize = AtomicUsize::new(0);
static RIPS: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static FSB: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static RSI: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static RDI: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];

/// Starts sampling the address space with page-table root `cr3` and forgets earlier samples; 0 stops and keeps them.
pub fn target(cr3: u64) {
    TARGET.store(0, Ordering::SeqCst);
    if cr3 != 0 {
        HEAD.store(0, Ordering::SeqCst);
        for c in &SYSCALLS {
            c.store(0, Ordering::Relaxed);
        }
        TARGET.store(cr3 & !0xFFF, Ordering::SeqCst);
    }
}

static SYSCALLS: [AtomicU64; 512] = [const { AtomicU64::new(0) }; 512];

/// Called for every syscall (thread context, `UserContext::execute_or_stop`): counts it by number while the
/// target space runs.
pub(crate) fn syscall(nr: u64) {
    let t = TARGET.load(Ordering::Relaxed);
    if t != 0 && crate::cpu::read_cr3() & 0x000F_FFFF_FFFF_F000 == t {
        SYSCALLS[(nr as usize).min(511)].fetch_add(1, Ordering::Relaxed);
    }
}

/// Syscall counts of the target space since the last `target` (number, count), non-zero only.
pub fn syscalls() -> alloc::vec::Vec<(usize, u64)> {
    SYSCALLS.iter().enumerate().map(|(i, c)| (i, c.load(Ordering::Relaxed))).filter(|(_, c)| *c > 0).collect()
}

/// Called after an interrupt in user mode (thread context, `UserContext::execute_or_stop`).
pub(crate) fn note(rip: u64, fs_base: u64, rsi: u64, rdi: u64) {
    let t = TARGET.load(Ordering::Relaxed);
    if t == 0 || crate::cpu::read_cr3() & 0x000F_FFFF_FFFF_F000 != t {
        return;
    }
    let i = HEAD.fetch_add(1, Ordering::Relaxed);
    if i < RING {
        RIPS[i].store(rip, Ordering::Relaxed);
        FSB[i].store(fs_base, Ordering::Relaxed);
        RSI[i].store(rsi, Ordering::Relaxed);
        RDI[i].store(rdi, Ordering::Relaxed);
    }
}

/// The samples taken so far: (rip, fs_base).
pub fn samples() -> alloc::vec::Vec<(u64, u64)> {
    let n = HEAD.load(Ordering::Acquire).min(RING);
    (0..n).map(|i| (RIPS[i].load(Ordering::Relaxed), FSB[i].load(Ordering::Relaxed))).collect()
}

/// The source and destination registers (rsi, rdi) of the samples whose rip lies in [lo, hi): where a copy
/// loop reads and writes.
pub fn operands(lo: u64, hi: u64) -> alloc::vec::Vec<(u64, u64)> {
    let n = HEAD.load(Ordering::Acquire).min(RING);
    (0..n)
        .filter(|&i| (lo..hi).contains(&RIPS[i].load(Ordering::Relaxed)))
        .map(|i| (RSI[i].load(Ordering::Relaxed), RDI[i].load(Ordering::Relaxed)))
        .collect()
}
