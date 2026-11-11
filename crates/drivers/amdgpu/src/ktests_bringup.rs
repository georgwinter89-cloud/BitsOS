//! GX-7d in-kernel tests of the PSP run the desk starts on the Mini-PC (`gx_bringup`), against the fakes: the same
//! engine (`gpucore::pspbringup::run`) over the same owner stack (`Bringup<Watch<_>>`, `Mode::Block`) as the live
//! run, for every start state and carve-out sizes from 512 MiB to 16 GiB; M1 and M2; a kept ring and its Quiesce
//! undo; every failure answer; DMCUB and VCN refused; the watcher stopping a buffer over the scanout. The real
//! PSP is never touched here (its run is an MCP tool, not a ktest: writes to the PSP are not rerun-anywhere
//! safe). Nothing touches a device: Safe and rerunnable.

use crate::bo::{BoManager, DISCOVERY_RESERVE, Reserved};
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP};
use crate::gpucore::classify::{self, Start};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::{self, Crash, StartAs};
use crate::gpucore::fwload;
use crate::gpucore::gcfake;
use crate::gpucore::handoff;
use crate::gpucore::gfxup::{self, GfxError};
use crate::gpucore::smu::{self, SmuError};
use crate::gpucore::smufake;
use crate::gpucore::psp::{Carve, Hw, PspError, Step, Timeouts, Until, cmd};
use crate::gpucore::pspbringup::{self, Inputs, Outcome};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::ucode::{ListOpts, UcodeId};
use crate::gpucore::watch::{DisplayGuard, Mode, Watch};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::kassert;

pub(crate) type Owner = Bringup<Watch<FakeGpu>>;

pub(crate) const STARTS: &[StartAs] = &[
    StartAs::Cold,
    StartAs::AfterLinux,
    StartAs::AfterBitsOs,
    StartAs::AfterCrash(Crash::RingLeft),
    StartAs::AfterCrash(Crash::SmuUnanswered),
    StartAs::AfterCrash(Crash::GcWithoutRlc),
    StartAs::AfterCrash(Crash::TmrLeft),
    StartAs::PspDown,
];

/// The GOP surface as the live run sees it: reserved in the BO manager and protected by the watcher.
pub(crate) const GOP: u64 = 8 << 20;

/// One run as the tool makes it: the fake started as `s` with a carve-out of `size`, classified, then the
/// engine with `opts`' list. `protect`: extra MC ranges for the watcher (beyond the GOP surface).
#[allow(clippy::too_many_arguments)]
pub(crate) fn one(d: &Discovery, size: u64, s: StartAs, until: Until, keep: bool, h: Hostile, opts: &ListOpts, protect: &[(u64, u64)]) -> Result<(Box<Outcome>, Box<Owner>, BoManager, Start), String> {
    one_gop(d, size, s, until, keep, h, opts, protect, GOP)
}

/// `one` with a GOP surface of `gop` bytes at offset 0 (any size: 1920x1080x4 is not a whole 2 MiB piece).
#[allow(clippy::too_many_arguments)]
pub(crate) fn one_gop(d: &Discovery, size: u64, s: StartAs, until: Until, keep: bool, h: Hostile, opts: &ListOpts, protect: &[(u64, u64)], gop: u64) -> Result<(Box<Outcome>, Box<Owner>, BoManager, Start), String> {
    one_with(d, size, s, until, keep, h, opts, protect, gop, &|_| {})
}

/// `one_gop` with `setup` applied to the fake after the classifier read it (GX-7e: PMFW and GC hostiles).
#[allow(clippy::too_many_arguments)]
pub(crate) fn one_with(d: &Discovery, size: u64, s: StartAs, until: Until, keep: bool, h: Hostile, opts: &ListOpts, protect: &[(u64, u64)], gop: u64, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager, Start), String> {
    one_variant(d, size, s, until, keep, h, opts, protect, gop, crate::gpucore::gfxring::Variant::DEFAULT, setup)
}

/// `one_with` with the GFX stage's start order (GX-7j2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn one_variant(d: &Discovery, size: u64, s: StartAs, until: Until, keep: bool, h: Hostile, opts: &ListOpts, protect: &[(u64, u64)], gop: u64, v: crate::gpucore::gfxring::Variant, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager, Start), String> {
    let mut f = FakeGpu::with_carve(d, Hostile::None, size);
    f.start_as(d, s);
    // GX-7f: a clean BitsOS stop is told from a crash by its note (the registers alone look the same).
    let note = f.stop_note(s);
    let st = classify::classify_with(d, &mut |b| f.reg_read(b), note.as_ref());
    f.hostile = h;
    setup(&mut f);
    f.reserved.push((f.fb_mc, f.fb_mc + gop));
    let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), opts).map_err(|e| format!("{e:?}"))?;
    let list = fw.load_list(opts).map_err(|e| format!("{e:?}"))?;
    let mut guard = DisplayGuard::from_discovery(d);
    guard.vram_mc = Some(f.fb_mc);
    guard.protect(f.fb_mc, gop);
    for &(mc, len) in protect {
        guard.protect(mc, len);
    }
    let reserved = alloc::vec![
        Reserved { name: String::from("gop"), range: 0..gop },
        Reserved { name: String::from("discovery"), range: size - DISCOVERY_RESERVE..size },
    ];
    let mut bo = BoManager::new(size, reserved);
    let sys = f.sdma.sys_alloc();
    let inp = Inputs {
        addrs: f.a,
        carve: Carve { mc_base: f.fb_mc, phys_base: f.fb_phys },
        start: st.start,
        until,
        fw: &fw,
        list: &list,
        autoload: opts.autoload,
        keep,
        t: Timeouts::LINUX,
        gfx: pspbringup::GfxInputs::from_discovery(d),
        gmc: amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, size).and_then(|c| crate::gpucore::gmc::GmcInputs::new(d, c)),
        sdma: amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, size)
            .and_then(|c| crate::gpucore::sdma::SdmaInputs::new(d, c, crate::gpucore::sdma::burst_nop(&fw)))
            .map(|x| crate::gpucore::sdma::SdmaInputs { sys: Some(&sys), ..x }),
        gfx_ring: amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, size).and_then(|c| crate::gpucore::gfxring::GfxRingInputs::new(d, c)).map(|x| crate::gpucore::gfxring::GfxRingInputs { variant: v, ..x }),
        mes: amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, size).and_then(|c| crate::gpucore::mes::MesInputs::new(d, c, fw.mes_uc_starts())),
        vm: crate::gpucore::vmring::VmAddrs::from_discovery(d),
        // GX-7j3: the fakes' AfterBitsOs is a stop of a kernel before GX-7j3 (its note says Booted, not Unloaded).
        unload_first: pspbringup::unload_first(st.start, false),
    };
    let mut owner = Box::new(Bringup::new(Watch::new(f, guard, Mode::Block), Gate::Allow, LOG_KEEP));
    let out = pspbringup::run(&mut owner, &mut bo, &inp);
    // The answer's lines: the verdict, and one `W` line per write of the run, in order.
    let text = pspbringup::render(&out, &inp);
    let want = if out.passed() { "verdict: PASS" } else { "verdict: FAIL" };
    kassert!(text.last().map(String::as_str) == Some(want), "{s:?}: last line {:?}", text.last());
    let w: Vec<(u32, u32)> = text.iter().filter_map(|l| w_line(l)).collect();
    let e: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).collect();
    kassert!(w == e, "{s:?}: W lines differ from the owner's entries");
    Ok((out, owner, bo, st.start))
}

/// A `W <byte> <value> # ...` line as `regtrace diff --ours` reads it.
pub(crate) fn w_line(l: &str) -> Option<(u32, u32)> {
    let t: Vec<&str> = l.split('#').next()?.split_whitespace().collect();
    let h = |x: &str| u32::from_str_radix(x.trim_start_matches("0x"), 16).ok();
    match t[..] {
        ["W", b, v] => Some((h(b)?, h(v)?)),
        _ => None,
    }
}

/// Every start state at three carve-out sizes, M2 and back (the tool's default end: undone).
pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    let opts = ListOpts::PSP13_0_4;
    for size in [512u64 << 20, 8 << 30, 16 << 30] {
        for &s in STARTS {
            let (out, mut owner, bo, start) = one(d, size, s, Until::Fw, false, Hostile::None, &opts, &[])?;
            let f = owner.hw().hw();
            if s == StartAs::PspDown {
                kassert!(start == Start::PspDown && !out.passed() && out.error == Some(PspError::SosNotRunning));
                kassert!(out.entries.is_empty() && f.writes.is_empty(), "PspDown: written {:?}", f.writes);
                continue;
            }
            kassert!(out.passed(), "{s:?} at {size:#x}: {:?} {:?}", out.error, out.checks);
            kassert!(out.undo == Some(Ok(())) && out.kept.is_none(), "{s:?}: undo {:?}", out.undo);
            kassert!(f.ring.is_none() && f.tmr.is_none(), "{s:?}: left ring {:?} tmr {:?}", f.ring, f.tmr);
            kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty() && f.tmr_writes.is_empty(), "{s:?}: display or TMR touched");
            kassert!(f.order_errors.is_empty() && f.autoload, "{s:?}: order {:?}", f.order_errors);
            kassert!(f.loaded.len() == pspfake::EXPECT_780M.len(), "{s:?}: {} loads", f.loaded.len());
            kassert!(out.entries.len() == f.writes.len() && out.violations_total == 0, "{s:?}: entries");
            // Destroyed first exactly when the classifier or the registers say stale.
            let stale = out.report.steps.iter().any(|x| matches!(x, Step::StaleRingDestroyed { .. }));
            kassert!(stale == (start != Start::Cold || out.report.stale_ring_size != 0 || out.report.stale_wptr != 0), "{s:?}: stale {stale}");
            kassert!(bo.count() == 0 && bo.free_bytes() == size - GOP - DISCOVERY_RESERVE, "{s:?}: buffers kept");
            // The closing undo is marked: one frame (DESTROY_TMR) and DESTROY_RINGS.
            let u = out.undo_from.ok_or("no undo mark")?;
            let tail: Vec<(u32, u32)> = out.entries[u..].iter().map(|e| (e.byte, e.value)).collect();
            kassert!(tail.len() == 2 && tail[0].0 == f.a.c2pmsg_67 && tail[1] == (f.a.c2pmsg_64, crate::gpucore::psp::CTRL_DESTROY_RINGS), "{s:?}: undo {tail:x?}");
        }
    }
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.bringup.starts", Safe, starts);

/// M1 only, a kept M2 and the Quiesce undo, DMCUB/VCN refused, the watcher's last line of defence.
pub(crate) fn check_modes(d: &Discovery) -> ktest::TestResult {
    let opts = ListOpts::PSP13_0_4;
    // M1: create and LOAD_TOC (after a stale start DESTROY_TMR first); no TMR.
    for (s, want) in [(StartAs::Cold, &[cmd::LOAD_TOC][..]), (StartAs::AfterLinux, &[cmd::DESTROY_TMR, cmd::LOAD_TOC][..])] {
        let (out, mut owner, bo, _) = one(d, 512 << 20, s, Until::Ring, false, Hostile::None, &opts, &[])?;
        let f = owner.hw().hw();
        kassert!(out.passed() && out.checks.len() == 2, "{s:?}: {:?} {:?}", out.error, out.checks);
        kassert!(f.cmds == want && f.tmr.is_none() && f.ring.is_none() && bo.count() == 0, "{s:?}: {:?}", f.cmds);
        kassert!(matches!(out.report.steps.last(), Some(Step::Toc { tmr_size }) if *tmr_size == pspfake::TOC_TMR));
    }
    // Kept: ring and TMR stay; the Quiesce path (watcher and device only, busy waits) destroys both.
    let (mut out, mut owner, mut bo, _) = one(d, 8 << 30, StartAs::Cold, Until::Fw, true, Hostile::None, &opts, &[])?;
    let mut p = out.kept.take().ok_or("not kept")?;
    kassert!(out.undo.is_none() && owner.hw().hw().ring.is_some() && owner.hw().hw().tmr.is_some());
    let logged = owner.log().totals().writes;
    p.t = Timeouts::QUIESCE;
    p.stop(owner.hw()).map_err(|e| format!("quiesce stop: {e:?}"))?;
    let f = owner.hw().hw();
    kassert!(f.ring.is_none() && f.tmr.is_none() && f.cmds.last() == Some(&cmd::DESTROY_TMR), "quiesce: {:?}", f.cmds);
    kassert!(owner.log().totals().writes == logged, "the Quiesce path went through the owner's log");
    p.free(&mut bo);
    kassert!(bo.count() == 0);
    // DMCUB (GX-D9) and VCN (not this item) never reach the PSP.
    for (o, id) in [(ListOpts { keep_display_firmware: false, ..opts }, UcodeId::Dmcub), (ListOpts { vcn: true, ..opts }, UcodeId::Vcn)] {
        let (out, mut owner, bo, _) = one(d, 512 << 20, StartAs::Cold, Until::Fw, false, Hostile::None, &o, &[])?;
        kassert!(out.error == Some(PspError::Forbidden(id)) && owner.hw().hw().writes.is_empty() && bo.count() == 0, "{id:?}: {:?}", out.error);
    }
    // A buffer the BO manager would give out over a surface the display reads: the watcher blocks the write,
    // the device never sees it, the run fails before ring create.
    let (out, mut owner, bo, _) = one(d, 512 << 20, StartAs::Cold, Until::Fw, false, Hostile::None, &opts, &[(pspfake::FB_MC + GOP, 2 << 20)])?;
    let f = owner.hw().hw();
    kassert!(!out.passed() && matches!(out.error, Some(PspError::Mem { .. })) && out.violations_total >= 1, "{:?}", out.error);
    kassert!(out.violations.iter().all(|v| v.blocked) && f.scanout_writes.is_empty() && f.ring.is_none() && bo.count() == 0);
    Ok(())
}

fn modes() -> ktest::TestResult {
    check_modes(&fakechip::discovery_780m())
}
ktest::ktest!("gx.bringup.modes", Safe, modes);

/// Every failure answer of the fake through the engine: its own error, FAIL, nothing left on the PSP.
pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    let opts = ListOpts::PSP13_0_4;
    for (h, ok) in crate::ktests_psp::hostile_cases() {
        let (out, mut owner, mut bo, _) = one(d, 512 << 20, StartAs::Cold, Until::Fw, false, h, &opts, &[])?;
        let f = owner.hw().hw();
        kassert!(!out.passed() && out.error.as_ref().is_some_and(ok), "{h:?}: {:?}", out.error);
        kassert!(f.ring.is_none() && f.tmr.is_none(), "{h:?}: left ring {:?} tmr {:?}", f.ring, f.tmr);
        kassert!(f.now <= 3_000_000, "{h:?}: waited {} us", f.now);
        // A ring create without an answer or a lost fence: the PSP may still hold the buffers (fixer).
        let parks = matches!(h, Hostile::Silent | Hostile::NoFence | Hostile::WrongFence);
        check_parked(&out, &mut bo, parks).map_err(|e| format!("{h:?}: {e}"))?;
    }
    // The undo's own failures (cold start, M2 passed, then the closing DESTROY_TMR or DESTROY_RINGS fails) and the
    // stale path's (after Linux: the precautionary DESTROY_TMR answers a status, DESTROY_RINGS never answers).
    let destroy_tmr = Hostile::CmdStatus { id: cmd::DESTROY_TMR, status: 0x11 };
    for (s, h) in [(StartAs::Cold, destroy_tmr), (StartAs::Cold, Hostile::StopSilent), (StartAs::AfterLinux, destroy_tmr), (StartAs::AfterLinux, Hostile::StopSilent)] {
        let (out, mut owner, mut bo, _) = one(d, 512 << 20, s, Until::Fw, false, h, &opts, &[])?;
        kassert!(!out.passed(), "{s:?} {h:?}: passed");
        // Only the stale DESTROY_TMR refusal ends in a plain refusal (SETUP_TMR's status, undone): rerunnable.
        let soft = s == StartAs::AfterLinux && h == destroy_tmr;
        kassert!(out.needs_reboot() != soft, "{s:?} {h:?}: reboot {} err {:?} undo {:?}", out.needs_reboot(), out.error, out.undo);
        let f = owner.hw().hw();
        match (s, h) {
            (StartAs::AfterLinux, Hostile::StopSilent) => {
                // The old ring never went: nothing of ours was created, nothing sent, the buffers go back.
                kassert!(matches!(out.error, Some(PspError::Timeout { what: "ring.stop", .. })) && f.cmds.is_empty(), "{:?} {:?}", out.error, f.cmds);
                kassert!(out.undo.is_none() && !out.report.steps.iter().any(|x| matches!(x, Step::RingCreated { .. })));
                check_parked(&out, &mut bo, false)?;
            }
            (_, Hostile::StopSilent) => {
                kassert!(out.error.is_none() && matches!(out.undo, Some(Err(PspError::Timeout { what: "ring.stop", .. }))), "{:?}", out.undo);
                check_parked(&out, &mut bo, true)?;
            }
            (StartAs::AfterLinux, _) => {
                // The precautionary DESTROY_TMR's status is tolerated and shown; the old TMR then stays, and
                // SETUP_TMR's status is the proof (the fake answers its assumed "TMR exists"): fatal, undone in
                // the start (no TMR of ours, so no DESTROY_TMR), the ring stopped, every buffer back.
                kassert!(out.report.warnings == [(cmd::DESTROY_TMR, 0x11)], "{:?}", out.report.warnings);
                kassert!(out.report.steps.contains(&Step::StaleTmrDestroyed { status: 0x11 }));
                kassert!(matches!(out.error, Some(PspError::CmdStatus { cmd: cmd::SETUP_TMR, .. })) && out.undo.is_none(), "{:?}", out.error);
                kassert!(f.ring.is_none() && f.loaded.is_empty());
                check_parked(&out, &mut bo, false)?;
            }
            (_, _) => {
                // Cold: the run passed, the closing DESTROY_TMR fails: the TMR may still be the PSP's.
                kassert!(out.error.is_none() && f.loaded.len() == pspfake::EXPECT_780M.len(), "{s:?}: {:?}", out.error);
                kassert!(!out.report.steps.iter().any(|x| matches!(x, Step::StaleTmrDestroyed { .. })));
                kassert!(matches!(out.undo, Some(Err(PspError::CmdStatus { cmd: cmd::DESTROY_TMR, status: 0x11, .. }))), "{:?}", out.undo);
                kassert!(f.ring.is_none(), "the ring is stopped even when DESTROY_TMR fails");
                check_parked(&out, &mut bo, true)?;
            }
        }
    }
    Ok(())
}

/// Whether the answer says the undo is incomplete (`render` needs `Inputs` only for the header and the names).
fn says_incomplete(out: &Outcome) -> bool {
    let fw = crate::gpucore::ucode::FwSet::default();
    let inp = Inputs {
        addrs: FakeGpu::new(Hostile::None).a,
        carve: Carve { mc_base: 0, phys_base: 0 },
        start: Start::Cold,
        until: Until::Fw,
        fw: &fw,
        list: &[],
        autoload: false,
        keep: false,
        t: Timeouts::LINUX,
        gfx: None,
        gmc: None,
        sdma: None,
        gfx_ring: None,
        mes: None,
        vm: None,
        unload_first: false,
    };
    pspbringup::render(out, &inp).iter().any(|l| l.starts_with("undo: INCOMPLETE"))
}

/// Parked (quarantined): every buffer still allocated, and the manager hands out nothing over it (it is filled
/// up, pieces then pages, and no new BO meets a parked buffer); else every buffer given back.
pub(crate) fn check_parked(out: &Outcome, bo: &mut BoManager, parks: bool) -> Result<(), String> {
    match (&out.quarantined, parks) {
        (None, false) => {
            kassert!(bo.count() == 0, "buffers kept: {}", bo.count());
        }
        (Some(p), true) => {
            kassert!(out.needs_reboot() && !out.passed() && says_incomplete(out));
            kassert!(bo.count() == p.buffers().count(), "{} BOs for {} buffers", bo.count(), p.buffers().count());
            for b in p.buffers() {
                kassert!(bo.get(b.handle).is_some_and(|x| x.range.start == b.offset), "buffer at {:#x} given back", b.offset);
            }
            for unit in [pspbringup::PIECE, crate::bo::PAGE] {
                while let Ok(h) = bo.create(unit, crate::bo::Placement::Vram, unit, None) {
                    let r = bo.get(h).map(|x| x.range.clone()).ok_or("no BO")?;
                    kassert!(p.buffers().all(|b| r.end <= b.offset || b.offset + b.size <= r.start), "new BO {r:x?} over a parked buffer");
                }
            }
        }
        (q, _) => return Err(format!("quarantined {} but expected {parks}", q.is_some())),
    }
    Ok(())
}

fn failures() -> ktest::TestResult {
    check_failures(&fakechip::discovery_780m())
}
ktest::ktest!("gx.bringup.failures", Safe, failures);

/// GX-7e: the driver messages PMFW took, in order.
fn drv_ids(f: &FakeGpu) -> Vec<u32> {
    f.smu.taken.iter().filter(|t| t.0 == smufake::Client::Driver).map(|t| t.1).collect()
}

/// GX-7e invariants of every GC stage, passed or not: only allowed messages, never AllowGfxOff, never two
/// messages in flight, no GC register written while GC was off nor read before EnableGfxImu while off (GX-7f:
/// the autoload poll reads a powering GC as Linux does), nothing to the display, bounded time.
fn gfx_clean(f: &FakeGpu, what: &str) -> ktest::TestResult {
    let ids = drv_ids(f);
    kassert!(f.smu.taken.iter().all(|t| t.0 == smufake::Client::Driver && smu::allowed(t.1, t.2)), "{what}: {:?}", f.smu.taken);
    kassert!(!ids.contains(&smufake::msg::ALLOW_GFXOFF) && !f.gc.gfxoff_allowed, "{what}: GFXOFF allowed");
    kassert!(f.smu.overlaps.is_empty(), "{what}: a message over one in flight {:?}", f.smu.overlaps);
    kassert!(f.gc.off_access.is_empty(), "{what}: GC touched while off {:x?}", f.gc.off_access);
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "{what}: display touched {:x?}", f.display_writes);
    kassert!(f.now <= 12_000_000, "{what}: waited {} us", f.now);
    Ok(())
}

/// GX-7e: M3 on every start state and three sizes: the messages in the capture's order, EnableGfxImu not waited
/// for, GC on and booted, GFXOFF never allowed, the undo leaves GC on; the SMU writes in Linux's order.
pub(crate) fn check_gfx(d: &Discovery) -> ktest::TestResult {
    use smufake::msg;
    let opts = ListOpts::PSP13_0_4;
    for size in [512u64 << 20, 8 << 30, 16 << 30] {
        for &s in STARTS {
            let (out, mut owner, bo, start) = one(d, size, s, Until::Gfx, false, Hostile::None, &opts, &[])?;
            let f = owner.hw().hw();
            let what = format!("{s:?} at {size:#x}");
            gfx_clean(f, &what)?;
            match s {
                StartAs::PspDown => {
                    kassert!(!out.passed() && out.gfx.is_none() && f.writes.is_empty(), "{what}");
                    continue;
                }
                StartAs::AfterCrash(Crash::SmuUnanswered) => {
                    // The stale AllowGfxOff never answers. GX-7j3: seen before the PSP is touched (smu::settle):
                    // nothing loaded, nothing reaches PMFW, a reboot.
                    kassert!(out.gfx.is_none() && matches!(out.pmfw, Some(Ok(smu::MailboxState::Stuck { msg: msg::ALLOW_GFXOFF, .. }))), "{what}: {:?}", out.pmfw);
                    kassert!(!out.gc_failed() && out.needs_reboot() && out.pmfw_stuck() && drv_ids(f).is_empty() && f.cmds.is_empty(), "{what}: cmds {:x?}", f.cmds);
                    kassert!(f.writes.iter().all(|&(b, _)| b != f.smu.drv.msg_at), "{what}: a message written");
                    kassert!(f.ring.is_none() && f.tmr.is_none(), "{what}: PSP touched");
                    continue;
                }
                _ => {}
            }
            kassert!(out.passed(), "{what}: {:?} {:?} {:?}", out.error, out.gfx.as_ref().map(|g| g.error), out.checks.iter().filter(|c| !c.pass).collect::<Vec<_>>());
            let g = out.gfx.as_ref().ok_or("no gfx")?;
            // GX-7j3: GC left booted by a BitsOS stop before GX-7j3 (no unload in its note): caught up first.
            let catch_up = pspbringup::unload_first(start, false);
            let mut want = alloc::vec![msg::GET_DRIVER_IF_VERSION, msg::GET_PMFW_VERSION, msg::ENABLE_GFX_IMU];
            if catch_up {
                want.insert(0, msg::PREPARE_MP1_FOR_UNLOAD);
            }
            kassert!(out.catch_up.is_some() == catch_up && out.catch_up.is_none_or(|u| u.told() && u.rlc.is_some_and(|r| r.1) == (start == Start::AfterBitsOs)), "{what}: catch-up {:?}", out.catch_up);
            kassert!(!f.smu.wedged, "{what}: PMFW wedged (a load under a running IMU without an unload)");
            if start != Start::Cold {
                want.push(msg::DISALLOW_GFXOFF);
            }
            kassert!(drv_ids(f) == want, "{what}: {:x?}", drv_ids(f));
            kassert!(f.smu.taken.iter().find(|t| t.1 == msg::ENABLE_GFX_IMU).is_some_and(|t| t.2 == 1), "{what}: EnableGfxImu argument");
            kassert!(f.gc.booted && f.gc.power == gcfake::Power::On && !f.gc.gfxoff_allowed, "{what}: GC {:?} booted {}", f.gc.power, f.gc.booted);
            // GX-7f: EnableGfxImu reached PMFW: no undo, TMR and ring stay with the booted GC.
            kassert!(out.held_for_gc && out.undo.is_none() && out.kept.is_some() && f.ring.is_some() && f.tmr.is_some(), "{what}: undo {:?}", out.undo);
            kassert!(g.polls > 0 && g.flags_seen.first().is_some_and(|x| x.1 == gcfake::IMU_FLAGS_LATER || x.1 == gcfake::IMU_FLAGS_COLD || x.1 == gcfake::IMU_FLAGS_POWERED), "{what}: flags {:x?}", g.flags_seen);
            kassert!(g.booted_before() == matches!(s, StartAs::AfterLinux | StartAs::AfterBitsOs), "{what}: prior {:?}", g.prior);
            kassert!(g.answered_us.is_some_and(|a| a >= smufake::latency_us(msg::ENABLE_GFX_IMU)), "{what}: answered {:?}", g.answered_us);
            // The SMU part of the log: the SMN index once, then per message answer 0, argument, message.
            let smu_w: Vec<(u32, u32)> = out.entries.iter().map(|e| (e.byte, e.value)).filter(|&(b, _)| f.smu.owns(b) || b == f.smn_window.0).collect();
            let m = f.smu.drv;
            let mut expect = Vec::new();
            let mut ids = want.iter().peekable();
            if catch_up && let Some(&id) = ids.next() {
                expect.extend([(m.resp_at, 0), (m.arg_at, 0), (m.msg_at, id)]);
            }
            expect.push((f.smn_window.0, smufake::SMN_FIRMWARE_FLAGS));
            for (&id, arg) in ids.zip([0, 0, 1, 0]) {
                expect.extend([(m.resp_at, 0), (m.arg_at, arg), (m.msg_at, id)]);
            }
            kassert!(smu_w == expect, "{what}: SMU writes {smu_w:x?}");
            // The catch-up's RLC stop is the only GC register written.
            kassert!(out.entries.iter().all(|e| !f.gc.in_gc(e.byte) || (start == Start::AfterBitsOs && e.byte == f.gc.rlc_cntl)), "{what}: a GC register written");
            let fw0 = crate::gpucore::ucode::FwSet::default();
            let text = pspbringup::render(&out, &dummy_inputs(&fw0));
            let name = if g.booted_before() { crate::gpucore::gfxup::BOOT_NOT_PROVEN } else { "M3 RLC bootload complete" };
            kassert!(text.iter().any(|l| l.starts_with(&format!("{name}: PASS"))), "{what}: no M3 line");
            kassert!(text.iter().any(|l| l.starts_with("M3 not proven:")) == g.booted_before(), "{what}: not-proven line");
            kassert!(text.iter().any(|l| l.starts_with("undo: none: EnableGfxImu reached PMFW, so TMR and ring stay")), "{what}: undo line");
            // The Quiesce hook's stop (GC booted): TMR and ring destroyed, GC stays on and booted.
            let mut out = out;
            let mut bo = bo;
            let mut p = out.kept.take().ok_or("not kept")?;
            p.t = Timeouts::QUIESCE;
            p.stop(owner.hw()).map_err(|e| format!("{what}: quiesce stop: {e:?}"))?;
            p.free(&mut bo);
            let f = owner.hw().hw();
            kassert!(f.ring.is_none() && f.tmr.is_none() && bo.count() == 0 && f.gc.booted && f.gc.power == gcfake::Power::On, "{what}: after the stop");
        }
    }
    // After Linux with GFXOFF allowed and GC gated (Linux's late init allows it): DisallowGfxOff, GC on, booted.
    let (out, mut owner, ..) = one_with(d, 512 << 20, StartAs::AfterLinux, Until::Gfx, false, Hostile::None, &opts, &[], GOP, &|f| {
        f.gc.gfxoff_allowed = true;
        f.gc.power = gcfake::Power::Off;
    })?;
    let f = owner.hw().hw();
    kassert!(out.passed() && drv_ids(f).last() == Some(&msg::DISALLOW_GFXOFF) && !f.gc.gfxoff_allowed && f.gc.power == gcfake::Power::On, "gated: {:?}", out.gfx.as_ref().map(|g| g.error));
    gfx_clean(f, "gated")?;
    // Kept: the PSP stays for the next item; the Quiesce stop destroys TMR and ring, GC stays on.
    let (mut out, mut owner, mut bo, _) = one(d, 8 << 30, StartAs::Cold, Until::Gfx, true, Hostile::None, &opts, &[])?;
    let mut p = out.kept.take().ok_or("not kept")?;
    p.t = Timeouts::QUIESCE;
    p.stop(owner.hw()).map_err(|e| format!("quiesce stop: {e:?}"))?;
    let f = owner.hw().hw();
    kassert!(f.ring.is_none() && f.tmr.is_none() && f.gc.booted && f.gc.power == gcfake::Power::On);
    p.free(&mut bo);
    Ok(())
}

pub(crate) fn dummy_inputs(fw: &crate::gpucore::ucode::FwSet) -> Inputs<'_> {
    Inputs {
        addrs: FakeGpu::new(Hostile::None).a,
        carve: Carve { mc_base: 0, phys_base: 0 },
        start: Start::Cold,
        until: Until::Gfx,
        fw,
        list: &[],
        autoload: false,
        keep: false,
        t: Timeouts::LINUX,
        gfx: None,
        gmc: None,
        sdma: None,
        gfx_ring: None,
        mes: None,
        vm: None,
        unload_first: false,
    }
}

fn gfx_starts() -> ktest::TestResult {
    check_gfx(&fakechip::discovery_780m())
}
ktest::ktest!("gx.bringup.gfx", Safe, gfx_starts);

/// One hostile PMFW or GC: the setup, the error it must give (`None`: passes), whether GC was touched.
pub(crate) type GfxCase = (&'static str, fn(&mut FakeGpu), Option<fn(&GfxError) -> bool>, bool);

pub(crate) fn gfx_cases() -> Vec<GfxCase> {
    use smufake::{SmuHostile as H, msg, resp};
    fn add(f: &mut FakeGpu, h: H) {
        f.smu.hostile.push(h);
    }
    alloc::vec![
        ("PMFW dead", |f| add(f, H::Dead), Some(|e| matches!(e, GfxError::Smu(SmuError::NotRunning { flags: 0 }))), false),
        ("interface 9", |f| add(f, H::Value { id: msg::GET_DRIVER_IF_VERSION, value: 9 }), Some(|e| *e == GfxError::Interface { got: 9 }), false),
        ("GetDriverIfVersion failed", |f| add(f, H::Answer { id: msg::GET_DRIVER_IF_VERSION, code: resp::FAILED }), Some(|e| *e == GfxError::Smu(SmuError::Answer { id: 3, code: 0xFF })), false),
        ("GetDriverIfVersion never", |f| add(f, H::Never { id: msg::GET_DRIVER_IF_VERSION }), Some(|e| *e == GfxError::Smu(SmuError::Timeout { id: 3 })), false),
        ("GetPmfwVersion late 3 s", |f| add(f, H::Late { id: msg::GET_PMFW_VERSION, us: 3_000_000 }), Some(|e| *e == GfxError::Smu(SmuError::Timeout { id: 2 })), false),
        ("GetPmfwVersion unknown", |f| add(f, H::Answer { id: msg::GET_PMFW_VERSION, code: resp::UNKNOWN }), Some(|e| *e == GfxError::Smu(SmuError::Answer { id: 2, code: 0xFE })), false),
        ("EnableGfxImu failed", |f| add(f, H::Answer { id: msg::ENABLE_GFX_IMU, code: resp::FAILED }), Some(|e| *e == GfxError::Smu(SmuError::Answer { id: 0x16, code: 0xFF })), true),
        ("EnableGfxImu prerequisites", |f| add(f, H::Answer { id: msg::ENABLE_GFX_IMU, code: resp::PREREQ }), Some(|e| *e == GfxError::Smu(SmuError::Answer { id: 0x16, code: 0xFD })), true),
        ("EnableGfxImu busy", |f| add(f, H::Answer { id: msg::ENABLE_GFX_IMU, code: resp::BUSY }), Some(|e| *e == GfxError::Smu(SmuError::Answer { id: 0x16, code: 0xFC })), true),
        ("EnableGfxImu never", |f| add(f, H::Never { id: msg::ENABLE_GFX_IMU }), Some(|e| *e == GfxError::Smu(SmuError::PrePoll { last: 0x16 })), true),
        ("EnableGfxImu late 2.5 s", |f| add(f, H::Late { id: msg::ENABLE_GFX_IMU, us: 2_500_000 }), Some(|e| *e == GfxError::Smu(SmuError::PrePoll { last: 0x16 })), true),
        ("EnableGfxImu late 1.5 s", |f| add(f, H::Late { id: msg::ENABLE_GFX_IMU, us: 1_500_000 }), None, true),
        ("GC stuck off", |f| f.gc.hostile = gcfake::GcHostile::StuckOff, Some(|e| matches!(e, GfxError::BootTimeout { .. })), true),
        ("GC wakes after 3 s", |f| f.gc.wake_us = 3_000_000, Some(|e| matches!(e, GfxError::BootTimeout { .. })), true),
        ("IMU flags stuck at 0x80000001 (28.10.)", |f| f.gc.hostile = gcfake::GcHostile::Flags(gcfake::IMU_FLAGS_COLD), None, true),
        ("IMU flags bits 2:1 == 2 always", |f| f.gc.hostile = gcfake::GcHostile::Flags(0x4), None, true),
        ("IMU flags 0", |f| f.gc.hostile = gcfake::GcHostile::Flags(0), None, true),
        ("IMU flags all ones", |f| f.gc.hostile = gcfake::GcHostile::Flags(u32::MAX), Some(|e| matches!(e, GfxError::Gone { .. })), false),
        ("device gone after EnableGfxImu", |f| f.gc.hostile = gcfake::GcHostile::GoneAfterImu, Some(|e| matches!(e, GfxError::Gone { .. })), true),
        ("autoload never", |f| f.gc.hostile = gcfake::GcHostile::AutoloadNever, Some(|e| matches!(e, GfxError::BootTimeout { .. })), true),
        ("CP never idle", |f| f.gc.hostile = gcfake::GcHostile::CpBusy, Some(|e| matches!(e, GfxError::BootTimeout { .. })), true),
        ("boot 250 ms after the send", |f| f.gc.boot_us = 250_000, None, true),
        ("PMFW fresh: answer and message 0", |f| f.smu.drv.resp = resp::NONE, None, true),
        ("a message in flight: answered late", |f| {
            f.smu.stale_message(0x12, 0);
            f.smu.drv.flight = Some((800_000, 0x12, 0));
        }, None, true),
        ("boot 400 ms after the send", |f| f.gc.boot_us = 400_000, Some(|e| matches!(e, GfxError::BootTimeout { .. })), true),
    ]
}

/// GX-7e: every timeout and wrong answer of PMFW and GC: its own error, FAIL, nothing sent past it; the PSP
/// undone before EnableGfxImu, held (TMR and ring up, no undo) once it reached PMFW (GX-7f); a reboot when PMFW
/// may still work on our message or GC was touched.
pub(crate) fn check_gfx_failures(d: &Discovery) -> ktest::TestResult {
    let opts = ListOpts::PSP13_0_4;
    for (name, setup, want, touched) in gfx_cases() {
        let (out, mut owner, bo, _) = one_with(d, 512 << 20, StartAs::Cold, Until::Gfx, false, Hostile::None, &opts, &[], GOP, &|f| setup(f))?;
        let f = owner.hw().hw();
        gfx_clean(f, name)?;
        let g = out.gfx.as_ref().ok_or("no gfx")?;
        match want {
            None => kassert!(out.passed() && g.error.is_none() && !pspbringup::gc_latch(true, &out), "{name}: {:?}", g.error),
            Some(ok) => {
                // A failure that never reached GC keeps an earlier failure's latch; one that did sets it.
                kassert!(pspbringup::gc_latch(true, &out) && pspbringup::gc_latch(false, &out) == touched, "{name}: latch");
                let fw0 = crate::gpucore::ucode::FwSet::default();
                let undo_line = pspbringup::render(&out, &dummy_inputs(&fw0)).iter().any(|l| l.starts_with("GC: NOT booted; EnableGfxImu reached PMFW"));
                kassert!(undo_line == touched, "{name}: undo line {undo_line}");
                kassert!(!out.passed() && g.error.as_ref().is_some_and(ok), "{name}: {:?}", g.error);
                kassert!(g.gc_touched == touched && out.gc_failed() == touched, "{name}: touched {}", g.gc_touched);
                kassert!(out.needs_reboot() == (touched || g.error.is_some_and(|e| e.needs_reboot())), "{name}: reboot {}", out.needs_reboot());
                kassert!(drv_ids(f).contains(&smufake::msg::ENABLE_GFX_IMU) == touched, "{name}: {:x?}", drv_ids(f));
            }
        }
        if g.gc_touched {
            kassert!(out.held_for_gc && out.undo.is_none() && out.kept.is_some() && f.ring.is_some() && f.tmr.is_some(), "{name}: not held");
        } else {
            kassert!(f.ring.is_none() && f.tmr.is_none() && bo.count() == 0, "{name}: PSP left");
        }
    }
    Ok(())
}

/// GX-7e: the client refuses every message off its list before writing anything; mode 2 only with RESET_MODE_2.
pub(crate) fn check_forbidden() -> ktest::TestResult {
    use smufake::msg;
    let mut f = FakeGpu::new(Hostile::None);
    let a = crate::ipmod::smu13::Addrs::from_discovery(&fakechip::discovery_780m()).ok_or("no SMU addresses")?;
    let mut c = smu::Smu::new(a, smu::SmuTimeouts::LINUX);
    for (id, arg) in [(msg::ALLOW_GFXOFF, 0), (msg::ALLOW_ZSTATES, 1), (msg::POWER_UP_VCN, 0), (msg::SET_DRIVER_DRAM_ADDR_LOW, 0), (msg::GFX_DEVICE_DRIVER_RESET, 1), (msg::POWER_DOWN_VCN, 0), (msg::POWER_DOWN_JPEG, 0), (0x09, 400), (0x20, 50)] {
        kassert!(c.send(&mut f, id, arg, true) == Err(SmuError::Forbidden { id, arg }), "{id:#x}");
    }
    kassert!(f.writes.is_empty() && f.smu.taken.is_empty(), "{:x?}", f.writes);
    kassert!(c.send(&mut f, msg::GET_PMFW_VERSION, 0, true) == Ok(smufake::PMFW_VERSION));
    Ok(())
}

/// GX-7f: mode 2. A GC stage that never boots leaves TMR and ring (held) and needs a reboot. GfxDeviceDriverReset
/// as the fake now models 13.0.4 (the Mini-PC 28.10.: never answered): `Smu(Timeout)` after the SMU wait, a
/// reboot, no GC read after it, bounded time. With a PMFW that answers, GC is reset (the fake's model) and a
/// second stage after `until=stop` boots GC again. The live tool refuses `until=reset` before writing anything.
pub(crate) fn check_recovery(d: &Discovery) -> ktest::TestResult {
    let opts = ListOpts::PSP13_0_4;
    let (mut out, mut owner, mut bo, _) = one_with(d, 512 << 20, StartAs::Cold, Until::Gfx, false, Hostile::None, &opts, &[], GOP, &|f| f.gc.hostile = gcfake::GcHostile::AutoloadNever)?;
    kassert!(out.gc_failed() && out.needs_reboot() && out.held_for_gc, "{:?}", out.gfx.as_ref().map(|g| g.error));
    let gi = pspbringup::GfxInputs::from_discovery(d).ok_or("no gfx inputs")?;
    let t0 = owner.hw().hw().now;
    let r = gfxup::mode2(&mut *owner, gi.smu, &gi.gc, &gi.t);
    let f = owner.hw().hw();
    kassert!(r.error == Some(GfxError::Smu(SmuError::Timeout { id: smufake::msg::GFX_DEVICE_DRIVER_RESET })) && r.error.is_some_and(|e| e.needs_reboot()), "{:?}", r.error);
    kassert!(!r.steps.iter().any(|s| matches!(s, gfxup::GfxStep::Mode2 { .. })) && f.now - t0 <= gi.t.smu.answer_us + gi.t.smu.pre_poll_us + 1_000, "{:?} {} us", r.steps, f.now - t0);
    let pre = crate::psplive::Pre { reboot_needed: false, quarantined: false, display: Ok(()), has_bos: true, until: Until::Gfx, m2_runs: 0, again: true, kept: false, gfx_runs: 1, gc_failed: true, reset: true, gmc_runs: 0, kept_gc_booted: false, sdma_runs: 0, gart_up: false, gfx_ring_runs: 0, mes_runs: 0, mes_up: false, mes_stranded: false, mes_queues: false };
    kassert!(crate::psplive::preflight(&pre).is_err_and(|e| e.starts_with(gfxup::MODE2_REFUSED)), "mode 2 not refused");
    // A PMFW that answers (not 13.0.4 as measured): GC reset; the held PSP stopped (until=stop), then again.
    let f = owner.hw().hw();
    f.smu.drv.flight = None;
    f.smu.drv.resp = smufake::resp::OK;
    f.smu.mode2_answers = true;
    let r = gfxup::mode2(&mut *owner, gi.smu, &gi.gc, &gi.t);
    kassert!(r.error.is_none() && r.sent.len() == 1 && r.sent[0].id == smufake::msg::GFX_DEVICE_DRIVER_RESET && r.sent[0].arg == crate::ipmod::smu13::RESET_MODE_2, "{:?}", r);
    let f = owner.hw().hw();
    kassert!(f.gc.resets == 1 && !f.gc.booted && f.display_writes.is_empty() && f.gc.off_access.is_empty());
    let mut p = out.kept.take().ok_or("not held")?;
    p.stop(&mut *owner).map_err(|e| format!("stop: {e:?}"))?;
    p.free(&mut bo);
    let f = owner.hw().hw();
    f.gc.hostile = gcfake::GcHostile::None;
    let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).map_err(|e| format!("{e:?}"))?;
    let list = fw.load_list(&opts).map_err(|e| format!("{e:?}"))?;
    let inp = Inputs { addrs: f.a, carve: Carve { mc_base: f.fb_mc, phys_base: f.fb_phys }, start: Start::AfterBitsOsNoGc, until: Until::Gfx, fw: &fw, list: &list, autoload: true, keep: false, t: Timeouts::LINUX, gfx: Some(gi), gmc: None, sdma: None, gfx_ring: None, mes: None, vm: None, unload_first: false };
    let out = pspbringup::run(&mut owner, &mut bo, &inp);
    let f = owner.hw().hw();
    kassert!(out.passed() && f.gc.booted && !out.gfx.as_ref().is_some_and(|g| g.booted_before()), "{:?} {:?}", out.error, out.gfx.as_ref().map(|g| g.error));
    kassert!(!pspbringup::gc_latch(true, &out), "a booted GC clears the latch");
    gfx_clean(f, "recovery")?;
    Ok(())
}

fn gfx_failures() -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    check_gfx_failures(&d)?;
    check_forbidden()?;
    check_recovery(&d)
}
ktest::ktest!("gx.bringup.gfx_failures", Safe, gfx_failures);

/// GX-7f: the stand-in for kexec's driver-note slot (CR-1): what `put` stored is what the next kernel's `take` gives.
static SLOT: ksync::SpinLock<Option<Vec<u8>>> = ksync::SpinLock::named("amdgpu.kt_note_slot", None);

fn slot_put(tag: u32, b: &[u8]) -> bool {
    tag == handoff::TAG && SLOT.try_lock().map(|mut g| *g = Some(b.to_vec())).is_some()
}
fn slot_take(tag: u32) -> Option<Vec<u8>> {
    if tag != handoff::TAG {
        return None;
    }
    SLOT.try_lock().and_then(|g| g.clone())
}
/// A Reload that damaged the slot (one bit flipped).
fn slot_take_damaged(tag: u32) -> Option<Vec<u8>> {
    slot_take(tag).map(|mut b| {
        if let Some(x) = b.get_mut(13) {
            *x ^= 1;
        }
        b
    })
}
const IO: handoff::NoteIo = handoff::NoteIo { put: slot_put, take: slot_take, from_reload: || true };

/// The next kernel's classifier on the fake as the Quiesce hook left it, with the note `io` hands on (`None`: no
/// NoteIo bound, nothing crosses the Reload).
fn next_kernel(d: &Discovery, f: &mut FakeGpu, io: Option<handoff::NoteIo>) -> classify::State {
    let n = io.and_then(|io| io.take_note());
    classify::classify_with(d, &mut |b| f.reg_read(b), n.as_ref())
}

/// GX-7f: a Reload after a clean stop classifies as clean, end to end: the run (M3 kept, M1 kept), the Quiesce
/// hook's own stop and note (`handoff::quiesce_kept`), the note through the slot, the next kernel's classifier on the
/// registers the stop left (C2PMSG_67 keeps 0x10). Damaged bytes or no NoteIo: a crash, as on 28.10. A chain of
/// Reloads without a run hands the note on unchanged (`idle_hand_on`), its bus master flag kept unless taken over. A
/// GC that did not boot: ring and TMR left up, the note says unsettled, the next kernel refuses until a reboot.
pub(crate) fn check_reload_note(d: &Discovery) -> ktest::TestResult {
    use handoff::{GcFate, RingFate};
    let opts = ListOpts::PSP13_0_4;
    // GX-7j3: the Quiesce unloads a booted GC toward PMFW first (PrepareMp1ForUnload, the RLC stopped): the note says Unloaded.
    let un = pspbringup::GfxInputs::from_discovery(d).map(|g| handoff::UnloadAddrs { smu: g.smu, gc: g.gc }).ok_or("no SMU/GC addresses")?;
    for (until, gc, want_gc, want) in [(Until::Gfx, GcFate::Booted, GcFate::Unloaded, Start::AfterBitsOs), (Until::Ring, GcFate::Untouched, GcFate::Untouched, Start::AfterBitsOsNoGc)] {
        let what = format!("{until:?}");
        let (mut out, mut owner, mut bo, _) = one(d, 8 << 30, StartAs::Cold, until, true, Hostile::None, &opts, &[])?;
        let mut p = out.kept.take().ok_or("not kept")?;
        let a = owner.hw().hw().a;
        let (r, n) = handoff::quiesce_kept(&mut p, owner.hw(), &a, gc, Some(&un), true);
        kassert!(r == Some(true) && n.ring == RingFate::Destroyed && n.tmr_destroyed && n.gc == want_gc && n.bus_master_left_on, "{what}: {r:?} {n:?}");
        let f = owner.hw().hw();
        let unloads = f.smu.taken.iter().filter(|t| t.1 == smufake::msg::PREPARE_MP1_FOR_UNLOAD).count();
        kassert!(unloads == usize::from(gc == GcFate::Booted) && f.gc.rlc_stopped() == (gc == GcFate::Booted), "{what}: {unloads} unloads, RLC stopped {}", f.gc.rlc_stopped());
        p.free(&mut bo);
        // The Quiesce hook hands on the note with bus master as it left it (switched off: quiet).
        let n = handoff::StopNote { bus_master_left_on: false, ..n };
        kassert!(IO.put_note(&n), "{what}: put");
        let f = owner.hw().hw();
        kassert!(f.ring.is_none() && f.tmr.is_none(), "{what}: PSP not quiet");
        kassert!(n.wptr != 0, "{what}: the fake should keep the write pointer like the 780M (C2PMSG_67 {:#x})", n.wptr);
        let st = next_kernel(d, f, Some(IO));
        kassert!(st.start == want, "{what}: {:?} {:?}", st.start, st.why);
        kassert!(st.why.iter().any(|w| w.contains("destroyed cleanly by the last stop")) && !st.why.iter().any(|w| w.contains("not known destroyed")), "{what}: {:?}", st.why);
        kassert!(pspbringup::admit(st.start).is_ok(), "{what}: not admitted");
        // A chain: kernel B never runs gx_bringup; its Quiesce hands the inherited note on unchanged.
        let inherited = IO.take_note();
        kassert!(inherited == Some(n), "{what}: take");
        let b = handoff::idle_hand_on(None, inherited, false, false).ok_or("chain: nothing handed on")?;
        kassert!(b == n && IO.put_note(&b), "{what}: chain");
        kassert!(next_kernel(d, f, Some(IO)).start == want, "{what}: after the chain");
        // Damaged bytes, or no NoteIo bound: the registers alone look like a crash (what the desk saw 28.10.).
        for (io, why) in [(Some(handoff::NoteIo { put: slot_put, take: slot_take_damaged, from_reload: || true }), "damaged"), (None, "unbound")] {
            let st = next_kernel(d, f, io);
            kassert!(st.start == Start::AfterCrash && st.why.iter().any(|w| w.contains("not known destroyed")), "{what} {why}: {:?} {:?}", st.start, st.why);
        }
    }
    // Bus master across a chain: the kernel before left it on; a boot that did not take it over keeps the flag, one
    // that did (and switched it off) clears it; this boot's own note always says what this boot left.
    let on = handoff::StopNote { ring: RingFate::Destroyed, gc: GcFate::Booted, tmr_destroyed: true, bus_master_left_on: true, wptr: 0x10, size: 0x1000, addr_lo: 1, addr_hi: 2 };
    kassert!(handoff::idle_hand_on(None, Some(on), false, false).is_some_and(|n| n.bus_master_left_on), "not adopted: flag lost");
    kassert!(handoff::idle_hand_on(None, Some(on), true, false).is_some_and(|n| !n.bus_master_left_on), "adopted and off");
    let own = handoff::StopNote { gc: GcFate::Untouched, ..on };
    kassert!(handoff::idle_hand_on(Some(own), Some(on), false, false) == Some(handoff::StopNote { bus_master_left_on: false, ..own }), "own note first");
    kassert!(handoff::idle_hand_on(None, None, false, true).is_none(), "nothing to hand on");
    // GC did not boot after EnableGfxImu: the run holds the PSP; the Quiesce leaves TMR and ring; the next kernel refuses.
    let (mut out, mut owner, ..) = one_with(d, 512 << 20, StartAs::Cold, Until::Gfx, false, Hostile::None, &opts, &[], GOP, &|f| f.gc.hostile = gcfake::GcHostile::StuckOff)?;
    kassert!(out.held_for_gc && out.gc_failed(), "stuck off: not held");
    let mut p = out.kept.take().ok_or("stuck off: not held in kept")?;
    let a = owner.hw().hw().a;
    let (r, n) = handoff::quiesce_kept(&mut p, owner.hw(), &a, GcFate::Unsettled, Some(&un), true);
    let f = owner.hw().hw();
    kassert!(r.is_none() && f.ring.is_some() && f.tmr.is_some() && n.gc == GcFate::Unsettled && n.ring == RingFate::NotDestroyed, "stuck off: {r:?} {n:?}");
    kassert!(IO.put_note(&n), "stuck off: put");
    let st = next_kernel(d, f, Some(IO));
    kassert!(st.start == Start::GcUnsettled && pspbringup::admit(st.start).is_err_and(|w| w.contains("reboot")), "stuck off: {:?} {:?}", st.start, st.why);
    Ok(())
}

fn reload_note() -> ktest::TestResult {
    check_reload_note(&fakechip::discovery_780m())
}
ktest::ktest!("gx.bringup.reload_note", Safe, reload_note);

/// `gx_bringup` is GX's, safety Change; bad arguments are BadArgument; without a GPU (QEMU) a plain Failed. On the
/// Mini-PC this test never calls it beyond the arguments (a PSP run is the desk's step over MCP, not a rerunnable
/// test; even `until=stop` would destroy a ring the desk kept).
fn bringup_tool() -> ktest::TestResult {
    use mcp::{Json, ToolContext, ToolError, ToolOutput};
    let t = mcp::find("gx_bringup").ok_or("gx_bringup not registered")?;
    kassert!(t.owner == "GX" && t.safety == mcp::Safety::Change, "owner {} safety {:?}", t.owner, t.safety);
    let ctx = ToolContext { caller: policy::Subject::KERNEL, deadline_ns: u64::MAX };
    let call = |args: &str| -> Result<String, ToolError> {
        let a = Json::parse(args).map_err(|e| ToolError::Failed(format!("{e:?}")))?;
        match (t.handler)(&ctx, &a)?.first() {
            Some(ToolOutput::Text(s)) => Ok(s.clone()),
            _ => Err(ToolError::Failed(String::from("no text answer"))),
        }
    };
    for bad in [r#"{"until":"dmcub"}"#, r#"{"keep":"yes"}"#, r#"{"again":1}"#, r#"{"until":"mode1"}"#, r#"{"until":"mmhub"}"#] {
        kassert!(matches!(call(bad), Err(ToolError::BadArgument(_))), "{bad}");
    }
    kassert!(t.input_schema.contains(r#""gart""#) && t.description.contains("until=gart"), "GX-7h: until=gart not in the schema");
    kassert!(t.input_schema.contains(r#""sdma""#) && t.description.contains("until=sdma"), "GX-7i: until=sdma not in the schema");
    kassert!(t.input_schema.contains(r#""gfx_ring""#) && t.description.contains("until=gfx_ring"), "GX-7j: until=gfx_ring not in the schema");
    if crate::gpu::Gpu::get().is_none() {
        let e = call(r#"{"until":"ring"}"#);
        kassert!(matches!(&e, Err(ToolError::Failed(s)) if s.starts_with("no AMD GPU")), "QEMU: {e:?}");
    }
    // The PSP run itself is the MCP tool gx_bringup (the desk's step): this test is about its arguments.
    Ok(())
}
ktest::ktest!("gx.bringup.tool", Safe, bringup_tool);

/// The hang of 29.10. (plan/notes/gx-GX-7j3-hang.md): the tool's state read after the `LIVE` lock, the one step
/// before its first klog event that QEMU never reached (no GPU: `tool` refuses first). INT-W28 took `MES_UP` twice
/// in it: there it spun and this test timed out; now it comes back with no recursive lock seen (debug builds' order
/// checker). Reads statics and takes two spin locks briefly, so Safe on the Mini-PC too (a kept run is not touched).
fn pre_state() -> ktest::TestResult {
    use crate::psplive;
    let before = ksync::lockdep::counts().unexpected;
    for until in [Until::Ring, Until::Fw, Until::Gfx, Until::GfxRing, Until::GfxMes, Until::Vm] {
        let p = psplive::pre_now(until, false, None, Ok(()), true, false);
        kassert!(p.until == until && !p.kept && !p.reset && (p.mes_up || !p.mes_queues), "{until:?}: {p:?}");
    }
    let p = psplive::pre_now(Until::Gfx, false, None, Ok(()), true, true);
    kassert!(p.reset && !p.gart_up && psplive::preflight(&p).is_err(), "reset: {p:?}");
    let after = ksync::lockdep::counts().unexpected;
    kassert!(after == before, "lock order checker: {} new violations ({:?})", after - before, ksync::lockdep::list());
    Ok(())
}
ktest::ktest!("gx.bringup.pre_state", Safe, pre_state);
