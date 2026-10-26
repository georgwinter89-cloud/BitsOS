//! This CPU's LAPIC timer (owner: CP, item CP-5; spec/core.md §7.2, §11.8; SDM Vol. 3A 11.5.4).
//!
//! Two modes are used, both one-shot: the classic **one-shot** mode (an initial count runs down at the bus or
//! crystal clock divided by `divide`; the Mini-PC: ~24 MHz, no TSC-deadline, ARAT) and **TSC-deadline** mode
//! (the interrupt comes when the TSC reaches IA32_TSC_DEADLINE; CPUID 1 ECX bit 24). `time` owns the policy:
//! which mode, the calibration, and what to program when; this file only touches the registers, on the CPU it
//! runs on, with interrupts off (`IrqGuard` inside).
//!
//! The vector is always `LAPIC_TIMER` (0xF0, a system vector: one handler for every CPU). A masked timer still
//! counts (calibration). Writing initial count 0 stops a one-shot timer; writing deadline 0 disarms a deadline.

use super::{LAPIC_TIMER, LVT_MASKED, Mode, REG_LVT_TIMER, REG_TIMER_CURRENT, REG_TIMER_DIV, REG_TIMER_INIT, mode, raw_read, raw_write};
use crate::msr;
use core::sync::atomic::{AtomicBool, Ordering};

/// Set by the first `setup(TscDeadline, ..)` whose CPUID check passed: IA32_TSC_DEADLINE exists, so writing it
/// cannot #GP. One flag for all CPUs is enough: the MSR is a CPU feature, and in another timer mode the CPU
/// ignores writes to it (SDM 11.5.4.1), so only its existence matters. Kept in an atomic so `arm_deadline` does
/// not run CPUID per call (a VM exit under a hypervisor).
static DEADLINE_OK: AtomicBool = AtomicBool::new(false);

/// LVT timer mode field (bits 17-18).
pub const LVT_ONESHOT: u32 = 0;
pub const LVT_PERIODIC: u32 = 1 << 17;
pub const LVT_TSC_DEADLINE: u32 = 2 << 17;
pub const LVT_MODE_MASK: u32 = 3 << 17;

/// How this CPU's timer counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerMode {
    /// Initial-count mode; `divide` is 1, 2, 4, ..., 128.
    OneShot { divide: u32 },
    /// IA32_TSC_DEADLINE (only where CPUID says `tsc_deadline_timer`).
    TscDeadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerError {
    /// The LAPIC is not set up (`init_bsp`/`init_ap` first).
    NotEnabled,
    /// A divide value the register cannot hold.
    BadDivide(u32),
    /// TSC-deadline mode on a CPU without it.
    NoTscDeadline,
}

/// The divide configuration register's value for `divide` (pure; host-tested): bits 0, 1 and 3 encode
/// 2^(n+1), with 0b111 meaning 1 (SDM Figure 11-10).
pub const fn divide_bits(divide: u32) -> Option<u32> {
    let n = match divide {
        2 => 0,
        4 => 1,
        8 => 2,
        16 => 3,
        32 => 4,
        64 => 5,
        128 => 6,
        1 => 7,
        _ => return None,
    };
    Some((n & 3) | ((n & 4) << 1))
}

/// The LVT timer value for `mode` on vector 0xF0 (pure; host-tested).
pub const fn lvt_value(mode: TimerMode, masked: bool) -> u32 {
    let m = match mode {
        TimerMode::OneShot { .. } => LVT_ONESHOT,
        TimerMode::TscDeadline => LVT_TSC_DEADLINE,
    };
    m | LAPIC_TIMER as u32 | if masked { LVT_MASKED } else { 0 }
}

/// Puts this CPU's timer into `mode`, stopped, masked or not. Interrupts off inside.
pub fn setup(tm: TimerMode, masked: bool) -> Result<(), TimerError> {
    let m = mode();
    if m == Mode::Off {
        return Err(TimerError::NotEnabled);
    }
    if tm == TimerMode::TscDeadline {
        if !crate::cpu::live().has("tsc_deadline_timer") {
            return Err(TimerError::NoTscDeadline);
        }
        DEADLINE_OK.store(true, Ordering::Release);
    }
    let div = match tm {
        TimerMode::OneShot { divide } => Some(divide_bits(divide).ok_or(TimerError::BadDivide(divide))?),
        TimerMode::TscDeadline => None,
    };
    let _irq = crate::cpu::IrqGuard::new();
    // Stop whatever runs before the mode changes (a mode change also disarms a deadline, SDM 11.5.4.1).
    raw_write(m, REG_TIMER_INIT, 0);
    if let Some(d) = div {
        raw_write(m, REG_TIMER_DIV, d);
    }
    raw_write(m, REG_LVT_TIMER, lvt_value(tm, masked));
    if tm == TimerMode::TscDeadline {
        // The LVT write must be visible before the first IA32_TSC_DEADLINE write (SDM 11.5.4.1; Linux
        // `__setup_APIC_LVTT`); an MSR write is not ordered after an xAPIC MMIO store.
        imp::fence();
        msr::write(msr::IA32_TSC_DEADLINE, 0);
    }
    Ok(())
}

/// One-shot mode: starts counting down from `count` (0 stops the timer) on the CPU it runs on. Interrupts off
/// inside; the caller decides which CPU's timer this is, so it calls with interrupts off already (the hrtimer
/// queue lock is an `IrqSpinLock`): with them on, preemption at an interrupt exit could move it to another CPU
/// between its decision and this write.
pub fn arm_count(count: u32) {
    let _irq = crate::cpu::IrqGuard::new();
    raw_write(mode(), REG_TIMER_INIT, count);
}

/// TSC-deadline mode: the interrupt comes once the TSC reaches `tsc` (0 disarms; a deadline already past fires
/// at once), on the CPU it runs on (call with interrupts off, as `arm_count`). Refused with `NoTscDeadline`
/// until a `setup(TscDeadline, ..)` confirmed the MSR exists (no #GP from a wrong caller); on a CPU whose timer
/// is in another mode the write is ignored by the CPU.
pub fn arm_deadline(tsc: u64) -> Result<(), TimerError> {
    if !DEADLINE_OK.load(Ordering::Acquire) {
        return Err(TimerError::NoTscDeadline);
    }
    let _irq = crate::cpu::IrqGuard::new();
    imp::fence();
    msr::write(msr::IA32_TSC_DEADLINE, tsc);
    Ok(())
}

/// Whether `arm_deadline` may write the MSR (a `setup(TscDeadline, ..)` passed its CPUID check).
pub fn deadline_ready() -> bool {
    DEADLINE_OK.load(Ordering::Acquire)
}

/// The one-shot timer's current count (0 once it ran out or when stopped).
pub fn current_count() -> u32 {
    raw_read(mode(), REG_TIMER_CURRENT)
}

/// The LVT timer register (`None` before the LAPIC is set up).
pub fn lvt() -> Option<u32> {
    let m = mode();
    (m != Mode::Off).then(|| raw_read(m, REG_LVT_TIMER))
}

/// Stops and masks this CPU's timer in either mode (Reload, reboot, tests). Interrupts off inside; IRQ-safe.
pub fn stop() {
    let m = mode();
    if m == Mode::Off {
        return;
    }
    let _irq = crate::cpu::IrqGuard::new();
    let v = raw_read(m, REG_LVT_TIMER);
    raw_write(m, REG_TIMER_INIT, 0);
    if v & LVT_MODE_MASK == LVT_TSC_DEADLINE {
        imp::fence();
        msr::write(msr::IA32_TSC_DEADLINE, 0);
    }
    raw_write(m, REG_LVT_TIMER, (v & !LVT_MODE_MASK) | LVT_MASKED);
}

#[cfg(target_os = "none")]
mod imp {
    pub fn fence() {
        // SAFETY: two fences, no memory or register effects besides ordering (Linux `weak_wrmsr_fence` before
        // IA32_TSC_DEADLINE: the MSR write is not serializing and must come after earlier stores).
        unsafe { core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags)) };
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn fence() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divide_encoding() {
        // SDM Figure 11-10: 000 = 2, 001 = 4, 010 = 8, 011 = 16, 100 = 32, 101 = 64, 110 = 128, 111 = 1 (bit 2 is reserved).
        assert_eq!(divide_bits(2), Some(0b0000));
        assert_eq!(divide_bits(16), Some(0b0011));
        assert_eq!(divide_bits(32), Some(0b1000));
        assert_eq!(divide_bits(128), Some(0b1010));
        assert_eq!(divide_bits(1), Some(0b1011));
        assert_eq!(divide_bits(3), None);
        assert_eq!(divide_bits(256), None);
        assert_eq!(divide_bits(0), None);
    }

    #[test]
    fn lvt_values() {
        assert_eq!(lvt_value(TimerMode::OneShot { divide: 16 }, false), 0xF0);
        assert_eq!(lvt_value(TimerMode::OneShot { divide: 16 }, true), 0x1_00F0);
        assert_eq!(lvt_value(TimerMode::TscDeadline, false), 0x4_00F0);
    }

    #[test]
    fn host_has_no_lapic() {
        assert_eq!(arm_deadline(1), Err(TimerError::NoTscDeadline), "no setup confirmed the MSR: refused, no wrmsr");
        assert!(!deadline_ready());
        assert_eq!(setup(TimerMode::TscDeadline, false), Err(TimerError::NotEnabled));
        assert!(!deadline_ready(), "a refused setup does not open arm_deadline");
        assert_eq!(setup(TimerMode::OneShot { divide: 16 }, false), Err(TimerError::NotEnabled));
        assert_eq!(lvt(), None);
        stop();
    }
}
