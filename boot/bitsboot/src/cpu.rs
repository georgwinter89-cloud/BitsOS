//! CPU registers, CPUID, the boot ID and the rough TSC rate. Decisions on the values: `bitsboot_pure::cpu`.

use alloc::vec::Vec;
use bitsboot_pure::cpu::CpuidWords;
use core::arch::asm;
use core::time::Duration;
use uefi::boot;

pub use bitsboot_pure::cpu::CR4_CET;
pub const EFER: u32 = 0xC000_0080;
pub const EFER_NXE: u64 = 1 << 11;

pub fn cr0() -> u64 {
    let v;
    // SAFETY: reading CR0 has no side effect; the loader runs in ring 0.
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack)) };
    v
}
pub fn cr3() -> u64 {
    let v;
    // SAFETY: as cr0.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)) };
    v
}
pub fn cr4() -> u64 {
    let v;
    // SAFETY: as cr0.
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack)) };
    v
}
/// # Safety
/// `v` must be a valid CR4 for the current paging mode (callers only clear CET).
pub unsafe fn write_cr4(v: u64) {
    // SAFETY: the caller's contract.
    unsafe { asm!("mov cr4, {}", in(reg) v, options(nostack)) };
}
/// Only for MSRs every x86-64 CPU has (EFER).
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: callers only read architectural MSRs that exist in long mode, so no #GP.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    (hi as u64) << 32 | lo as u64
}
/// # Safety
/// The MSR must exist and the value must not break the running firmware (callers only set EFER.NXE).
pub unsafe fn wrmsr(msr: u32, v: u64) {
    // SAFETY: the caller's contract.
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nostack)) };
}
pub fn rdtsc() -> u64 {
    // SAFETY: RDTSC exists on every x86-64 CPU and is allowed in ring 0.
    unsafe { core::arch::x86_64::_rdtsc() }
}
#[allow(unused_unsafe)]
pub fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID exists on every x86-64 CPU; an unsupported leaf returns data, never faults.
    let r = unsafe { core::arch::x86_64::__cpuid_count(leaf, 0) };
    (r.eax, r.ebx, r.ecx, r.edx)
}

fn max_ext() -> u32 {
    cpuid(0x8000_0000).0
}

pub fn has_nx() -> bool {
    max_ext() >= 0x8000_0001 && cpuid(0x8000_0001).3 & (1 << 20) != 0
}

/// Running under a hypervisor (QEMU in the tests). Only test hooks and the error screen's wait depend on it.
pub fn hypervisor() -> bool {
    cpuid(1).2 & (1 << 31) != 0
}

pub fn missing_v3_features() -> Vec<&'static str> {
    let (_, _, ecx1, edx1) = cpuid(1);
    let ebx7 = if cpuid(0).0 >= 7 { cpuid(7).1 } else { 0 };
    let (ecx81, edx81) = if max_ext() >= 0x8000_0001 {
        let r = cpuid(0x8000_0001);
        (r.2, r.3)
    } else {
        (0, 0)
    };
    bitsboot_pure::cpu::missing_v3(CpuidWords { ecx1, edx1, ebx7, ecx81, edx81 })
}

fn rdrand() -> Option<u64> {
    for _ in 0..16 {
        let (v, ok): (u64, u8);
        // SAFETY: only called when CPUID says RDRAND exists.
        unsafe { asm!("rdrand {v}", "setc {ok}", v = out(reg) v, ok = out(reg_byte) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(v);
        }
    }
    None
}

fn splitmix(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 16 bytes that differ on every boot: RDRAND if present, else TSC jitter around short stalls.
pub fn boot_id() -> [u8; 16] {
    let have_rdrand = cpuid(1).2 & (1 << 30) != 0;
    let mut words = [0u64; 2];
    let mut seed = rdtsc();
    for w in &mut words {
        *w = match have_rdrand.then(rdrand).flatten() {
            Some(v) => v,
            None => {
                boot::stall(Duration::from_micros(7));
                seed ^= rdtsc().rotate_left(17);
                splitmix(&mut seed)
            }
        };
    }
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&words[0].to_le_bytes());
    id[8..].copy_from_slice(&words[1].to_le_bytes());
    id
}

/// TSC ticks per second against the firmware's Stall: three 17 ms windows, the shortest wins (V1's method;
/// only a plausibility bound, the kernel calibrates against the HPET).
pub fn tsc_hint_hz() -> u64 {
    let mut best = u64::MAX;
    for _ in 0..3 {
        let t0 = rdtsc();
        boot::stall(Duration::from_micros(17_000));
        best = best.min(rdtsc().wrapping_sub(t0));
    }
    best.saturating_mul(1000) / 17
}
