//! MWAIT idle states on hybrid Intel (GP-CPU2; pure, host-tested): Linux's `intel_idle` on a CPU without a
//! native table. `intel_idle_init` needs MONITOR/MWAIT with the extensions, the interrupt break and sub-states
//! (linux-7.3.1/drivers/idle/intel_idle.c:2856-2861); it takes `_CST` only when every state past C1 is FFH
//! (`intel_idle_cst_usable`, :1791-1806); each state's hint is checked against CPUID 5 (arch/x86/kernel/acpi/
//! cstate.c:135-146); exit latency is the table's (:1960), target residency three times it for C2 and deeper
//! (:1961-1972), the hint is entered with the interrupt break (:166-176). No bus-master check and no dummy read:
//! `intel_idle` does neither. With ARAT the LAPIC timer keeps running in every state, so no state needs the
//! broadcast timer (:1745-1757); with the invariant TSC nothing marks the clock unstable (:1981-1982). The HPET,
//! which stops in PC10 on this CPU, is not used here at all (GP-CPU B5).
//!
//! Where BitsOS differs: its `_CST` reader is a byte scan for `Name(_CST, Package…)`. This board's `_CST` sits in the
//! SSDT "Cpu0Cst" (Intel reference code, 916 bytes, read from this host's journal); in that code it is a Method,
//! which the scan does not read [U: the round-2 line `sched idle.mwait source=…` says which one was taken]. For the
//! 265K (family 6 model 0xC6) the states this firmware hands Linux are built in instead: `ARROW_LAKE_S`.

use super::plan::{sort_states, C1Only, CpuIdle, Deep, Firmware, Plan, Refused, MAX_LATENCY_US};
use acpi::cst::{CState, Entry};

/// Where the MWAIT states came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MwaitSource {
    /// The firmware's `_CST`, all states past C1 FFH.
    Cst,
    /// The built-in table of this CPU model.
    Model,
}

impl MwaitSource {
    pub fn name(self) -> &'static str {
        match self {
            MwaitSource::Cst => "cst",
            MwaitSource::Model => "model",
        }
    }
}

/// The 265K's states as its firmware lists them to Linux, the same on P and E cores (sysfs
/// `/sys/devices/system/cpu/cpu{0,12}/cpuidle/state{2,3}/{desc,latency}` on this machine, 5.11. in
/// plan/hw/gaming-pc-265k-2026-11-05/cstates-{p,e}.txt and again 15.11.: "ACPI FFH MWAIT 0x21" 127 us,
/// "ACPI FFH MWAIT 0x60" 1048 us). C1 (MWAIT 0x00, 1 us) is BitsOS's `sti; hlt`.
pub const ARROW_LAKE_S: [CState; 2] = [
    CState { kind: 2, entry: Entry::FixedHw(0x21), latency_us: 127 },
    CState { kind: 3, entry: Entry::FixedHw(0x60), latency_us: 1048 },
];

/// The built-in states of a model (`None`: no table).
pub fn model_table(cpu: &CpuIdle) -> Option<&'static [CState]> {
    (cpu.family == 6 && cpu.model == 0xC6).then_some(&ARROW_LAKE_S[..])
}

/// `intel_idle_cst_usable`: something past C1, and all of it FFH.
fn cst_usable(states: &[CState]) -> bool {
    let deep = || states.iter().filter(|c| c.kind > 1);
    deep().next().is_some() && deep().all(|c| matches!(c.entry, Entry::FixedHw(_)))
}

/// The plan for hybrid Intel (the caller checked hypervisor, ARAT and the invariant TSC).
pub fn plan(cpu: &CpuIdle, fw: Option<&Firmware<'_>>) -> Plan {
    let c1 = |why| Plan { c1_only: Some(why), ..Plan::default() };
    if !cpu.mwait.usable() {
        return c1(C1Only::NoMwait);
    }
    let cst = fw.map_or(&[][..], |f| f.states);
    let (states, source) = if cst_usable(cst) {
        (cst, MwaitSource::Cst)
    } else if let Some(t) = model_table(cpu) {
        (t, MwaitSource::Model)
    } else {
        return c1(C1Only::HybridIntel);
    };
    let mut p = Plan { mwait_source: Some(source), ..Plan::default() };
    for c in states {
        match judge(cpu, c, &p.deep) {
            Ok(Some(d)) => p.deep.push(d),
            Ok(None) => {}
            Err(why) => p.refused.push((*c, why)),
        }
    }
    sort_states(&mut p);
    if p.deep.is_empty() {
        p.c1_only = Some(C1Only::NoUsableState);
        p.mwait_source = None;
    }
    p
}

fn judge(cpu: &CpuIdle, c: &CState, taken: &[Deep]) -> Result<Option<Deep>, Refused> {
    let hint = match (c.kind, c.entry) {
        (1, _) => return Ok(None),
        (0 | 4.., _) => return Err(Refused::Kind),
        (_, Entry::FixedHw(a)) => (a & 0xFF) as u8,
        _ => return Err(Refused::NotMwait),
    };
    if c.latency_us == 0 || c.latency_us > MAX_LATENCY_US {
        return Err(Refused::Latency);
    }
    if !cpu.mwait.has_hint(hint) {
        return Err(Refused::MwaitHint);
    }
    if taken.iter().any(|d| d.mwait == Some(hint)) {
        return Err(Refused::Duplicate);
    }
    Ok(Some(Deep { kind: c.kind, port: 0, latency_ns: u64::from(c.latency_us) * 1000, bm_check: false, mwait: Some(hint) }))
}

/// Whether a state is a core C6 or deeper (MWAIT C-state field 2 or more): its residency must show in the core
/// C6/C7 counters, which turbostat and perf read on Arrow Lake (tools/power/x86/turbostat/turbostat.c:928-934 and
/// :1229, CC6 | CC7; arch/x86/events/intel/cstate.c:510-513 and :668).
pub fn reaches_core_c6(hint: u8) -> bool {
    hint >> 4 >= 2
}
