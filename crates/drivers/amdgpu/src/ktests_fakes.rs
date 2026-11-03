//! GX-7c in-kernel tests of the grown fakes (the same checks run as host tests): the PMFW fake answers Linux's
//! load sequence as the real 780M did, GFXOFF catches an unguarded access, the RLC boots after PSP autoload and
//! EnableGfxImu in either order (and never with an image missing), every start state is what the classifier says
//! and what the GX-7a bring-up meets there, and the scanout watcher sees nothing during the PSP bring-up but each
//! display write. Nothing touches a device: Safe and rerunnable.

use crate::gpucore::classify::{self, Start};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::{self, Crash, StartAs};
use crate::gpucore::gcfake::{BOOTLOAD_DONE, GcHostile, Power};
use crate::gpucore::hwid;
use crate::gpucore::psp::{Carve, Hw, Prior, PspError, Step, cmd};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource, RunError};
use crate::gpucore::smufake::{self, Client, SendError, SmuHostile, linux_send, msg, resp};
use crate::gpucore::ucode::UcodeId;
use crate::dcn;
use crate::gpucore::watch::{self, At, Class, DisplayGuard, Mode, Space, Watch};
use crate::ipmod::{gfx11, gmc11};
use alloc::format;
use alloc::vec::Vec;
use ktest::kassert;

fn at(d: &Discovery, hw: u16, r: (usize, u32)) -> Result<u32, alloc::string::String> {
    d.reg(hw, 0, r.0, r.1).and_then(|x| x.checked_mul(4)).ok_or_else(|| format!("no address for hw {hw} {r:?}"))
}

/// Sends with Linux's protocol (10 us polls, 2 s limit).
fn send(f: &mut FakeGpu, id: u32, arg: u32, wait: bool) -> Result<(u32, u32), SendError> {
    let m = f.smu.drv;
    linux_send(f, &m, id, arg, wait, 10, 2_000_000)
}

pub(crate) fn check_smu() -> ktest::TestResult {
    let mut f = FakeGpu::new(Hostile::None);
    let mut prev = 0;
    for &(id, arg, out) in smufake::LINUX_LOAD {
        let t0 = f.now;
        let wait = id != msg::ENABLE_GFX_IMU;
        let (r, a) = send(&mut f, id, arg, wait).map_err(|e| format!("{id:#x}: {e:?}"))?;
        if wait {
            kassert!(r == resp::OK, "{id:#x} answered {r:#x}");
            if let Some(o) = out {
                kassert!(a == o, "{id:#x} gave {a:#x}, the 780M {o:#x}");
            }
        }
        if prev == msg::ENABLE_GFX_IMU {
            // EnableGfxImu has no waiting caller: the next message's pre-poll waits for its answer (29.3 ms).
            kassert!(f.now - t0 >= smufake::latency_us(msg::ENABLE_GFX_IMU), "pre-poll waited only {} us", f.now - t0);
        }
        prev = id;
    }
    kassert!(f.smu.taken.len() == smufake::LINUX_LOAD.len() && f.smu.overlaps.is_empty(), "taken {} overlaps {:?}", f.smu.taken.len(), f.smu.overlaps);
    f.delay_us(f.gc.wake_us);
    kassert!(f.gc.imu_started && f.gc.power == Power::On, "GC {:?}", f.gc.power);
    kassert!(f.smu.dram == (0x80, 0x1e_d000) && !f.smu.vcn_on && !f.smu.jpeg_on);

    // FIRMWARE_FLAGS through the PCIe index/data window (smu_v13_0_check_fw_status).
    let (idx, data) = f.smn_window;
    kassert!(f.reg_write(idx, smufake::SMN_FIRMWARE_FLAGS) && f.reg_read(data) == Some(smufake::FIRMWARE_FLAGS_UP));
    kassert!(f.reg_write(idx, 0x1234_5678) && f.reg_read(data) == Some(0) && f.smn_unknown == [0x1234_5678]);

    // Hostile answers, each with what a Linux-style client sees.
    let mut h = FakeGpu::new(Hostile::None);
    h.smu.hostile = alloc::vec![SmuHostile::Answer { id: msg::GET_PMFW_VERSION, code: resp::PREREQ }, SmuHostile::Late { id: msg::TEST, us: 500_000 }];
    kassert!(matches!(send(&mut h, msg::GET_PMFW_VERSION, 0, true), Ok((resp::PREREQ, _))));
    let t0 = h.now;
    kassert!(matches!(send(&mut h, msg::TEST, 41, true), Ok((resp::OK, 42))) && h.now - t0 >= 500_000);
    kassert!(matches!(send(&mut h, 0x30, 0, true), Ok((resp::UNKNOWN, _))), "an id past the table");
    kassert!(matches!(send(&mut h, msg::TRANSFER_TABLE_SMU2DRAM, 4, true), Ok((resp::PREREQ, _))), "a table transfer before the DRAM address");
    let mut never = FakeGpu::new(Hostile::None);
    never.smu.hostile.push(SmuHostile::Never { id: msg::ALLOW_GFXOFF });
    kassert!(send(&mut never, msg::ALLOW_GFXOFF, 0, true) == Err(SendError::Timeout));
    kassert!(send(&mut never, msg::TEST, 0, true) == Err(SendError::PrePoll { last: msg::ALLOW_GFXOFF }), "the next pre-poll sees PMFW stuck");
    let mut dead = FakeGpu::new(Hostile::None);
    dead.smu.hostile.push(SmuHostile::Dead);
    let (idx, data) = dead.smn_window;
    kassert!(dead.reg_write(idx, smufake::SMN_FIRMWARE_FLAGS) && dead.reg_read(data) == Some(0), "FIRMWARE_FLAGS 0 when PMFW is down");
    kassert!(send(&mut dead, msg::TEST, 0, true) == Err(SendError::Timeout));

    // Writing over a message in flight (no pre-poll): the newer one answers busy and is not taken.
    let mut b = FakeGpu::new(Hostile::None);
    let m = b.smu.drv;
    kassert!(b.reg_write(m.msg_at, msg::POWER_UP_VCN) && b.reg_write(m.msg_at, msg::TEST));
    b.delay_us(10_000);
    kassert!(b.smu.overlaps == [(Client::Driver, msg::TEST)] && b.reg_read(m.resp_at) == Some(resp::BUSY));

    // The display's mailbox is its own: a message on each, both answered, ids never mixed.
    let mut two = FakeGpu::new(Hostile::None);
    let (dm, dd) = (two.smu.drv, two.smu.dcn);
    for (mb, id, arg) in [(dm, msg::POWER_UP_VCN, 0u32), (dd, msg::GET_PMFW_VERSION, 0)] {
        kassert!(two.reg_write(mb.resp_at, 0) && two.reg_write(mb.arg_at, arg) && two.reg_write(mb.msg_at, id));
    }
    two.delay_us(2_000);
    kassert!(two.reg_read(dm.resp_at) == Some(resp::OK) && two.reg_read(dd.resp_at) == Some(resp::OK) && two.reg_read(dd.arg_at) == Some(smufake::PMFW_VERSION));
    kassert!(two.smu.vcn_on && two.smu.taken.iter().map(|t| t.0).collect::<Vec<_>>() == [Client::Driver, Client::Display]);
    Ok(())
}
ktest::ktest!("gx.fake.smu", Safe, check_smu);

pub(crate) fn check_gc() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let cp_stat = at(&d, hwid::GC, gfx11::CP_STAT)?;
    let boot = at(&d, hwid::GC, gfx11::RLC_RLCS_BOOTLOAD_STATUS_11_0_1)?;
    let flags = at(&d, hwid::GC, gfx11::GFX_IMU_MSG_FLAGS)?;
    let smuio = at(&d, hwid::SMUIO, crate::ipmod::smu13::SMUIO_GFX_MISC_CNTL)?;
    let me = at(&d, hwid::GC, gfx11::CP_ME_CNTL)?;
    let mut f = FakeGpu::new(Hostile::None);
    // Cold: GC power-gated. The status registers say so; any other GC access is garbage and recorded.
    kassert!(f.reg_read(smuio) == Some(0) && f.reg_read(flags) == Some(crate::gpucore::gcfake::IMU_FLAGS_COLD) && f.gc.off_access.is_empty());
    kassert!(f.reg_read(cp_stat) == Some(u32::MAX) && f.reg_write(me, 0));
    kassert!(f.gc.off_access == [(cp_stat, false), (me, true)], "{:?}", f.gc.off_access);

    // Order 1: PSP autoload first (the GX-7a sequence), then EnableGfxImu.
    let src = pspfake::synthetic_780m();
    pspfake::run(&mut f, &mut MapSource(src.clone())).map_err(|e| format!("{e:?}"))?;
    kassert!(f.gc.autoload.as_ref().is_some_and(|m| m.is_empty()) && !f.gc.booted);
    send(&mut f, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
    f.delay_us(f.gc.boot_us + f.gc.wake_us);
    kassert!(f.reg_read(smuio) == Some(2 << 1) && f.reg_read(flags) == Some(crate::gpucore::gcfake::IMU_FLAGS_LATER));
    kassert!(f.reg_read(boot) == Some(BOOTLOAD_DONE) && f.reg_read(cp_stat) == Some(0), "boot {:?}", f.reg_read(boot));
    kassert!(f.reg_read(me) == Some(crate::gpucore::gcfake::ME_HALTED), "the lost write did not land");
    // AllowGfxOff: GC gates; a read now gives all ones (counted: after the IMU start it is no guard's miss).
    let (before, powering) = (f.gc.off_access.len(), f.gc.powering_reads);
    send(&mut f, msg::ALLOW_GFXOFF, 0, true).map_err(|e| format!("{e:?}"))?;
    f.delay_us(f.gc.sleep_us);
    kassert!(f.reg_read(smuio) == Some(0) && f.reg_read(cp_stat) == Some(u32::MAX) && f.gc.off_access.len() == before && f.gc.powering_reads == powering + 1);
    send(&mut f, msg::DISALLOW_GFXOFF, 0, true).map_err(|e| format!("{e:?}"))?;
    f.delay_us(f.gc.wake_us);
    kassert!(f.reg_read(smuio) == Some(2 << 1) && f.reg_read(cp_stat) == Some(0));

    // Order 2: EnableGfxImu first, PSP autoload after.
    let mut g = FakeGpu::new(Hostile::None);
    send(&mut g, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
    g.delay_us(g.gc.boot_us * 2);
    kassert!(g.gc.power == Power::On && !g.gc.booted, "no boot without autoload");
    pspfake::run(&mut g, &mut MapSource(src.clone())).map_err(|e| format!("{e:?}"))?;
    g.delay_us(g.gc.boot_us);
    kassert!(g.reg_read(boot) == Some(BOOTLOAD_DONE));

    // Never: the hostile RLC, an image missing from the TMR, GC stuck in GFXOFF.
    for case in 0..3 {
        let mut n = FakeGpu::new(Hostile::None);
        match case {
            0 => n.gc.hostile = GcHostile::AutoloadNever,
            1 => {}
            _ => n.gc.hostile = GcHostile::StuckOff,
        }
        if case == 1 {
            let types: Vec<u32> = crate::gpucore::gcfake::AUTOLOAD_NEEDS.iter().filter(|u| **u != UcodeId::ImuD).map(|u| u.fw_type()).collect();
            let now = n.now;
            n.gc.autoload_rlc(&types, now);
            kassert!(n.gc.autoload.as_deref() == Some(&[UcodeId::ImuD][..]), "missing {:?}", n.gc.autoload);
        } else {
            pspfake::run(&mut n, &mut MapSource(src.clone())).map_err(|e| format!("{e:?}"))?;
        }
        send(&mut n, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
        n.delay_us(1_000_000);
        kassert!(!n.gc.booted && n.reg_read(boot) != Some(BOOTLOAD_DONE), "case {case} booted");
        kassert!((case == 2) == (n.reg_read(smuio) == Some(1 << 1)), "case {case}: GC {:?}", n.gc.power);
    }
    // GX-7f: GfxDeviceDriverReset as measured on 28.10.: taken, never answered; the fake shows nothing else.
    kassert!(send(&mut g, msg::GFX_DEVICE_DRIVER_RESET, 2, true).is_err() && g.gc.resets == 0 && g.gc.booted);
    // A PMFW that answers it (a model, not 13.0.4 as measured): the boot is undone.
    let mut h = FakeGpu::new(Hostile::None);
    h.smu.mode2_answers = true;
    pspfake::run(&mut h, &mut MapSource(src.clone())).map_err(|e| format!("{e:?}"))?;
    send(&mut h, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
    h.delay_us(h.gc.boot_us + h.gc.wake_us);
    kassert!(h.gc.booted);
    send(&mut h, msg::GFX_DEVICE_DRIVER_RESET, 2, true).map_err(|e| format!("{e:?}"))?;
    kassert!(h.gc.resets == 1 && !h.gc.booted);
    // GX-7j3, the 29.10. model: a second load (AUTOLOAD_RLC) under the IMU of a GFX driver that never unloaded
    // wedges PMFW (no driver message answered again); after PrepareMp1ForUnload it does not, and EnableGfxImu
    // makes the driver loaded again. A mode-2 reset takes the IMU, so the next load is cold.
    let boot_once = |w: &mut FakeGpu| -> Result<(), alloc::string::String> {
        pspfake::run(w, &mut MapSource(pspfake::synthetic_780m())).map_err(|e| format!("{e:?}"))?;
        send(w, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
        w.delay_us(w.gc.boot_us + w.gc.wake_us + 100_000);
        Ok(())
    };
    let mut w = FakeGpu::new(Hostile::None);
    boot_once(&mut w)?;
    kassert!(w.gc.booted && !w.smu.unloaded && !w.smu.wedged);
    w.smu.warm_load(w.gc.imu_started);
    kassert!(w.smu.wedged && send(&mut w, msg::GET_DRIVER_IF_VERSION, 0, true).is_err(), "not wedged");
    let mut u = FakeGpu::new(Hostile::None);
    boot_once(&mut u)?;
    send(&mut u, msg::PREPARE_MP1_FOR_UNLOAD, 0, true).map_err(|e| format!("{e:?}"))?;
    kassert!(u.smu.unloaded && u.gc.booted, "unload");
    u.smu.warm_load(u.gc.imu_started);
    kassert!(!u.smu.wedged && send(&mut u, msg::GET_DRIVER_IF_VERSION, 0, true).is_ok(), "wedged after the unload");
    send(&mut u, msg::ENABLE_GFX_IMU, 1, false).map_err(|e| format!("{e:?}"))?;
    kassert!(!u.smu.unloaded, "EnableGfxImu loads the driver again");
    kassert!(!h.gc.imu_started, "mode 2 kept the IMU");
    Ok(())
}
ktest::ktest!("gx.fake.gc", Safe, check_gc);

/// Every start the fake can take, what the classifier says, and what the PSP bring-up meets there (GX-7d: a
/// stale ring and TMR are destroyed first, so every start but PspDown loads; the classifier's verdict is the
/// hint for the TMR nobody can see).
pub(crate) fn check_stale(d: &Discovery) -> ktest::TestResult {
    let starts = [
        (StartAs::Cold, Start::Cold),
        (StartAs::AfterLinux, Start::AfterLinux),
        (StartAs::AfterBitsOs, Start::AfterBitsOs),
        (StartAs::AfterCrash(Crash::RingLeft), Start::AfterCrash),
        (StartAs::AfterCrash(Crash::SmuUnanswered), Start::AfterCrash),
        (StartAs::AfterCrash(Crash::GcWithoutRlc), Start::AfterCrash),
        // A TMR has no register: the classifier cannot see it (the PSP's SETUP_TMR answer shows it).
        (StartAs::AfterCrash(Crash::TmrLeft), Start::Cold),
        (StartAs::PspDown, Start::PspDown),
    ];
    for size in [512u64 << 20, 8 << 30, 16 << 30] {
        for (s, want) in starts {
            let mut f = FakeGpu::with_carve(d, Hostile::None, size);
            f.start_as(d, s);
            let note = f.stop_note(s);
            let st = classify::classify_with(d, &mut |b| f.reg_read(b), note.as_ref());
            kassert!(st.start == want, "{s:?} at {size:#x}: {:?} {:?}", st.start, st.why);
            if note.is_some() {
                // Without the note the PSP's kept registers look like a crash (the desk's 28.10. Reload).
                let bare = classify::classify(d, &mut |b| f.reg_read(b));
                kassert!(bare.start == Start::AfterCrash && bare.why.iter().any(|w| w.contains("not known destroyed")), "{s:?}: {:?}", bare.why);
            }
            kassert!(st.value("RCC_CONFIG_MEMSIZE") == Some((size >> 20) as u32), "MEMSIZE for {size:#x}");
            kassert!(f.gc.off_access.is_empty(), "{s:?}: the classifier read GC while off: {:?}", f.gc.off_access);
            let r = pspfake::run(&mut f, &mut MapSource(pspfake::synthetic_780m()));
            match (s, r) {
                (StartAs::AfterLinux | StartAs::AfterCrash(Crash::RingLeft | Crash::TmrLeft), Ok(r)) => {
                    // The registers show a ring: destroyed first, then the TMR nobody can see, before LOAD_TOC.
                    kassert!(r.report.prior == Some(Prior::Stale), "{s:?}: {:?}", r.report.prior);
                    kassert!(r.report.steps.iter().any(|x| matches!(x, Step::StaleRingDestroyed { .. })), "{s:?}: {:?}", r.report.steps);
                    kassert!(r.report.steps.contains(&Step::StaleTmrDestroyed { status: 0 }), "{s:?}: {:?}", r.report.steps);
                    kassert!(f.cmds.first() == Some(&cmd::DESTROY_TMR) && f.cmds.get(1) == Some(&cmd::LOAD_TOC), "{s:?}: {:?}", f.cmds);
                    kassert!(r.report.warnings.is_empty() && f.order_errors.is_empty(), "{s:?}: {:?}", r.report.warnings);
                }
                (StartAs::PspDown, Err(RunError::Psp(PspError::SosNotRunning))) => {}
                (StartAs::AfterBitsOs, Ok(r)) => {
                    // The kept write pointer says "a ring was used": destroyed first (no ring is left, so the
                    // stale DESTROY_RINGS is skipped or harmless); no TMR is left either: DESTROY_TMR answers
                    // 0xffff0007, tolerated (the Mini-PC 28.10.).
                    kassert!(r.report.prior == Some(Prior::Stale), "{s:?}: {:?}", r.report.prior);
                    kassert!(r.report.warnings == [(cmd::DESTROY_TMR, pspfake::NO_TMR)] && f.order_errors.is_empty(), "{s:?}: {:?}", r.report.warnings);
                }
                (StartAs::Cold | StartAs::AfterCrash(Crash::SmuUnanswered | Crash::GcWithoutRlc), Ok(r)) => {
                    kassert!(r.report.warnings.is_empty() && f.order_errors.is_empty(), "{s:?}: {:?}", r.report.warnings);
                }
                (s, r) => return Err(format!("{s:?} at {size:#x}: {:?}", r.map(|r| r.report.warnings))),
            }
            // A TMR left with clean ring registers (no register shows it): without the hint SETUP_TMR fails
            // (fatal); the classifier's verdict as the hint destroys it first (`pspbringup::admit`).
            if s == StartAs::AfterCrash(Crash::TmrLeft) {
                let mut t = FakeGpu::with_carve(d, Hostile::None, size);
                t.start_as(d, s);
                t.set_reg(t.a.c2pmsg_71, 0);
                let r = pspfake::run(&mut t, &mut MapSource(pspfake::synthetic_780m()));
                kassert!(matches!(r, Err(RunError::Psp(PspError::CmdStatus { cmd: cmd::SETUP_TMR, .. }))), "hidden TMR passed");
                kassert!(t.ring.is_none(), "ring left after the failed start");
            }
        }
    }
    Ok(())
}

fn stale() -> ktest::TestResult {
    check_stale(&fakechip::discovery_780m())
}
ktest::ktest!("gx.fake.stale", Safe, stale);

pub(crate) fn check_watch(d: &Discovery, dcn_reg: u32) -> ktest::TestResult {
    let guard = DisplayGuard::from_discovery(d);
    kassert!(guard.class(dcn_reg) == Some(Class::Dcn), "{dcn_reg:#x} not DCN");
    // The whole PSP bring-up behind the watcher: nothing for the display.
    let fake = FakeGpu::new(Hostile::None);
    let (a, carve, size) = (fake.a, Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys }, fake.fb_size);
    let mut w = Watch::new(fake, guard.clone(), Mode::Record);
    pspfake::run_via(&mut w, a, carve, size, &mut MapSource(pspfake::synthetic_780m())).map_err(|e| format!("{e:?}"))?;
    kassert!(w.violations().is_empty() && w.hw().display_writes.is_empty(), "{:?}", w.violations());
    // Each kind of display write is caught; in Block mode it never reaches the device.
    let fb_base = at(d, hwid::MMHUB, gmc11::MMHUB_FB_LOCATION_BASE)?;
    let dcn_msg = d.reg(hwid::MP1, 0, 1, 0x283).and_then(|x| x.checked_mul(4)).ok_or("no MP1")?;
    let before = w.reg_read(fb_base);
    kassert!(w.reg_write(dcn_reg, 1) && w.reg_write(fb_base, 0x8000) && w.reg_write(dcn_msg, 2));
    let v = w.violations();
    kassert!(v.len() == 3 && v[0].class == Class::Dcn && v[1].class == Class::Mmhub("MMMC_VM_FB_LOCATION_BASE") && v[1].before == before);
    kassert!(matches!(v[2].class, Class::DcnMailbox(_)) && w.hw().display_writes.len() == 3, "the fake records them too");
    let mut b = Watch::new(FakeGpu::new(Hostile::None), guard, Mode::Block);
    kassert!(!b.reg_write(dcn_reg, 1) && b.hw().writes.is_empty() && b.violations()[0].blocked);
    // Scanout memory: a write overlapping the protected range is caught and blocked.
    let mut g2 = DisplayGuard::from_discovery(d);
    g2.protect(pspfake::FB_MC, 8 << 20);
    let mut m = Watch::new(FakeGpu::new(Hostile::None), g2, Mode::Block);
    kassert!(!m.mem_write(pspfake::FB_MC + (8 << 20) - 2, &[1, 2, 3, 4]) && m.mem_write(pspfake::FB_MC + (8 << 20), &[1]));
    kassert!(m.violations().len() == 1 && m.violations()[0].at == At::Mem { mc: pspfake::FB_MC + (8 << 20) - 2, len: 4 });
    check_watch_indirect(d, dcn_reg, dcn_msg)
}

/// GX-7c fixer: the index/data windows reach display registers too; the record is bounded.
fn check_watch_indirect(d: &Discovery, dcn_reg: u32, dcn_msg: u32) -> ktest::TestResult {
    let guard = DisplayGuard::from_discovery(d);
    let index2 = at(d, hwid::NBIF, (0, 0xe))?;
    let smn = guard.windows.iter().find(|w| w.space == Space::Smn && w.index == index2).copied().ok_or("no PCIE_INDEX2 window")?;
    // Record: a DCN register through PCIE_INDEX2/DATA2 (BAR5 identity) reaches the fake and is flagged there too.
    let mut w = Watch::new(FakeGpu::new(Hostile::None), guard.clone(), Mode::Record);
    kassert!(w.reg_write(smn.index, dcn_reg) && w.reg_write(smn.data, 7));
    kassert!(w.violations().len() == 1 && w.violations()[0].at == At::Indirect { data: smn.data, target: u64::from(dcn_reg) }, "{:?}", w.violations());
    kassert!(w.hw().display_writes == [(dcn_reg, 7)], "fake: {:?}", w.hw().display_writes);
    // The display mailbox at its MP1 SMN alias; a harmless SMN target (FIRMWARE_FLAGS) passes.
    let alias = watch::MP1_SMN_SEG1[1] + (dcn_msg - at(d, hwid::MP1, (1, 0))?);
    kassert!(w.reg_write(smn.index, alias) && w.reg_write(smn.data, 1) && w.violations().len() == 2 && matches!(w.violations()[1].class, Class::DcnMailbox(_)));
    kassert!(w.reg_write(smn.index, smufake::SMN_FIRMWARE_FLAGS) && w.reg_write(smn.data, 1) && w.violations().len() == 2);
    // Block: the data write never reaches the device; the index write does.
    let mut b = Watch::new(FakeGpu::new(Hostile::None), guard.clone(), Mode::Block);
    kassert!(b.reg_write(smn.index, dcn_reg) && !b.reg_write(smn.data, 7));
    kassert!(b.hw().writes == [(smn.index, dcn_reg)] && b.violations()[0].blocked, "{:?}", b.hw().writes);
    // MM_INDEX: bit 31 clear is a register byte; with bit 31 and MM_INDEX_HI a graphics memory position.
    let mut g = guard.clone();
    g.vram_mc = Some(pspfake::FB_MC);
    g.protect(pspfake::FB_MC + (1 << 32), 4096);
    let mut m = Watch::new(FakeGpu::new(Hostile::None), g, Mode::Block);
    kassert!(m.reg_write(dcn::MM_INDEX, dcn_reg) && !m.reg_write(dcn::MM_DATA, 1));
    kassert!(m.reg_write(dcn::MM_INDEX, 0x8000_0000) && m.reg_write(dcn::MM_INDEX_HI, 2) && !m.reg_write(dcn::MM_DATA, 1));
    kassert!(m.reg_write(dcn::MM_INDEX_HI, 0) && m.reg_write(dcn::MM_DATA, 1), "position 0 is not protected");
    kassert!(m.violations().len() == 2 && m.violations()[1].at == At::Indirect { data: dcn::MM_DATA, target: 1 << 32 }, "{:?}", m.violations());
    // A loop on a display register: the record keeps `keep`, counts all, and keeps blocking.
    let mut c = Watch::with_keep(FakeGpu::new(Hostile::None), guard, Mode::Block, 4);
    for _ in 0..100 {
        kassert!(!c.reg_write(dcn_reg, 1));
    }
    kassert!(c.violations().len() == 4 && c.total() == 100 && c.hw().writes.is_empty());
    Ok(())
}

fn watch() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let hubp = dcn314_regs::lookup("HUBPREQ0_DCSURF_PRIMARY_SURFACE_ADDRESS").ok_or("no HUBPREQ0 register")?.byte;
    check_watch(&d, hubp)
}
ktest::ktest!("gx.fake.watch", Safe, watch);
