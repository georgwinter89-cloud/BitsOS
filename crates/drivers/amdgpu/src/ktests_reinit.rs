//! GX-7j3: the 780M started again without a reboot, on the fakes. The Mini-PC 29.10.: after `until=stop` or a
//! Reload the next GC stage's GetDriverIfVersion was never answered (the PSP had loaded the firmware again under
//! an IMU PMFW still ran for a GFX driver that never unloaded); the fake models that (`SmuFake::wedge_on_warm_load`).
//! The fix is Linux's unload order: PrepareMp1ForUnload and the RLC stop before the PSP's DESTROY_TMR.

use crate::bo::{BoManager, DISCOVERY_RESERVE, Reserved};
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP};
use crate::gpucore::classify::{self, Start};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::{self, StartAs};
use crate::gpucore::fwload;
use crate::gpucore::gfxup::GfxError;
use crate::gpucore::handoff::{self, GcFate, RingFate, StopNote, UnloadAddrs};
use crate::gpucore::psp::{Carve, Hw, Timeouts, Until};
use crate::gpucore::pspbringup::{self, GfxInputs, Inputs, Outcome};
use crate::gpucore::pspfake::{self, FakeGpu, Hostile, MapSource};
use crate::gpucore::smu::{MailboxState, SmuError, SmuTimeouts};
use crate::gpucore::smufake::{SmuHostile, msg};
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::watch::{DisplayGuard, Mode, Watch};
use crate::gpucore::{gfxring, sdma};
use crate::ktests_bringup::{GOP, Owner};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::kassert;

const SIZE: u64 = 512 << 20;

/// A fresh cold fake behind the live run's owner and BO manager (as `ktests_bringup::one_variant` builds them).
#[inline(never)]
fn boot(d: &Discovery) -> (Box<Owner>, BoManager) {
    let mut f = FakeGpu::with_carve(d, Hostile::None, SIZE);
    f.start_as(d, StartAs::Cold);
    f.reserved.push((f.fb_mc, f.fb_mc + GOP));
    let mut guard = DisplayGuard::from_discovery(d);
    guard.vram_mc = Some(f.fb_mc);
    guard.protect(f.fb_mc, GOP);
    let reserved = alloc::vec![
        Reserved { name: String::from("gop"), range: 0..GOP },
        Reserved { name: String::from("discovery"), range: SIZE - DISCOVERY_RESERVE..SIZE },
    ];
    (Box::new(Bringup::new(Watch::new(f, guard, Mode::Block), Gate::Allow, LOG_KEEP)), BoManager::new(SIZE, reserved))
}

/// One `gx_bringup` run on the owner as it is (same boot or after a Reload): the classifier with `note`, then the
/// engine; `unload_first`: `None` is the tool's rule (`pspbringup::unload_first`), `Some` forces it (the old
/// kernel had no catch-up).
#[inline(never)]
fn run(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, until: Until, note: Option<&StopNote>, unload_first: Option<bool>) -> Result<(Box<Outcome>, Start), String> {
    let opts = ListOpts::PSP13_0_4;
    let (fw, _) = fwload::load(&pspfake::plan_780m().files(), &mut MapSource(pspfake::synthetic_780m()), &opts).map_err(|e| format!("{e:?}"))?;
    let list = fw.load_list(&opts).map_err(|e| format!("{e:?}"))?;
    let f = owner.hw().hw();
    let st = classify::classify_with(d, &mut |b| f.reg_read(b), note);
    let sys = f.sdma.sys_alloc();
    let c = amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, SIZE);
    let inp = Inputs {
        addrs: f.a,
        carve: Carve { mc_base: f.fb_mc, phys_base: f.fb_phys },
        start: st.start,
        until,
        fw: &fw,
        list: &list,
        autoload: opts.autoload,
        keep: true,
        t: Timeouts::LINUX,
        gfx: GfxInputs::from_discovery(d),
        gmc: c.and_then(|c| crate::gpucore::gmc::GmcInputs::new(d, c)),
        sdma: c.and_then(|c| sdma::SdmaInputs::new(d, c, sdma::burst_nop(&fw))).map(|x| sdma::SdmaInputs { sys: Some(&sys), ..x }),
        gfx_ring: c.and_then(|c| gfxring::GfxRingInputs::new(d, c)),
        mes: None,
        vm: None,
        unload_first: unload_first.unwrap_or_else(|| pspbringup::unload_first(st.start, note.is_some_and(|n| n.gc == GcFate::Unloaded))),
    };
    Ok((pspbringup::run(owner, bo, &inp), st.start))
}

fn un(d: &Discovery) -> Result<UnloadAddrs, String> {
    GfxInputs::from_discovery(d).map(|g| UnloadAddrs { smu: g.smu, gc: g.gc }).ok_or_else(|| String::from("no SMU/GC addresses"))
}

/// `until=stop` as psplive's `stop_kept` does it on a kept run: the CP and SDMA halted, (`unload`) the GC unloaded
/// toward PMFW, the PSP's DESTROY_TMR/DESTROY_RINGS, the buffers back; the note it hands on.
#[inline(never)]
fn stop(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, mut out: Box<Outcome>, unload: bool) -> Result<(StopNote, Option<crate::gpucore::gfxup::Unload>), String> {
    if out.gfx_ring.is_some() {
        let a = gfxring::GfxAddrs::from_discovery(d).ok_or("no GFX addresses")?;
        kassert!(gfxring::halt(owner, &a), "CP halt");
    }
    if out.sdma.is_some() {
        let a = sdma::SdmaAddrs::from_discovery(d).ok_or("no SDMA addresses")?;
        kassert!(sdma::halt(owner, &a), "SDMA halt");
    }
    let gc = if out.gfx.as_ref().is_some_and(|g| g.error.is_none() && g.gc_touched) { GcFate::Booted } else { GcFate::Untouched };
    let u = un(d)?;
    let (gc, told) = if unload { handoff::unload_gc(owner, gc, Some(&u), SmuTimeouts::LINUX, false) } else { (gc, None) };
    let mut p = out.kept.take().ok_or("not kept")?;
    p.stop(owner).map_err(|e| format!("stop: {e:?}"))?;
    p.free(bo);
    let a = owner.hw().hw().a;
    Ok((handoff::read_note(owner.hw(), &a, RingFate::Destroyed, gc, true, false), told))
}

fn failed(out: &Outcome) -> String {
    format!("{:?} {:?} {:?} {:?}", out.error, out.pmfw, out.gfx.as_ref().map(|g| g.error), out.checks.iter().filter(|c| !c.pass).map(|c| (c.name, c.why.as_str())).collect::<Vec<_>>())
}

/// The 29.10. failure as the fake models it, and Linux's order that avoids it: stop then gfx/gart/sdma/gfx_ring
/// again in the same boot, three times each; a Reload (the Quiesce's unload, the note across) then gfx_ring, three
/// times; a stop of a kernel before GX-7j3 caught up by the start; a message left in flight answered late
/// (recovered) or never (refused before the PSP); PrepareMp1ForUnload never answered (a reboot); the Quiesce with
/// GC unsettled sends nothing; a crash's catch-up sends the message but leaves the RLC.
pub(crate) fn check_reinit(d: &Discovery) -> ktest::TestResult {
    // 1. The chip's failure: a stop without the unload, then a load: PMFW never answers GetDriverIfVersion.
    let (mut owner, mut bo) = boot(d);
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    kassert!(out.passed(), "first: {}", failed(&out));
    let (note, _) = stop(d, &mut owner, &mut bo, out, false)?;
    kassert!(note.gc == GcFate::Booted && owner.hw().hw().smu.drv.resp == crate::gpucore::smufake::resp::OK, "old stop: {note:?}");
    let (out, start) = run(d, &mut owner, &mut bo, Until::Gfx, Some(&note), Some(false))?;
    let f = owner.hw().hw();
    kassert!(start == Start::AfterBitsOs && f.smu.wedged, "the model: {start:?} wedged {}", f.smu.wedged);
    kassert!(out.gfx.as_ref().is_some_and(|g| g.error == Some(GfxError::Smu(SmuError::Timeout { id: msg::GET_DRIVER_IF_VERSION }))) && out.needs_reboot(), "the model: {}", failed(&out));
    // 2. The same old stop, then today's start: the catch-up unload (PrepareMp1ForUnload, the RLC stopped) comes
    // before the PSP, and the load passes.
    let (mut owner, mut bo) = boot(d);
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    let (note, _) = stop(d, &mut owner, &mut bo, out, false)?;
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, Some(&note), None)?;
    let f = owner.hw().hw();
    kassert!(out.passed() && !f.smu.wedged && out.catch_up.is_some_and(|u| u.told() && u.rlc.is_some_and(|r| r.1)), "catch-up: {}", failed(&out));
    // 3. The fixed stop, then the same stage again in the same boot, three times each (the desk's `again:true`).
    for until in [Until::Gfx, Until::Gart, Until::Sdma, Until::GfxRing] {
        let (mut owner, mut bo) = boot(d);
        let (mut out, _) = run(d, &mut owner, &mut bo, until, None, None)?;
        kassert!(out.passed(), "{until:?} first: {}", failed(&out));
        for round in 1..=3 {
            let what = format!("{until:?} after stop {round}");
            let n0 = owner.hw().hw().smu.taken.len();
            let (note, told) = stop(d, &mut owner, &mut bo, out, true)?;
            let f = owner.hw().hw();
            kassert!(note.gc == GcFate::Unloaded && told.is_some_and(|u| u.told() && u.rlc.is_some_and(|r| r.1)) && f.gc.rlc_stopped(), "{what}: stop {note:?} {told:?}");
            kassert!(f.smu.taken[n0..].iter().map(|t| t.1).collect::<Vec<_>>() == [msg::PREPARE_MP1_FOR_UNLOAD], "{what}: messages at the stop {:x?}", &f.smu.taken[n0..]);
            let (o, start) = run(d, &mut owner, &mut bo, until, Some(&note), None)?;
            let f = owner.hw().hw();
            kassert!(start == Start::AfterBitsOs && o.passed() && o.catch_up.is_none() && !f.smu.wedged, "{what}: {start:?} {}", failed(&o));
            kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "{what}: display touched");
            out = o;
        }
    }
    // 4. A Reload: the Quiesce unloads and destroys (its bounds), the note crosses, the next kernel's gfx_ring passes.
    let (mut owner, mut bo) = boot(d);
    let (mut out, _) = run(d, &mut owner, &mut bo, Until::GfxRing, None, None)?;
    let u = un(d)?;
    for round in 1..=3 {
        let what = format!("Reload {round}");
        let a = gfxring::GfxAddrs::from_discovery(d).ok_or("no GFX addresses")?;
        kassert!(crate::psplive::quiesce_halt_gfx(Some(&a), owner.hw()) == Some(true), "{what}: CP halt");
        let mut p = out.kept.take().ok_or("not kept")?;
        let pa = owner.hw().hw().a;
        let (r, n) = handoff::quiesce_kept(&mut p, owner.hw(), &pa, GcFate::Booted, Some(&u), false);
        p.free(&mut bo);
        kassert!(r == Some(true) && n.gc == GcFate::Unloaded, "{what}: {r:?} {n:?}");
        let n = StopNote::parse(&n.bytes()).ok_or("note did not cross")?;
        let (o, start) = run(d, &mut owner, &mut bo, Until::GfxRing, Some(&n), None)?;
        kassert!(start == Start::AfterBitsOs && o.passed() && o.catch_up.is_none() && !owner.hw().hw().smu.wedged, "{what}: {start:?} {}", failed(&o));
        out = o;
    }
    // 5. A message left in flight (a crash): answered within Linux's pre-poll wait, the run goes on (nothing sent
    // again); never answered: refused before the PSP, a reboot.
    let (mut owner, mut bo) = boot(d);
    let now = owner.hw().hw().now;
    owner.hw().hw().smu.in_flight(msg::GET_ENABLED_SMU_FEATURES, 0, now + 50_000);
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    kassert!(out.passed() && matches!(out.pmfw, Some(Ok(MailboxState::LateAnswer { msg: msg::GET_ENABLED_SMU_FEATURES, .. }))), "late answer: {}", failed(&out));
    kassert!(pspbringup::render(&out, &crate::ktests_bringup::dummy_inputs(&Default::default())).iter().any(|l| l.contains("recovered, nothing re-sent")), "late answer: no line");
    // 6. PrepareMp1ForUnload never answered at the stop: the note stays Booted, a reboot; the next run sees the
    // message in flight and stops before the PSP.
    let (mut owner, mut bo) = boot(d);
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    owner.hw().hw().smu.hostile.push(SmuHostile::Never { id: msg::PREPARE_MP1_FOR_UNLOAD });
    let (note, told) = stop(d, &mut owner, &mut bo, out, true)?;
    kassert!(note.gc == GcFate::Booted && told.is_some_and(|u| u.prepared == Err(SmuError::Timeout { id: msg::PREPARE_MP1_FOR_UNLOAD }) && u.prepared.is_err_and(|e| e.needs_reboot())), "never: {note:?} {told:?}");
    let cmds = owner.hw().hw().cmds.len();
    let (out, _) = run(d, &mut owner, &mut bo, Until::Gfx, Some(&note), None)?;
    kassert!(!out.passed() && out.pmfw_stuck() && out.needs_reboot() && owner.hw().hw().cmds.len() == cmds && out.gfx.is_none(), "never, then: {}", failed(&out));
    // 7. (fixer) The Quiesce with GC unsettled: nothing to PMFW, the RLC left alone, TMR and ring kept.
    let (mut owner, mut bo) = boot(d);
    let (mut out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    let (n0, cmds) = (owner.hw().hw().smu.taken.len(), owner.hw().hw().cmds.len());
    let mut p = out.kept.take().ok_or("not kept")?;
    let pa = owner.hw().hw().a;
    let (r, n) = handoff::quiesce_kept(&mut p, owner.hw(), &pa, GcFate::Unsettled, Some(&u), false);
    let f = owner.hw().hw();
    kassert!(r.is_none() && n.gc == GcFate::Unsettled && n.ring == RingFate::NotDestroyed, "unsettled: {r:?} {n:?}");
    kassert!(f.smu.taken.len() == n0 && f.cmds.len() == cmds && !f.gc.rlc_stopped(), "unsettled: messages {:x?}, PSP commands {}, RLC stopped {}", &f.smu.taken[n0..], f.cmds.len() - cmds, f.gc.rlc_stopped());
    // 8. (fixer) A crash in the same boot (the kernel replaced without a Quiesce, no note): the start catches up
    // the unload with the message alone; GC's power is unknown, so the RLC is not stopped.
    let (mut owner, mut bo) = boot(d);
    let (mut out, _) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    kassert!(out.kept.take().is_some(), "crash: not kept");
    let n0 = owner.hw().hw().smu.taken.len();
    let (out, start) = run(d, &mut owner, &mut bo, Until::Gfx, None, None)?;
    let f = owner.hw().hw();
    kassert!(start == Start::AfterCrash, "crash: start {start:?}");
    kassert!(out.catch_up.is_some_and(|u| u.told() && u.rlc.is_none()), "crash: catch-up {:?}", out.catch_up);
    kassert!(f.smu.taken.get(n0).map(|t| t.1) == Some(msg::PREPARE_MP1_FOR_UNLOAD), "crash: first message {:x?}", &f.smu.taken[n0..]);
    Ok(())
}

fn reinit() -> ktest::TestResult {
    check_reinit(&fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.bringup.reinit", Safe, reinit);
