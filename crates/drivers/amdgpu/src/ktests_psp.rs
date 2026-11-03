//! GX-7a in-kernel tests: the PSP bring-up against the fake (`gpucore::pspfake`), in QEMU and on the Mini-PC
//! alike, since nothing here touches a device: the synthetic firmware, hostile answers, and the real files from
//! the stick's /boot/BITSOS2/firmware/amdgpu when the image has them. All state is local: rerunnable (Safe).

use crate::gpucore::fwload::{self, FwSource, LoadError, StickSource};
use crate::gpucore::psp::{self, PspError, cmd};
use crate::gpucore::pspfake::{self, EXPECT_780M, FakeGpu, Hostile, MapSource, RunError};
use alloc::format;
use alloc::vec::Vec;
use ktest::kassert;

/// Checks one good run: Linux's order, every copy the fake read equals the file's part, RLC autoload right
/// after RLC_G, the TMR where SETUP_TMR said, then the undo (DESTROY_TMR, DESTROY_RINGS) and every buffer back.
pub(crate) fn check_run(fake: &mut FakeGpu, src: &mut dyn FwSource) -> ktest::TestResult {
    let pspfake::Run { report, mut psp, mut bo, fw, list } = pspfake::run(fake, src).map_err(|e| format!("{e:?}"))?;
    let ids: Vec<_> = list.iter().map(|u| u.id).collect();
    kassert!(ids == EXPECT_780M, "order {ids:?}");
    kassert!(fake.loaded.len() == list.len(), "fake saw {} loads", fake.loaded.len());
    for (u, seen) in list.iter().zip(&fake.loaded) {
        kassert!(seen.fw_type == u.id.fw_type() && seen.size == u.size(), "{:?}: {seen:?}", u.id);
        kassert!(seen.sum == pspfake::fnv(fw.bytes(u)), "{:?}: copy differs", u.id);
    }
    let mut want: Vec<u32> = alloc::vec![cmd::LOAD_TOC, cmd::SETUP_TMR];
    for u in &list {
        want.push(cmd::LOAD_IP_FW);
        if u.id == crate::gpucore::ucode::UcodeId::RlcG {
            want.push(cmd::AUTOLOAD_RLC);
        }
    }
    kassert!(fake.cmds == want, "commands {:?}", fake.cmds);
    kassert!(fake.autoload, "RLC autoload not started");
    // GX-7c: Linux's order, the TMR untouched by the CPU, no display register or scanout memory written, and
    // the RLC autoload got every image it needs.
    kassert!(fake.order_errors.is_empty(), "out of Linux's order: {:?}", fake.order_errors);
    kassert!(fake.tmr_writes.is_empty() && fake.display_writes.is_empty() && fake.scanout_writes.is_empty(), "tmr {:?} display {:?}", fake.tmr_writes, fake.display_writes);
    kassert!(fake.gc.autoload.as_ref().is_some_and(|m| m.is_empty()), "autoload missing {:?}", fake.gc.autoload);
    kassert!(report.warnings.is_empty(), "warnings {:?}", report.warnings);
    // GX-7d: a cold fake is fresh (no destroy first), never DMCUB or VCN, one snapshot per step.
    kassert!(report.prior == Some(psp::Prior::Fresh) && report.snaps.len() == report.steps.len(), "prior {:?}", report.prior);
    let banned = [crate::gpucore::ucode::UcodeId::Dmcub.fw_type(), crate::gpucore::ucode::UcodeId::Vcn.fw_type()];
    kassert!(fake.loaded.iter().all(|l| !banned.contains(&l.fw_type)), "DMCUB or VCN loaded");
    let (tmc, tsize) = fake.tmr.ok_or("no TMR")?;
    kassert!(tmc % psp::TMR_ALIGN == 0 && tsize == pspfake::TOC_TMR, "TMR {tmc:#x} {tsize:#x}");
    kassert!(psp.fence_value() == want.len() as u32, "fence {}", psp.fence_value());
    psp.stop(fake).map_err(|e| format!("stop: {e:?}"))?;
    kassert!(fake.ring.is_none() && fake.tmr.is_none(), "undo left ring {:?} tmr {:?}", fake.ring, fake.tmr);
    psp.free(&mut bo);
    kassert!(bo.free_bytes() == fake.fb_size && bo.count() == 0, "buffers not returned");
    Ok(())
}

fn fake_run() -> ktest::TestResult {
    check_run(&mut FakeGpu::new(Hostile::None), &mut MapSource(pspfake::synthetic_780m()))?;
    klog::event!(Info, "ktest", "gx.psp.fake", ok = true);
    Ok(())
}
ktest::ktest!("gx.psp.fake", Safe, fake_run);

/// Every failing hostile mode with the error it must end in (GX-7d: a non-zero status is fatal too). `Hold` (the
/// ring fills; driven frame by frame in the host test) is checked on its own.
pub(crate) fn hostile_cases() -> Vec<(Hostile, fn(&PspError) -> bool)> {
    let a = FakeGpu::new(Hostile::None).a;
    alloc::vec![
        (Hostile::SosDead, |e| *e == PspError::SosNotRunning),
        (Hostile::NotReady, |e| matches!(e, PspError::Timeout { what: "ring.ready", .. })),
        (Hostile::Silent, |e| matches!(e, PspError::Timeout { what: "ring.create", .. })),
        (Hostile::RingStatus(7), |e| *e == PspError::RingStatus { status: 7 }),
        (Hostile::AllOnes, |e| matches!(e, PspError::Gone { .. })),
        (Hostile::RefuseWrite(a.c2pmsg_69), |e| matches!(e, PspError::WriteRefused { .. })),
        (Hostile::NoFence, |e| matches!(e, PspError::Timeout { what: "fence", .. })),
        (Hostile::WrongFence, |e| matches!(e, PspError::Timeout { what: "fence", .. })),
        (Hostile::Wptr(5), |e| *e == PspError::RingBounds { wptr: 5 }),
        (Hostile::Wptr(1024 + 16), |e| *e == PspError::RingBounds { wptr: 1040 }),
        (Hostile::TocTmr(0), |e| *e == PspError::TmrSize(0)),
        (Hostile::TocTmr(0x4000_0000), |e| matches!(e, PspError::Bo(_))),
        (Hostile::MemFail, |e| matches!(e, PspError::Mem { .. })),
        (Hostile::WptrKept(0x40), |e| *e == PspError::RingNotReset { wptr: 0x40 }),
        (Hostile::CmdStatus { id: cmd::LOAD_TOC, status: 3 }, |e| *e == PspError::CmdStatus { cmd: cmd::LOAD_TOC, status: 3, ucode: None }),
        (Hostile::CmdStatus { id: cmd::SETUP_TMR, status: 7 }, |e| *e == PspError::CmdStatus { cmd: cmd::SETUP_TMR, status: 7, ucode: None }),
        (Hostile::CmdStatus { id: cmd::LOAD_IP_FW, status: 9 }, |e| {
            *e == PspError::CmdStatus { cmd: cmd::LOAD_IP_FW, status: 9, ucode: Some(crate::gpucore::ucode::UcodeId::SdmaTh0) }
        }),
        (Hostile::CmdStatus { id: cmd::AUTOLOAD_RLC, status: 5 }, |e| *e == PspError::CmdStatus { cmd: cmd::AUTOLOAD_RLC, status: 5, ucode: None }),
    ]
}

/// Adding a hostile mode fails to compile here until it is placed in `hostile_cases` or tested on its own.
#[allow(dead_code)]
fn hostile_covered(h: Hostile) {
    match h {
        Hostile::None | Hostile::CmdStatus { .. } | Hostile::Hold | Hostile::WptrKept(_) => {}
        Hostile::SosDead | Hostile::NotReady | Hostile::Silent | Hostile::RingStatus(_) | Hostile::AllOnes => {}
        Hostile::RefuseWrite(_) | Hostile::NoFence | Hostile::WrongFence | Hostile::Wptr(_) => {}
        Hostile::TocTmr(_) | Hostile::MemFail => {}
        // GX-7d fixer: the undo's and the stale path's answers, in ktests_bringup::check_failures.
        Hostile::StopSilent => {}
    }
}

/// Each hostile fake ends in its own error within the bounded waits (the fake's clock: no real time passes),
/// and the failed start leaves no ring running on the fake (start's own undo).
pub(crate) fn check_hostile() -> ktest::TestResult {
    for (h, ok) in hostile_cases() {
        let mut fake = FakeGpu::new(h);
        match pspfake::run(&mut fake, &mut MapSource(pspfake::synthetic_780m())) {
            Err(RunError::Psp(e)) => kassert!(ok(&e), "{h:?}: wrong error {e:?}"),
            Err(e) => return Err(format!("{h:?}: {e:?}").into()),
            Ok(_) => return Err(format!("{h:?}: bring-up passed").into()),
        }
        kassert!(fake.now <= 3_000_000, "{h:?}: waited {} us of fake time", fake.now);
        kassert!(fake.ring.is_none(), "{h:?}: ring left running {:?}", fake.ring);
        kassert!(!fake.cmds.contains(&cmd::DESTROY_TMR) || fake.cmds.contains(&cmd::SETUP_TMR), "{h:?}: DESTROY_TMR without a TMR");
        // A failed start leaves no TMR either: DESTROY_TMR went out whenever SETUP_TMR had been accepted.
        kassert!(fake.tmr.is_none(), "{h:?}: TMR left {:?}", fake.tmr);
    }
    Ok(())
}

fn hostile() -> ktest::TestResult {
    check_hostile()
}
ktest::ktest!("gx.psp.hostile", Safe, hostile);

/// The real files from the stick (xtask image copies them, decompressed, with LICENSE.amdgpu): headers checked,
/// the list in Linux's order, the whole sequence against the fake. Skips when the image has none.
fn stick_files() -> ktest::TestResult {
    if !wait_stick() {
        return ktest::skip("no boot stick mounted at /boot within 30 s");
    }
    let first = fwload::stick_path("amdgpu/psp_13_0_4_toc.bin");
    match StickSource.read(&first) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let zst = StickSource.read(&format!("{first}.zst")).ok().flatten().is_some();
            kassert!(!zst, "{first}.zst on the stick: xtask must decompress");
            return ktest::skip("no firmware on the image (desk CR: xtask image copies linux-firmware-amdgpu)");
        }
        Err(LoadError::Read { errno, .. }) => return Err(format!("{first}: errno {errno}").into()),
        Err(e) => return Err(format!("{e:?}").into()),
    }
    let lic = StickSource.read(LICENSE).map_err(|e| format!("{e:?}"))?;
    kassert!(lic.is_some_and(|l| !l.is_empty()), "{LICENSE} missing beside the files");
    check_run(&mut FakeGpu::new(Hostile::None), &mut StickSource)?;
    klog::event!(Info, "ktest", "gx.fw.stick", ok = true);
    Ok(())
}
ktest::ktest!("gx.fw.stick", Safe, stick_files);

/// The stick enumerates after the kernel starts (USB): wait, bounded, for its STICK.ID under /boot.
fn wait_stick() -> bool {
    let end = ksync::now_ns().unwrap_or(0).saturating_add(30_000_000_000);
    loop {
        if matches!(StickSource.read("/boot/BITSOS2/STICK.ID"), Ok(Some(_))) {
            return true;
        }
        if ksync::now_ns().unwrap_or(u64::MAX) >= end {
            return false;
        }
        sched::sleep_ns(20_000_000);
    }
}

/// linux-firmware's licence for the amdgpu files, next to them on the stick.
const LICENSE: &str = "/boot/BITSOS2/firmware/LICENSE.amdgpu";
