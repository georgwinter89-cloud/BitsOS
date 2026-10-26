//! CP-5 power ktests: the reboot and power-off plans for this machine's tables (`Safe`: nothing is written),
//! and the QEMU-only action the scenario `host/tests/cp/60-power.sh` asks for with CONFIG `cp_power_test=`.

use acpi::power::{self, Step};
use alloc::format;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.power.reboot_plan", Safe, reboot_plan);
ktest!("cp.power.poweroff_plan", Safe, poweroff_plan);
ktest!("cp.power.qemu_action", QemuOnly, qemu_action);

fn hypervisor() -> bool {
    hal::cpu::live().has("hypervisor")
}

/// The live reboot plan ends with the triple fault, starts with what the FADT declares, and the firmware
/// declares at least one path (q35: 0xCF9 <- 0xF and an 8042; Mini-PC: 0xB2 <- 0xBE).
fn reboot_plan() -> R {
    let p = power::live_reboot_plan();
    let steps: alloc::vec::Vec<_> = p.steps.iter().map(Step::describe).collect();
    klog::event!(Info, "cp", "ktest.reboot_plan", steps = steps.join(" | "), skipped = p.skipped.join(" | "));
    kassert!(p.steps.last() == Some(&Step::TripleFault), "the triple fault is not last: {steps:?}");
    kassert!(p.steps.contains(&Step::Cf9 { code: hal::cpu::reset::CF9_CODE_HARD }), "0xCF9 hard reset missing: {steps:?}");
    kassert!(p.steps.contains(&Step::Cf9 { code: hal::cpu::reset::CF9_CODE_FULL }), "0xCF9 full reset missing: {steps:?}");
    kassert!(p.declared(), "the firmware declares no reset path: {steps:?}");
    if hypervisor() {
        kassert!(matches!(p.steps.first(), Some(Step::FadtCf9 { .. })), "q35's FADT names 0xCF9: {steps:?}");
    }
    Ok(())
}

/// The power-off plan can be built from this machine's tables (`\_S5`, PM1 control, GPE blocks).
fn poweroff_plan() -> R {
    let a = acpi::tables().ok_or("ACPI tables not read")?;
    let p = power::poweroff_plan(a, acpi::live::sci_en_at_boot()).map_err(|e| format!("no power-off plan: {e:?}"))?;
    klog::event!(Info, "cp", "ktest.poweroff_plan", slp_typ_a = p.s5.typ_a, slp_typ_b = p.s5.typ_b, gpe_blocks = p.gpes.len(),
        acpi_enable = p.acpi_enable.is_some(), pm1b = p.pm1b_cnt.is_some(), sci_en_at_boot = format!("{:?}", acpi::live::sci_en_at_boot()));
    kassert!(p.pm1a_cnt.is_some() || p.sleep_control.is_some(), "nothing to write SLP_TYP to");
    kassert!(!p.gpes.is_empty() || p.gpes_skipped > 0 || a.fadt.as_ref().is_some_and(|f| f.gpe0_len == 0), "GPE blocks lost");
    Ok(())
}

/// QEMU only, and only with CONFIG `cp_power_test=poweroff|reboot` (the scenario): quiesce, then do it. Comes
/// back only when it failed.
fn qemu_action() -> R {
    let Some(what) = acpi::cpu_keys().and_then(|k| k.power_test.clone()) else {
        return ktest::skip("CONFIG cp_power_test= not set (host/tests/cp/60-power.sh sets it)");
    };
    if !hypervisor() {
        return ktest::skip("QEMU only");
    }
    match what.trim() {
        "poweroff" => Err(format!("power-off came back: {:?}", crate::power::poweroff())),
        "reboot" => crate::power::reboot(),
        other => Err(format!("cp_power_test={other}: poweroff or reboot")),
    }
}
