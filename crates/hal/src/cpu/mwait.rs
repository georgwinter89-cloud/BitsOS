//! MONITOR/MWAIT idle entry (GP-CPU2), Linux's `mwait_idle_with_hints` with the interrupt-break extension
//! (linux-7.3.1/arch/x86/include/asm/mwait.h:109-135, drivers/idle/intel_idle.c:166-176): entered with interrupts
//! off and ECX bit 0 set, so a pending interrupt ends the wait although IF is clear; the caller's `sti` then takes it.
//! That is the same race-free shape as `sti; hlt`: an interrupt that arrives after the idle loop's last check stays
//! pending and MWAIT returns at once.
//!
//! What Linux does around it and why it is left out here: `x86_idle_clear_cpu_buffers` (VERW) only for CPUs with
//! the MDS bug, and the 265K reports MDS_NO; the CLFLUSH before MONITOR only for X86_BUG_CLFLUSH_MONITOR (Apollo
//! Lake era); MONITOR on the thread flags so a remote `need_resched` can skip the IPI: BitsOS always sends the
//! reschedule IPI to a halted CPU, so the monitored line is only a second way to wake a CPU (`wake`).

use core::sync::atomic::{AtomicU64, Ordering};

/// CPUID.5:ECX bit 0: MWAIT extensions are enumerated; bit 1: interrupts break MWAIT even when masked.
pub const ECX_EXTENSIONS: u32 = 1 << 0;
pub const ECX_INTERRUPT_BREAK: u32 = 1 << 1;
/// MWAIT ECX bit 0 (`MWAIT_ECX_INTERRUPT_BREAK`).
pub const BREAK_ON_INTERRUPT: u32 = 1;

/// What CPUID says about MWAIT on this CPU (leaf 1 ECX bit 3 and leaf 5).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MwaitCaps {
    pub monitor: bool,
    /// CPUID.5:ECX.
    pub ecx: u32,
    /// CPUID.5:EDX: number of sub-states per MWAIT C-state, four bits each (C0 in bits 3:0).
    pub substates: u32,
}

impl MwaitCaps {
    /// The 265K's leaf 5 (`40 40 3 10102020`, read from the machine; the same on P and E cores).
    pub const ARROW_LAKE: MwaitCaps = MwaitCaps { monitor: true, ecx: 3, substates: 0x1010_2020 };

    /// This CPU.
    pub fn live() -> MwaitCaps {
        let c = super::cpuid;
        let monitor = c(1, 0).ecx & (1 << 3) != 0;
        if !monitor || c(0, 0).eax < 5 {
            return MwaitCaps { monitor, ..MwaitCaps::default() };
        }
        let l5 = c(5, 0);
        MwaitCaps { monitor, ecx: l5.ecx, substates: l5.edx }
    }

    /// Linux's precondition for any MWAIT idle state (intel_idle.c:2856-2861, arch/x86/kernel/acpi/cstate.c:148-153):
    /// MONITOR, the extensions, the interrupt break and at least one sub-state.
    pub fn usable(&self) -> bool {
        self.monitor && self.ecx & ECX_EXTENSIONS != 0 && self.ecx & ECX_INTERRUPT_BREAK != 0 && self.substates != 0
    }

    /// Whether the CPU lists sub-states for the C-state `hint` asks for (intel_idle.c:2314-2327 `intel_idle_verify_cstate`,
    /// cstate.c:135-146): hint bits 7:4 are the C-state minus one.
    pub fn has_hint(&self, hint: u8) -> bool {
        let cstate = (u32::from(hint >> 4) + 1) & 0xF;
        (self.substates >> (cstate * 4)) & 0xF != 0
    }
}

/// One monitored line per CPU slot (a line shared by two CPUs only wakes the other one for nothing).
#[repr(align(64))]
struct Line(AtomicU64);

const LINES: usize = 64;
static MONITORED: [Line; LINES] = [const { Line(AtomicU64::new(0)) }; LINES];

fn line(cpu: usize) -> &'static AtomicU64 {
    &MONITORED[cpu % LINES].0
}

/// Wakes `cpu` out of MWAIT by a write to the line it monitors (no interrupt needed). The idle self-check's guard
/// uses it together with the reschedule IPI.
pub fn wake(cpu: usize) {
    line(cpu).fetch_add(1, Ordering::SeqCst);
}

/// Enters the MWAIT state `hint` on this CPU until an interrupt (masked or not) or a write to its line. Call with
/// interrupts off; returns with them still off. Only after `MwaitCaps::usable` said yes (MONITOR/MWAIT #UD otherwise).
#[cfg(target_os = "none")]
pub fn idle(hint: u8) {
    let l = line(crate::percpu::this_cpu());
    let seen = l.load(Ordering::SeqCst);
    // SAFETY: MONITOR arms on a static, always mapped line (ECX = EDX = 0: no extensions, no hints); MWAIT with
    // ECX bit 0 waits until an interrupt or a store to that line, and the caller made sure CPUID enumerates both
    // (`MwaitCaps::usable`) and that interrupts are off.
    unsafe {
        core::arch::asm!("monitor", in("rax") l.as_ptr(), in("ecx") 0u32, in("edx") 0u32, options(nostack, preserves_flags));
    }
    // A wake written between the load and MONITOR would be missed by MWAIT: look once more.
    if l.load(Ordering::SeqCst) != seen {
        return;
    }
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("mwait", in("eax") u32::from(hint), in("ecx") BREAK_ON_INTERRUPT, options(nostack, preserves_flags));
    }
}

/// On the host nothing sleeps.
#[cfg(not(target_os = "none"))]
pub fn idle(_hint: u8) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_lake_has_the_hints_linux_uses_there() {
        let c = MwaitCaps::ARROW_LAKE;
        assert!(c.usable());
        // The hints Linux enters on this machine (C1 0x00, 0x21, 0x60) and their neighbours.
        for h in [0x00u8, 0x01, 0x20, 0x21, 0x60] {
            assert!(c.has_hint(h), "{h:#x}");
        }
        assert!(!c.has_hint(0x10), "the 265K lists no sub-state for MWAIT C2");
        assert!(!c.has_hint(0x30), "nor for MWAIT C4");
    }

    #[test]
    fn without_extensions_or_break_nothing_is_usable() {
        for caps in [
            MwaitCaps { ecx: 1, ..MwaitCaps::ARROW_LAKE },
            MwaitCaps { ecx: 2, ..MwaitCaps::ARROW_LAKE },
            MwaitCaps { substates: 0, ..MwaitCaps::ARROW_LAKE },
            MwaitCaps { monitor: false, ..MwaitCaps::ARROW_LAKE },
        ] {
            assert!(!caps.usable(), "{caps:?}");
        }
    }
}
