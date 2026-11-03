//! BitsOS V2's vDSO (AB-14): the clock functions programs call without entering the kernel, as Linux's x86-64
//! vDSO exports them (linux-6.6/arch/x86/entry/vdso/vdso.lds.S: `__vdso_clock_gettime`, `__vdso_gettimeofday`,
//! `__vdso_time`, `__vdso_getcpu`, `__vdso_clock_getres` and the plain names, version LINUX_2.6; behaviour as
//! lib/vdso/gettimeofday.c and vgetcpu.c). Own code.
//!
//! User code: it runs in the program's mode and stack, mapped read+exec into every process right after the
//! read-only vvar page the kernel keeps current (`../src/vdso/layout.rs`). Built by `crates/proc/build.rs` for
//! x86_64-unknown-none (soft-float: no SSE/AVX anywhere), linked position-independent with no relocations.
//!
//! Every answer equals the syscall's: the same clocks (REALTIME/REALTIME_COARSE/TAI are the wall clock,
//! MONOTONIC/MONOTONIC_RAW/MONOTONIC_COARSE/BOOTTIME the TSC clock, as `crates/syscall/src/clock.rs` decodes
//! them), the same arithmetic as the time crate's `Snapshot::ns_at` (a TSC behind the base reads as the base).
//! Anything else — another clock, the page not published, a TSC the kernel marked unusable, getcpu without
//! rdpid/rdtscp — makes the real syscall, so errors are the kernel's own.

#![no_std]
#![allow(dead_code)] // layout.rs has constants only the kernel side uses

use core::arch::{asm, global_asm};
use core::ptr::read_volatile;
use core::sync::atomic::{Ordering, compiler_fence};

include!("../src/vdso/layout.rs");

const NSEC: u64 = 1_000_000_000;

const SYS_GETTIMEOFDAY: u64 = 96;
const SYS_TIME: u64 = 201;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_CLOCK_GETRES: u64 = 229;
const SYS_GETCPU: u64 = 309;

#[repr(C)]
pub struct Timespec {
    sec: i64,
    nsec: i64,
}

#[repr(C)]
pub struct Timeval {
    sec: i64,
    usec: i64,
}

#[repr(C)]
pub struct Timezone {
    minuteswest: i32,
    dsttime: i32,
}

/// The vvar page: the page before this image's ELF header.
#[inline(always)]
fn vvar() -> *const u8 {
    let p: *const u8;
    // SAFETY: only computes an address (the linker's hidden `__ehdr_start` is the image's first byte).
    unsafe { asm!("lea {}, [rip + __ehdr_start]", out(reg) p, options(pure, nomem, nostack, preserves_flags)) };
    p.wrapping_sub(VVAR_SIZE)
}

#[inline(always)]
fn r32(off: usize) -> u32 {
    // SAFETY: the kernel maps the vvar page readable before the image whenever it maps the image; `off` is a
    // naturally aligned field of `layout.rs`.
    unsafe { read_volatile(vvar().wrapping_add(off) as *const u32) }
}

#[inline(always)]
fn r64(off: usize) -> u64 {
    // SAFETY: as `r32`.
    unsafe { read_volatile(vvar().wrapping_add(off) as *const u64) }
}

/// The TSC, ordered after the loads before it (Linux `rdtsc_ordered`: rdtscp, else lfence + rdtsc). No `nomem`:
/// the asm is a compiler barrier between the sequence reads.
#[inline(always)]
fn tsc(flags: u32) -> u64 {
    let (lo, hi): (u32, u32);
    if flags & F_RDTSCP != 0 {
        // SAFETY: rdtscp exists (F_RDTSCP) and is unprivileged (the kernel keeps CR4.TSD clear).
        unsafe { asm!("rdtscp", out("eax") lo, out("edx") hi, out("ecx") _, options(nostack, preserves_flags)) };
    } else {
        // SAFETY: lfence and rdtsc are unprivileged x86-64 instructions.
        unsafe { asm!("lfence", "rdtsc", out("eax") lo, out("edx") hi, options(nostack, preserves_flags)) };
    }
    (hi as u64) << 32 | lo as u64
}

/// `ticks * mult >> SHIFT`, saturating (the time crate's `clock::scale`).
#[inline(always)]
fn scale(ticks: u64, mult: u64) -> u64 {
    let v = (ticks as u128 * mult as u128) >> SHIFT;
    if v > u64::MAX as u128 { u64::MAX } else { v as u64 }
}

/// Which base a clock id reads, None for clocks only the syscall serves.
#[derive(Clone, Copy)]
enum Base {
    Mono,
    Boot,
    Real,
    Tai,
}

fn base_of(clk: i32) -> Option<Base> {
    match clk {
        0 | 5 => Some(Base::Real),      // REALTIME, REALTIME_COARSE
        1 | 4 | 6 => Some(Base::Mono),  // MONOTONIC, MONOTONIC_RAW, MONOTONIC_COARSE
        7 => Some(Base::Boot),          // BOOTTIME
        11 => Some(Base::Tai),          // TAI
        _ => None,
    }
}

/// Rounds of the sequence loop before the syscall answers instead (a few milliseconds of `pause`).
const SPIN_ROUNDS: u32 = 1 << 16;

/// The clock's reading in ns, None when the vDSO must not answer (Linux `do_hres` returning -1).
#[inline(always)]
fn read_ns(b: Base) -> Option<u64> {
    // A writer keeps interrupts off while the count is odd, so a reader waits nanoseconds. One that cannot finish
    // (a vCPU the host stopped mid-write, a CPU that died there) sends the reader to the syscall after this many
    // rounds instead of spinning for ever (Linux spins); the kernel's clock does not depend on the page.
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        if rounds > SPIN_ROUNDS {
            return None;
        }
        let seq = r32(SEQ);
        if seq & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        compiler_fence(Ordering::SeqCst);
        if r32(MODE) != MODE_TSC {
            return None;
        }
        let t = tsc(r32(FLAGS));
        let base_tsc = r64(BASE_TSC);
        let base_ns = r64(BASE_NS);
        let mult = r64(MULT);
        let off = match b {
            Base::Mono => 0,
            Base::Boot => r64(BOOT_OFF),
            Base::Real => r64(REAL_OFF),
            Base::Tai => r64(REAL_OFF).wrapping_add(r64(TAI_OFF)),
        };
        compiler_fence(Ordering::SeqCst);
        if r32(SEQ) != seq {
            continue;
        }
        let mono = base_ns.saturating_add(scale(t.saturating_sub(base_tsc), mult));
        return Some(mono.wrapping_add(off));
    }
}

#[inline(always)]
fn syscall2(nr: u64, a: u64, b: u64) -> i64 {
    let r: i64;
    // SAFETY: a plain system call; the kernel checks the pointers.
    unsafe { asm!("syscall", inlateout("rax") nr as i64 => r, in("rdi") a, in("rsi") b, lateout("rcx") _, lateout("r11") _, options(nostack)) };
    r
}

#[inline(always)]
fn syscall3(nr: u64, a: u64, b: u64, c: u64) -> i64 {
    let r: i64;
    // SAFETY: as `syscall2`.
    unsafe {
        asm!("syscall", inlateout("rax") nr as i64 => r, in("rdi") a, in("rsi") b, in("rdx") c, lateout("rcx") _, lateout("r11") _, options(nostack))
    };
    r
}

/// # Safety
/// `ts` must be writable (as for the syscall; a bad pointer faults here instead of EFAULT, as on Linux).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_clock_gettime(clk: i32, ts: *mut Timespec) -> i32 {
    if let Some(ns) = base_of(clk).and_then(read_ns) {
        // SAFETY: the caller's pointer (see above).
        unsafe { ts.write(Timespec { sec: (ns / NSEC) as i64, nsec: (ns % NSEC) as i64 }) };
        return 0;
    }
    syscall2(SYS_CLOCK_GETTIME, clk as i64 as u64, ts as u64) as i32
}

/// # Safety
/// `tv` and `tz` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_gettimeofday(tv: *mut Timeval, tz: *mut Timezone) -> i32 {
    if !tv.is_null() {
        let Some(ns) = read_ns(Base::Real) else {
            return syscall2(SYS_GETTIMEOFDAY, tv as u64, tz as u64) as i32;
        };
        // SAFETY: the caller's pointer.
        unsafe { tv.write(Timeval { sec: (ns / NSEC) as i64, usec: ((ns % NSEC) / 1000) as i64 }) };
    }
    if !tz.is_null() {
        // V2's sys_tz is all zeros (nothing sets it), as the syscall answers.
        // SAFETY: the caller's pointer.
        unsafe { tz.write(Timezone { minuteswest: 0, dsttime: 0 }) };
    }
    0
}

/// # Safety
/// `t` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_time(t: *mut i64) -> i64 {
    let Some(ns) = read_ns(Base::Real) else {
        return syscall2(SYS_TIME, t as u64, 0);
    };
    let s = (ns / NSEC) as i64;
    if !t.is_null() {
        // SAFETY: the caller's pointer.
        unsafe { t.write(s) };
    }
    s
}

/// Answers from IA32_TSC_AUX whatever the clock mode (Linux `__vdso_getcpu`), the syscall without rdpid/rdtscp.
///
/// # Safety
/// `cpu` and `node` null or writable; `cache` is unused (as on Linux since 2.6.24).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_getcpu(cpu: *mut u32, node: *mut u32, cache: *mut u8) -> i64 {
    let flags = r32(FLAGS);
    if flags & F_GETCPU == 0 || flags & (F_RDPID | F_RDTSCP) == 0 {
        return syscall3(SYS_GETCPU, cpu as u64, node as u64, cache as u64);
    }
    let aux: u64;
    if flags & F_RDPID != 0 {
        // SAFETY: rdpid exists (F_RDPID).
        unsafe { asm!("rdpid {}", out(reg) aux, options(nomem, nostack, preserves_flags)) };
    } else {
        let c: u32;
        // SAFETY: rdtscp exists (F_RDTSCP).
        unsafe { asm!("rdtscp", out("eax") _, out("edx") _, out("ecx") c, options(nomem, nostack, preserves_flags)) };
        aux = c as u64;
    }
    // Linux's VDSO_CPUNODE encoding (arch/x86/include/asm/segment.h): (node << 12) | cpu.
    if !cpu.is_null() {
        // SAFETY: the caller's pointer.
        unsafe { cpu.write((aux & 0xfff) as u32) };
    }
    if !node.is_null() {
        // SAFETY: the caller's pointer.
        unsafe { node.write((aux >> 12) as u32) };
    }
    0
}

/// Answers once the kernel published the resolutions (as Linux, whatever the clock mode).
///
/// # Safety
/// `res` null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __vdso_clock_getres(clk: i32, res: *mut Timespec) -> i32 {
    let ns = match base_of(clk) {
        Some(_) if matches!(clk, 5 | 6) => r32(COARSE_RES_NS),
        Some(_) => r32(RES_NS),
        None => 0,
    };
    if ns == 0 {
        return syscall2(SYS_CLOCK_GETRES, clk as i64 as u64, res as u64) as i32;
    }
    if !res.is_null() {
        // SAFETY: the caller's pointer.
        unsafe { res.write(Timespec { sec: 0, nsec: ns as i64 }) };
    }
    0
}

// The plain names Linux exports next to the __vdso_ ones (weak aliases, vdso/vclock_gettime.c), and the
// "Linux" note with the kernel version uname reports (6.6.0), as Linux's vDSO carries it (vdso/vdso-note.S).
global_asm!(
    ".weak clock_gettime",
    ".type clock_gettime, @function",
    ".set clock_gettime, __vdso_clock_gettime",
    ".weak gettimeofday",
    ".type gettimeofday, @function",
    ".set gettimeofday, __vdso_gettimeofday",
    ".weak time",
    ".type time, @function",
    ".set time, __vdso_time",
    ".weak getcpu",
    ".type getcpu, @function",
    ".set getcpu, __vdso_getcpu",
    ".weak clock_getres",
    ".type clock_getres, @function",
    ".set clock_getres, __vdso_clock_getres",
    ".pushsection .note.Linux, \"a\", @note",
    ".balign 4",
    ".long 6",
    ".long 4",
    ".long 0",
    ".asciz \"Linux\"",
    ".balign 4",
    ".long 0x060600",
    ".popsection",
);
