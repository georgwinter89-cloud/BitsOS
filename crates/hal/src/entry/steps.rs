//! AB-15, measuring build only (`--cfg bitsos_fd_measure`, never in a release kernel): TSC cycles per step of
//! the syscall path, summed between `start` and `stop`, and cycle costs of the single instructions the entry
//! assembly uses. `mark(step)` charges the cycles since the previous mark to `step`, so the marks along the
//! path split one round (user mode and back) into its parts; the bookkeeping of a mark lands in the next
//! step, `EMPTY` measures it (two marks with nothing between). One global chain: measure one thread only.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const ENTER: usize = 0;
pub const EXEC: usize = 1;
pub const EXIT: usize = 2;
pub const SECCOMP: usize = 3;
pub const TRACE_START: usize = 4;
pub const LOOKUP: usize = 5;
pub const HANDLER: usize = 6;
pub const TRACE_DONE: usize = 7;
pub const SYS_EXIT: usize = 8;
pub const DISPATCH: usize = 9;
pub const NEW_IMAGE: usize = 10;
pub const RESCHED: usize = 11;
pub const EMPTY: usize = 12;
const N: usize = 13;
pub const NAMES: [&str; N] = [
    "user_enter", "execute", "user_exit", "seccomp", "trace_start", "lookup+count", "handler", "trace_done",
    "syscall_exit", "dispatch_call", "new_image", "cond_resched", "empty_mark",
];

static ON: AtomicBool = AtomicBool::new(false);
/// Page-table root of the measured address space: marks of other programs' threads are ignored.
static TARGET: AtomicU64 = AtomicU64::new(0);
const ROOT_MASK: u64 = 0x000F_FFFF_FFFF_F000;
static SUM: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static CNT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
/// TSC of the previous mark (0: none yet).
static LAST: AtomicU64 = AtomicU64::new(0);

/// Forgets earlier sums and starts summing the marks made while the address space with root `cr3` is active.
pub fn start(cr3: u64) {
    TARGET.store(cr3 & ROOT_MASK, Ordering::Relaxed);
    for i in 0..N {
        SUM[i].store(0, Ordering::Relaxed);
        CNT[i].store(0, Ordering::Relaxed);
    }
    LAST.store(0, Ordering::Relaxed);
    ON.store(true, Ordering::SeqCst);
}

pub fn stop() {
    ON.store(false, Ordering::SeqCst);
}

/// Charges the cycles since the previous mark to `step`.
#[inline(always)]
pub fn mark(step: usize) {
    if !ON.load(Ordering::Relaxed) || crate::cpu::read_cr3() & ROOT_MASK != TARGET.load(Ordering::Relaxed) {
        return;
    }
    let now = crate::cpu::rdtsc();
    let last = LAST.swap(now, Ordering::Relaxed);
    if last != 0 && step < N {
        SUM[step].fetch_add(now.wrapping_sub(last), Ordering::Relaxed);
        CNT[step].fetch_add(1, Ordering::Relaxed);
    }
}

/// (name, calls, mean cycles) per step with calls.
pub fn results() -> alloc::vec::Vec<(&'static str, u64, u64)> {
    (0..N)
        .filter_map(|i| {
            let c = CNT[i].load(Ordering::Relaxed);
            (c > 0).then(|| (NAMES[i], c, SUM[i].load(Ordering::Relaxed) / c))
        })
        .collect()
}

const REPS: u64 = 4096;

/// Mean cycles of `f` over `REPS` calls with interrupts off, minus an empty loop's.
pub fn cycles(f: impl Fn()) -> u64 {
    let _irq = crate::cpu::IrqGuard::new();
    let base = {
        let t0 = crate::cpu::rdtsc_ordered();
        for _ in 0..REPS {
            core::hint::black_box(());
        }
        crate::cpu::rdtsc_ordered() - t0
    };
    let t0 = crate::cpu::rdtsc_ordered();
    for _ in 0..REPS {
        f();
    }
    let d = crate::cpu::rdtsc_ordered() - t0;
    d.saturating_sub(base) / REPS
}

/// Cycles of the instructions on the null-syscall path, one by one. `swapgs`: also the swapgs pairs. Only the
/// QemuOnly ktest asks for them, out of caution: with IF off nothing but NMI, #MC and #DF can arrive inside a
/// pair, and those take their GS base from the APIC ID (`percpu::paranoid_gs_enter`), so the pairs would be
/// sound on real hardware too; the pairs touch no memory that could fault.
pub fn instructions(swapgs: bool) -> alloc::vec::Vec<(&'static str, u64)> {
    let mut v = alloc::vec::Vec::new();
    let fsgs = super::user_fsgsbase();
    v.push(("rdtsc", cycles(|| {
        core::hint::black_box(crate::cpu::rdtsc());
    })));
    v.push(("pushfq+cli+popfq", cycles(|| {
        // SAFETY: saves and restores RFLAGS around cli; the stack is balanced.
        unsafe { core::arch::asm!("pushfq", "cli", "popfq") };
    })));
    v.push(("pushfq+cli+pop+test+sti", cycles(|| {
        // SAFETY: saves RFLAGS, clears IF and runs sti only when the saved IF was set. `cycles` runs with
        // interrupts off, so the jz is always taken, sti never runs and IF stays clear; pushfq/pop balance.
        unsafe { core::arch::asm!("pushfq", "cli", "pop {0}", "test {0:e}, 0x200", "jz 2f", "sti", "2:", out(reg) _) };
    })));
    v.push(("lock xadd (own line)", cycles(|| {
        static X: AtomicU64 = AtomicU64::new(0);
        X.fetch_add(1, Ordering::Relaxed);
    })));
    v.push(("mov ss,ax + cmp", cycles(|| {
        // SAFETY: reads the SS selector only.
        unsafe { core::arch::asm!("mov {0:x}, ss", "cmp {0:x}, 0x10", out(reg) _, options(nomem, nostack)) };
    })));
    if fsgs {
        v.push(("rdfsbase", cycles(|| {
            // SAFETY: CR4.FSGSBASE is on (`user_fsgsbase`); reading the FS base changes nothing.
            unsafe { core::arch::asm!("rdfsbase {}", out(reg) _, options(nomem, nostack)) };
        })));
        v.push(("rdfsbase+wrfsbase (same value)", cycles(|| {
            // SAFETY: writes back the value just read, so FS is unchanged (the kernel does not use FS).
            unsafe { core::arch::asm!("rdfsbase {0}", "wrfsbase {0}", out(reg) _, options(nomem, nostack)) };
        })));
    }
    if fsgs && swapgs {
        v.push(("swapgs+rdgsbase+swapgs", cycles(|| {
            // SAFETY: interrupts are off (`cycles`); the two swapgs leave the kernel GS base active again.
            unsafe { core::arch::asm!("swapgs", "rdgsbase {}", "swapgs", out(reg) _, options(nomem, nostack)) };
        })));
        v.push(("swapgs+rdgsbase+wrgsbase+swapgs", cycles(|| {
            // SAFETY: as above; the inactive GS base is written back unchanged.
            unsafe { core::arch::asm!("swapgs", "rdgsbase {0}", "wrgsbase {0}", "swapgs", out(reg) _, options(nomem, nostack)) };
        })));
    }
    v.push(("gdt::set_rsp0 (call)", cycles(|| {
        // Any value: every return to user writes rsp0 again before ring 3 can use it (`hal_user_execute`).
        crate::gdt::set_rsp0(core::hint::black_box(0xFFFF_8000_0000_0000));
    })));
    if swapgs {
        v.push(("swapgs+swapgs", cycles(|| {
            // SAFETY: interrupts are off; the pair leaves the kernel GS base active.
            unsafe { core::arch::asm!("swapgs", "swapgs", options(nomem, nostack)) };
        })));
    }
    v
}
