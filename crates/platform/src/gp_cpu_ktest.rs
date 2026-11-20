//! GP-CPU2 ktests of the Gaming-PC (class GamingPc, run over MCP in its round; never in QEMU). Each says its result
//! in one plain German line (`cpu ktest.gp_cpu_*`).

use crate::smp::outcome;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::cpu::hwp;
use hal::{msr, percpu, smp};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("gp.cpu.epp", GamingPc, epp);
ktest!("gp.cpu.tsc_offsets", GamingPc, tsc_offsets);

/// This CPU's EPP field of IA32_HWP_REQUEST (bit 32 set: unreadable).
fn epp_here(_: u64) -> u64 {
    msr::try_read(msr::IA32_HWP_REQUEST).map_or(1 << 32, |r| r >> 24 & 0xFF)
}

/// Every CPU asks for Linux 7.3.1's EPP of this model under the `cpu_policy=` in force (Arrow Lake: 0x40 balanced,
/// 0x10 performance; `hwp::epp_values`). Read-only.
fn epp() -> R {
    let v = hal::cpu::live();
    kassert!(hwp::gate(&v).is_none(), "HWP is not run on this machine: {:?}", hwp::gate(&v));
    let policy = smp::hwp_policy();
    let want = u64::from(hwp::epp_for(&v, policy));
    let mut wrong = Vec::new();
    for c in 0..percpu::cpu_count() {
        let got = if c == percpu::this_cpu() { epp_here(0) } else { smp::call_on(c, epp_here, 0, time::now_ns, 1_000_000_000).map_err(|e| format!("cpu {c}: {e:?}"))? };
        if got != want {
            wrong.push(format!("cpu {c}: {got:#x}"));
        }
    }
    let word = if policy == hwp::Policy::Performance { "Höchstleistung" } else { "ausgewogen" };
    klog::event!(Info, "cpu", "ktest.gp_cpu_epp", policy = policy.name(), epp = klog::Hex(want), cpus = percpu::cpu_count(), mismatches = wrong.len();
        "Energievorgabe {:#x} ({}) auf {} Kernen{}", want, word, percpu::cpu_count(), if wrong.is_empty() { String::new() } else { format!(", abweichend: {}", wrong.join(", ")) });
    kassert!(wrong.is_empty(), "EPP {want:#x} wanted: {}", wrong.join(", "));
    Ok(())
}

/// The substance of `cp.smp.tsc_offsets_hardware` (the Mini-PC's, class Hardware) with this machine's own bound: an
/// AP's TSC may differ from the BSP's only by what the handshake cannot resolve, half the shortest round trip, plus the
/// Mini-PC's 38 ticks of jitter (`tsc::CHECK_TICKS`); and IA32_TSC_ADJUST is the same on every CPU (Linux's
/// `tsc_sanitize_first_cpu`/`tsc_store_and_check_tsc_adjust`, arch/x86/kernel/tsc_sync.c). Round 1 showed
/// `tsc_worst_ticks=343` without the round trips; this says which it is.
fn tsc_offsets() -> R {
    let o = outcome().ok_or("no outcome")?;
    kassert!(!o.up.is_empty(), "no AP came up");
    let mut worst = (0usize, 0u64, 0u64);
    let mut bad = Vec::new();
    for r in &o.up {
        let t = r.tsc.ok_or_else(|| format!("cpu {}: no TSC round came back", r.cpu))?;
        let bound = t.min_rtt / 2 + crate::tsc::CHECK_TICKS;
        if t.magnitude() > worst.1 {
            worst = (r.cpu as usize, t.magnitude(), t.min_rtt);
        }
        if t.magnitude() > bound {
            bad.push(format!("cpu {}: {} ticks (rtt {}, bound {bound})", r.cpu, t.median, t.min_rtt));
        }
        if r.tsc_adjust != o.bsp_tsc_adjust {
            bad.push(format!("cpu {}: TSC_ADJUST {:?}, BSP {:?}", r.cpu, r.tsc_adjust, o.bsp_tsc_adjust));
        }
    }
    klog::event!(Info, "cpu", "ktest.gp_cpu_tsc_offsets", aps = o.up.len(), worst_cpu = worst.0, worst_ticks = worst.1, worst_rtt = worst.2, outside = bad.len();
        "Zeitzähler aller Kerne im Gleichschritt: größte Abweichung {} Takte auf Kern {} (Messgenauigkeit {} Takte){}", worst.1, worst.0, worst.2 / 2,
        if bad.is_empty() { String::new() } else { format!("; außerhalb: {}", bad.join(", ")) });
    kassert!(bad.is_empty(), "{}", bad.join("; "));
    Ok(())
}
