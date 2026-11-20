//! In-kernel tests of GP-GUARD (the Gaming-PC's disk rule, `gp_platform::disk_rule`). Only the boots of
//! `host/tests/bt/65-gp-tiers.sh` run them: QEMU with the board's DMI names (so bitsboot forces the profile's
//! lines), an NVMe controller and a second USB disk beside the boot stick. No test here sends a write to a device.
//! - `block.guard.internal_off`: `nvme_bind=off` reached the block layer, no NVMe disk is registered, every write
//!   switch is off, and the full-access NVMe tick is refused (the gate stays closed).
//! - `block.guard.usb_other`: every USB disk that is not the boot stick refuses writes; the stick's identity is
//!   there exactly when CONFIG says `gp_stick=on`, and without it the stick refuses too.

use crate::error::Refusal;
use devmodel::block::BlockDevice;
use crate::policy::{self, DiskKind, StickMatch};
use crate::queue::Kind;
use crate::table::Verdict;
use alloc::string::String;
use ktest::{TestResult, kassert, ktest};

const NOT_THIS_BOOT: &str = "this boot's CONFIG has no nvme_bind=off (the Gaming-PC profile did not apply)";

fn internal_off() -> TestResult {
    // First, and before the tick below: on a boot without the guard `open_nvme_until` would really open the gate.
    if !policy::internal_disks_off() {
        return ktest::fail(NOT_THIS_BOOT);
    }
    let _ = super::ktests::usb_disks();
    for d in crate::disks() {
        kassert!(d.kind() != DiskKind::Nvme, "{}: an NVMe disk is registered under nvme_bind=off", d.name());
    }
    let sw = policy::switches();
    kassert!(!sw.nvme_write && !sw.usb_msd_write && !sw.stick_write, "a write switch is on: {sw:?}");
    let end = ksync::now_ns().unwrap_or(0).saturating_add(60_000_000_000);
    policy::open_nvme_until(end, "ktest block.guard.internal_off");
    kassert!(policy::nvme_tick_refused(), "the tick was not refused");
    kassert!(!policy::nvme_runtime_open(), "the tick opened the NVMe gate");
    let sw = policy::switches();
    kassert!(!sw.nvme_write, "nvme_write answers on after the tick");
    for stick in [StickMatch::Yes, StickMatch::No, StickMatch::Unknown] {
        let r = policy::decide(DiskKind::Nvme, false, &Verdict::Clean, stick, sw);
        kassert!(r.is_err(), "an NVMe disk with a clean table would take writes ({stick:?})");
    }
    Ok(())
}
ktest!("block.guard.internal_off", Run("bt/65-gp-tiers"), internal_off);

fn usb_other() -> TestResult {
    if !policy::internal_disks_off() {
        return ktest::fail(NOT_THIS_BOOT);
    }
    let disks = super::ktests::usb_disks();
    let write = Kind::Write { fua: false, preflush: false };
    let mut others = 0;
    for d in disks.iter().filter(|d| d.stick() == StickMatch::No) {
        let p = d.write_policy();
        kassert!(matches!(p, Err(Refusal::UsbWritesOff | Refusal::DeviceReadOnly)), "{}: {p:?}", d.name());
        kassert!(d.check_write(write, Some((0, 1))).is_err(), "{}: a write of block 0 would pass", d.name());
        kassert!(crate::DiskDevice(d.clone()).info().read_only, "{} does not say read-only", d.name());
        others += 1;
    }
    kassert!(others > 0, "no USB disk beside the boot stick (the scenario adds one)");
    kassert!(!disks.iter().any(|d| d.stick() == StickMatch::Unknown), "a USB disk cannot be told apart from the stick");
    let sticks: alloc::vec::Vec<_> = disks.iter().filter(|d| d.stick() == StickMatch::Yes).collect();
    kassert!(sticks.len() == 1, "{} disks match the boot stick (want 1)", sticks.len());
    let config = policy::FACTS.lock().config;
    let tier = config.and_then(|f| f("gp_stick")).is_some_and(|v| v.trim() == "on");
    let handed = policy::stick_identity().is_some_and(|s| s.id.is_some());
    kassert!(handed == tier, "STICK.ID handed over = {handed}, CONFIG gp_stick=on = {tier}");
    if !handed {
        let p = sticks[0].write_policy();
        kassert!(p.is_err(), "the boot stick takes writes without gp_stick=on");
        kassert!(!policy::stick_takes_fat_writes(policy::stick_identity()));
    }
    klog::event!(Info, "block", "ktest.guard", others = others as u64, stick = String::from(sticks[0].name()), gp_stick = tier, stick_id = handed);
    Ok(())
}
ktest!("block.guard.usb_other", Run("bt/65-gp-tiers"), usb_other);
