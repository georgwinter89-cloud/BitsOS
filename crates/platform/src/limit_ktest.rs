//! CP-4b ktests: CONFIG `smp=off` and `cpus=N`. Each skips unless its key is set, so the gate (no keys) skips
//! them; `host/tests/cp/55-smp-limits.sh` boots with the keys (`smp=off` and `cpus=4` under `-smp 16`).

use crate::plan;
use crate::smp::{config_limit, outcome};
use alloc::format;
use hal::percpu;
use hal::smp;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.smp.limit_off", Safe, limit_off);
ktest!("cp.smp.limit_cpus", Safe, limit_cpus);

const CALL_NS: u64 = 1_000_000_000;

fn plus_one(x: u64) -> u64 {
    x + 1
}

/// `smp=off` (or `cpus=1`): only the BSP runs, nothing was prepared, the other MADT CPUs are unknown to the
/// CPU map, and the Reload jump's single-CPU check passes (what RB-5a needs on the stick kernel).
fn limit_off() -> R {
    let (lim, _) = config_limit();
    if !lim.bsp_only() {
        return ktest::skip("CONFIG smp=off (or cpus=1) not set");
    }
    let o = outcome().ok_or("the bring-up initcall did not run")?;
    kassert!(o.not_started == Some(lim.reason()), "not_started {:?}, wanted {:?}", o.not_started, lim.reason());
    kassert!(o.limit == lim);
    kassert!(o.up.is_empty() && o.failed.is_empty() && o.held_back == 0 && o.planned == 0);
    kassert!(percpu::cpu_count() == 1, "cpu_count {}", percpu::cpu_count());
    let madt = acpi::tables().and_then(|a| a.madt.as_ref());
    let listed = madt.map_or(1, |m| m.enabled_cpus());
    // The possible count stays the MADT's (like Linux maxcpus=): per-CPU containers keep their size.
    kassert!(percpu::possible_cpus() >= listed, "possible {} < MADT {listed}", percpu::possible_cpus());
    // The BSP is still recorded as CPU 0 (APIC ID and topology), only nothing else starts.
    kassert!(smp::apic_id(0).is_some() && smp::apic_id(0) == hal::apic::id(), "BSP APIC ID {:?}", smp::apic_id(0));
    kassert!(smp::topology(0).is_some(), "the BSP has no topology record");
    kassert!(!smp::is_online(1));
    kassert!(smp::call_on(1, plus_one, 0, time::now_ns, CALL_NS) == Err(smp::CallError::NotOnline));
    let bsp = hal::apic::id().unwrap_or(0);
    for c in madt.iter().flat_map(|m| m.cpus.iter()).filter(|c| c.enabled && c.apic_id != bsp) {
        kassert!(smp::cpu_of_apic(c.apic_id).is_none(), "APIC {} is mapped although smp is off", c.apic_id);
    }
    kassert!(hal::kexec_stub::check_cpus().is_ok() && hal::kexec_stub::others_stopped(), "one CPU: the Reload jump has nothing to stop");
    Ok(())
}

/// `cpus=N` (N >= 2): exactly min(N, MADT CPUs) online, the APs after them held back in MADT order and never
/// touched, every online AP answers a call, and the Reload jump refuses while the APs run (RB-5b stops them first).
fn limit_cpus() -> R {
    let (lim, _) = config_limit();
    let Some(max) = lim.max_cpus.filter(|_| !lim.bsp_only()) else {
        return ktest::skip("CONFIG cpus=N (N >= 2) not set");
    };
    let o = outcome().ok_or("the bring-up initcall did not run")?;
    if let Some(why) = o.not_started {
        return ktest::skip(&format!("bring-up not started: {why}"));
    }
    kassert!(o.limit == lim);
    kassert!(o.failed.is_empty(), "{} AP(s) failed", o.failed.len());
    let expected = lim.expected_online(1 + o.planned);
    kassert!(percpu::cpu_count() == expected, "cpu_count {} with cpus={max} and {} APs planned", percpu::cpu_count(), o.planned);
    kassert!(o.up.len() == expected - 1, "{} APs up, wanted {}", o.up.len(), expected - 1);
    kassert!(o.held_back == o.planned - o.up.len(), "held_back {} with {} planned, {} up", o.held_back, o.planned, o.up.len());
    // The held-back APs are the plan's tail, in MADT order, and none of them has a CPU number.
    let madt = acpi::tables().and_then(|a| a.madt.as_ref()).ok_or("no MADT")?;
    let x2 = hal::apic::mode() == hal::apic::Mode::X2Apic;
    let p = plan::plan(madt, hal::apic::id().unwrap_or(0), x2);
    kassert!(p.aps.len() == o.planned);
    for (i, ap) in p.aps.iter().enumerate() {
        let cpu = smp::cpu_of_apic(ap.apic_id);
        if i < o.up.len() {
            kassert!(cpu == Some(i + 1), "APIC {} is cpu {cpu:?}, wanted {}", ap.apic_id, i + 1);
        } else {
            kassert!(cpu.is_none(), "held-back APIC {} has cpu {cpu:?}", ap.apic_id);
        }
    }
    for c in 1..percpu::cpu_count() {
        let v = smp::call_on(c, plus_one, c as u64, time::now_ns, CALL_NS).map_err(|e| format!("call_on cpu {c}: {e:?}"))?;
        kassert!(v == c as u64 + 1, "cpu {c}: {v}");
    }
    kassert!(smp::call_on(expected, plus_one, 0, time::now_ns, CALL_NS) == Err(smp::CallError::NotOnline));
    if expected > 1 {
        // Since RB-5b a Reload stops the other CPUs first; the jump itself refuses while they still run.
        kassert!(!hal::kexec_stub::others_stopped(), "the Reload jump must refuse while {expected} CPUs run");
    }
    Ok(())
}
