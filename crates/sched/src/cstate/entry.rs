//! Entering a planned state (Linux `acpi_idle_enter` / `acpi_idle_enter_bm` / `io_idle`,
//! linux-6.6/drivers/acpi/processor_idle.c l.501-525, l.527-575, l.616-705). Generic over the machine so the
//! host tests and the ktests run the very same steps against a recording fake; the idle loop uses the real
//! ports.

use super::plan::Plan;

/// PM1 status bit 4: a bus master (DMA) was active since it was last cleared. Write 1 to clear.
pub const BM_STS: u16 = 1 << 4;

/// What entering needs from the machine.
pub trait IdleHw {
    /// The C-state port read: the read itself puts the core to sleep until an interrupt is pending. `false`:
    /// the read did not happen (the port guard refused it), so the core did not sleep.
    fn read8(&self, port: u16) -> bool;
    /// PM1 status. A read that cannot happen must answer `BM_STS` (fail closed: C3 is then not entered).
    fn read16(&self, port: u16) -> u16;
    fn write16(&self, port: u16, v: u16);
    /// The dummy read after the state (its value is not used).
    fn read32(&self, port: u16);
    /// `sti; hlt` (C1).
    fn halt(&self);
    fn sti(&self);
}

/// What was entered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entered {
    C1,
    Deep(usize),
    /// The chosen C3 found bus-master activity; Linux's "safe state" (the deepest non-C3 state, else C1) was
    /// entered instead.
    Demoted { from: usize, to: Option<usize> },
    /// The state's port read was refused by the port guard (plan and guard disagree): C1 instead, so the idle
    /// loop does not spin.
    Refused(usize),
}

/// Enters `pick` (an index into `plan.deep`; `None` or out of range: C1). Called with interrupts off;
/// returns with them on, a pending interrupt is taken at the `sti`.
pub fn enter<H: IdleHw>(hw: &H, plan: &Plan, pick: Option<usize>) -> Entered {
    let Some(want) = pick.filter(|&i| i < plan.deep.len()) else {
        hw.halt();
        return Entered::C1;
    };
    let mut at = want;
    if plan.deep[want].bm_check && bus_master_active(hw, &plan.bm_status) {
        match plan.deep[..want].iter().rposition(|d| !d.bm_check) {
            Some(j) => at = j,
            None => {
                hw.halt();
                return Entered::Demoted { from: want, to: None };
            }
        }
    }
    if !hw.read8(plan.deep[at].port) {
        hw.halt();
        return Entered::Refused(at);
    }
    if let Some(p) = plan.dummy_read {
        hw.read32(p);
    }
    hw.sti();
    if at == want { Entered::Deep(at) } else { Entered::Demoted { from: want, to: Some(at) } }
}

/// BM_STS of PM1a or PM1b (Linux reads both as one register); when set, cleared in both.
fn bus_master_active<H: IdleHw>(hw: &H, ports: &[Option<u16>; 2]) -> bool {
    let active = ports.iter().flatten().fold(false, |a, &p| a | (hw.read16(p) & BM_STS != 0));
    if active {
        for &p in ports.iter().flatten() {
            hw.write16(p, BM_STS);
        }
    }
    active
}
