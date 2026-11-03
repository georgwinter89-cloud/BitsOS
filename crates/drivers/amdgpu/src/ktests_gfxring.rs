//! GX-7j in-kernel tests of the GFX ring stage (`gx_bringup {"until":"gfx_ring"}`) against the fakes: the same
//! engine (`pspbringup::run` with `Until::GfxRing`, `gpucore::gfxring::enable`) over the same owner stack as the
//! live run, ring 0 executed by GR-2's PM4 interpreter inside the chip fake (`gfxfake`), for the start states and
//! carve-outs of 512 MiB, 8 GiB and 16 GiB; every failure the fake can stage (a CP that never fetches the direct
//! ring, no writeback, MMIO wptr ignored, doorbells dropped, IBs refused, lost 64-bit fences, a stray write, GC
//! gated, MES running, a CP that never idles); a second stage on a running CP; a ring an earlier kernel left;
//! the stop's doorbell side; the Quiesce halt. GX-7j2: the three start orders (e1a, e1b, e1c) with MEC unhalted
//! first, the chip's stuck PFP (MEC left halted) and the active-ring hypothesis, the desk's e1a -> e1b -> e1c
//! sequence on one owner. Nothing touches a device: Safe and rerunnable.

use crate::bo::BoManager;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::gcfake::GcHostile;
use crate::gpucore::gfxfake::{self, GfxHostile};
use crate::gpucore::gfxring::{self, GfxAddrs, GfxError, GfxReport, GfxRingInputs, Variant};
use crate::gpucore::gmc::GmcInputs;
use crate::gpucore::psp::{Hw as _, Until};
use crate::gpucore::pspbringup::{self, Outcome};
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::ucode::ListOpts;
use crate::ipmod::gfx11::{self, R, f};
use crate::ktests_bringup::{GOP, Owner, one_variant};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use ktest::kassert;

const SIZES: [u64; 3] = [512 << 20, 8 << 30, 16 << 30];
/// The rungs' write pointers on a ring that starts at 0 (dwords): the clear-state preamble pads to 1024, every
/// rung to 256 (GFX ring alignment); the doorbell rung wraps the 2048-dword ring.
pub const WPTRS: [u64; 5] = [1280, 1536, 1792, 2048, 2304];

/// One `until=gfx_ring` run as the tool makes it (`setup` on the fake after the classifier read it).
pub(crate) fn gfx_run(d: &Discovery, size: u64, s: StartAs, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager), String> {
    gfx_run_v(d, size, s, Variant::DEFAULT, setup)
}

/// The same with the start order `v`.
pub(crate) fn gfx_run_v(d: &Discovery, size: u64, s: StartAs, v: Variant, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager), String> {
    let (out, owner, bo, _) = one_variant(d, size, s, Until::GfxRing, false, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, v, setup)?;
    Ok((out, owner, bo))
}

/// The capture's CP_STAT values after the unhalt as the stage records them.
fn capture_stat() -> Vec<(u64, Option<u32>)> {
    gfxfake::UNHALT_STAT.iter().map(|&(t, v)| (t, Some(v))).collect()
}

/// The chip's stuck PFP as the stage records it.
fn stuck_stat() -> Vec<(u64, Option<u32>)> {
    alloc::vec![(0, Some(gfxfake::STUCK_STAT.0)), (gfxfake::STUCK_SWITCH_US, Some(gfxfake::STUCK_STAT.1))]
}

pub(crate) fn report(out: &Outcome) -> Result<&GfxReport, String> {
    out.gfx_ring.as_ref().ok_or_else(|| format!("no GFX stage: gmc {:?}, checks {:?}", out.gmc.as_ref().map(|g| g.error), out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()))
}

fn addrs(d: &Discovery) -> Result<GfxAddrs, String> {
    GfxAddrs::from_discovery(d).ok_or_else(|| String::from("no GFX addresses"))
}

fn carve(owner: &mut Owner) -> Result<amdgpu_fmt::addr::Carveout, String> {
    let f = owner.hw().hw();
    amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, f.fb_size).ok_or_else(|| String::from("carve"))
}

/// What every passed run must show on the fake: every rung through the interpreter, the doorbell taken, the CP
/// running, MES untouched, the clear-state buffer where RLC_CSIB points, the picture untouched, every write on
/// the live owner's lists.
fn check_run(d: &Discovery, out: &Outcome, owner: &mut Owner, size: u64, start: u64, what: &str) -> ktest::TestResult {
    let r = report(out)?.clone();
    let failed: Vec<_> = out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect();
    kassert!(out.passed(), "{what}: {failed:?} {:?}", r.error);
    kassert!(r.rungs.len() == gfxring::RUNGS.len() && r.rungs.iter().all(|x| x.pass && x.done) && r.left_running, "{what}: rungs {:?}", r.rungs);
    let w: Vec<u64> = r.rungs.iter().map(|x| x.wptr).collect();
    let want: Vec<u64> = WPTRS.iter().map(|x| x + start).collect();
    kassert!(w == want, "{what}: wptrs {w:?}");
    let l = r.layout.ok_or("no layout")?;
    let a = addrs(d)?;
    let f = owner.hw().hw();
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty() && out.violations_total == 0, "{what}: display {:?} scanout {:?}", f.display_writes, f.scanout_writes);
    kassert!(f.gc.off_access.is_empty(), "{what}: GC touched while off {:?}", f.gc.off_access);
    // Any carve-out size: the buffer inside it, clear of the GOP and the discovery reserve.
    let off = l.bo_mc - f.fb_mc;
    kassert!(off >= GOP && off + gfxring::BO_BYTES <= size - crate::bo::DISCOVERY_RESERVE, "{what}: buffer at +{off:#x}");
    // The CP wrote only into its buffer (never the GOP surface).
    let bo_phys = f.fb_phys + off..f.fb_phys + off + gfxring::BO_BYTES;
    let stray: Vec<_> = f.gfx.gpu_writes.iter().filter(|&&(p, n)| !(bo_phys.contains(&p) && p + n as u64 <= bo_phys.end)).collect();
    kassert!(stray.is_empty(), "{what}: CP writes outside its buffer {stray:x?}");
    kassert!(f.gfx.running() && f.gfx.doorbells.iter().filter(|x| x.2).count() == 1, "{what}: running {} doorbells {:?}", f.gfx.running(), f.gfx.doorbells);
    kassert!(f.gfx.get(R::CpRbDoorbellControl) & f::DOORBELL_EN != 0 && f.gfx.get(R::CpMeCntl) & f::GFX_HALTS == 0, "{what}: doorbell control / CP_ME_CNTL");
    // The clear-state preamble ran on the ring (its SET_CONTEXT_REG writes), RLC_CSIB points at the buffer.
    kassert!(f.gfx.ring_reg_writes > 900, "{what}: ring register writes {}", f.gfx.ring_reg_writes);
    let rd = |f: &mut FakeGpu, r: R| a.byte(r).and_then(|b| f.reg_read(b));
    kassert!(rd(f, R::RlcCsibAddrLo) == Some(l.csb as u32) && rd(f, R::RlcCsibAddrHi) == Some((l.csb >> 32) as u32) && rd(f, R::RlcCsibLength) == Some(gfxring::CSB_DW), "{what}: RLC_CSIB");
    let mut b = alloc::vec![0u8; gfxring::CSB.len()];
    kassert!(f.mem_read(l.csb, &mut b) && b == gfxring::CSB, "{what}: the clear-state buffer in memory");
    let mut b = [0u8; 8];
    kassert!(f.mem_read(l.fence64, &mut b) && u64::from_le_bytes(b) == gfxring::FENCE64_START + 1, "{what}: fence64 {b:x?}");
    kassert!(f.hubs.faults.is_empty(), "{what}: VM faults {:?}", f.hubs.faults);
    kassert!(a.aper_en.is_some_and(|b| f.reg(b) & 1 == 1), "{what}: the doorbell aperture");
    // MES never written, MEC unhalted once (0x40000000 -> 0, Linux's cp_compute_enable) and running; CP_STAT went
    // through the capture's values; every GFX write of the run is on the live owner's lists.
    kassert!(!out.entries.iter().any(|e| Some(e.byte) == a.byte(R::CpMesCntl)), "{what}: MES written");
    let mec: Vec<u32> = out.entries.iter().filter(|e| Some(e.byte) == a.byte(R::CpMecCntl)).map(|e| e.value).collect();
    kassert!(mec == [0] && !f.gfx.mec_halted() && r.mec_unhalt == Some((f::MEC_ME1_HALT, 0)), "{what}: MEC writes {mec:x?} {:x?}", r.mec_unhalt);
    kassert!(r.unhalt_stat == capture_stat() && !f.gfx.stuck(), "{what}: CP_STAT after the unhalt {:x?}", r.unhalt_stat);
    let wl = a.writable();
    let bad: Vec<u32> = out.entries.iter().filter(|e| a.name_at(e.byte).is_some() && !wl.contains(&e.byte)).map(|e| e.byte).collect();
    kassert!(bad.is_empty(), "{what}: writes off the live lists {bad:x?}");
    let fw = crate::gpucore::ucode::FwSet::default();
    let text = pspbringup::render(out, &crate::ktests_bringup::dummy_inputs(&fw));
    kassert!(text.iter().any(|l| l.starts_with("GFX RELEASE_MEM 64-bit fence (M5): PASS")) && text.iter().any(|l| l.contains("gfx GFX golden IB (capture): ok")), "{what}: answer lines");
    kassert!(text.iter().any(|l| l.starts_with("# gfx tail:")) && text.iter().any(|l| l.starts_with("# gfx resume:")), "{what}: no marks");
    Ok(())
}

pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    for size in SIZES {
        for s in [StartAs::Cold, StartAs::AfterBitsOs] {
            let what = format!("{s:?} at {size:#x}");
            let (out, mut owner, _) = gfx_run(d, size, s, &|_| {})?;
            check_run(d, &out, &mut owner, size, 0, &what)?;
            let r = report(&out)?;
            kassert!(!r.halted_first && r.rewind.is_none() && !r.reused_bo, "{what}: halted first {} rewind {:?}", r.halted_first, r.rewind);
        }
        // After Linux MES runs (it maps the queues): refused before any GC CP write, nothing left running.
        let (out, mut owner, _) = gfx_run(d, size, StartAs::AfterLinux, &|_| {})?;
        let r = report(&out)?;
        kassert!(matches!(r.error, Some(GfxError::MesRunning { .. })) && r.layout.is_none() && !out.passed(), "after Linux at {size:#x}: {:?}", r.error);
        let a = addrs(d)?;
        let cp: Vec<u32> = R::all().into_iter().filter_map(|x| a.byte(x)).collect();
        kassert!(!out.entries.iter().any(|e| cp.contains(&e.byte)), "after Linux: a CP/tail register written");
        kassert!(gfxring::render(r).iter().any(|l| l.contains("MesRunning") && l.contains("direct CP_RB0 path needs MES idle")), "after Linux: no advice");
        kassert!(owner.hw().hw().display_writes.is_empty(), "after Linux: display touched");
    }
    Ok(())
}

/// A passed `until=gart` run and GFXHUB's addresses for a direct stage on its owner.
fn after_gart(d: &Discovery) -> Result<(Box<Owner>, BoManager, crate::gpucore::gmc::HubAddrs), String> {
    let (_, mut owner, bo) = crate::ktests_gmc::gart(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let hub = GmcInputs::new(d, carve(&mut owner)?).ok_or("gmc inputs")?.addrs.gfx;
    Ok((owner, bo, hub))
}

fn direct(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, hub: &crate::gpucore::gmc::HubAddrs, prev: Option<gfxring::GfxBo>) -> Result<GfxReport, String> {
    direct_armed(d, owner, bo, hub, prev).map(|x| x.0)
}

/// The same, with the arm hook counting: (report, times armed).
fn direct_armed(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, hub: &crate::gpucore::gmc::HubAddrs, prev: Option<gfxring::GfxBo>) -> Result<(GfxReport, u32), String> {
    let armed = Cell::new(0u32);
    let arm = |_: GfxAddrs| armed.set(armed.get() + 1);
    let inp = GfxRingInputs { bo: prev, arm: Some(&arm), ..GfxRingInputs::new(d, carve(owner)?).ok_or("inputs")? };
    let r = gfxring::enable(owner, bo, &inp, hub);
    Ok((r, armed.get()))
}

fn direct_v(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, hub: &crate::gpucore::gmc::HubAddrs, prev: Option<gfxring::GfxBo>, v: Variant) -> Result<GfxReport, String> {
    let inp = GfxRingInputs { bo: prev, variant: v, ..GfxRingInputs::new(d, carve(owner)?).ok_or("inputs")? };
    Ok(gfxring::enable(owner, bo, &inp, hub))
}

fn halted(f: &FakeGpu) -> bool {
    f.gfx.get(R::CpMeCntl) & f::GFX_HALTS == f::GFX_HALTS && !f.gfx.running()
}

pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    // Each hostile: the failing rung named, the CP halted again.
    for (h, rung, err) in [
        (GfxHostile::Dead, 0, "Timeout"),
        (GfxHostile::NoWriteback, 0, "Timeout"),
        (GfxHostile::IgnoreMmioWptr, 0, "Timeout"),
        (GfxHostile::IbRefused, 1, "Timeout"),
        (GfxHostile::Fence64Lost, 2, "Result"),
        (GfxHostile::NoDoorbell, 4, "Timeout"),
    ] {
        let what = format!("{h:?}");
        let (out, mut owner, _) = gfx_run(d, 512 << 20, StartAs::Cold, &|f| f.gfx.hostile = h)?;
        let r = report(&out)?;
        kassert!(!out.passed() && r.rungs.len() == rung + 1 && !r.rungs[rung].pass && r.rungs[..rung].iter().all(|x| x.pass), "{what}: rungs {:?}", r.rungs);
        kassert!(r.error.as_ref().is_some_and(|e| format!("{e:?}").starts_with(err)), "{what}: error {:?}", r.error);
        kassert!(!r.left_running && halted(owner.hw().hw()) && owner.hw().hw().gfx.mec_halted(), "{what}: CP or MEC not halted");
        kassert!(out.checks.iter().any(|c| c.name == gfxring::RUNGS[rung] && !c.pass), "{what}: no failed check");
        if rung == 0 {
            // The direct path never ran: the answer says what is missing.
            kassert!(r.error.as_ref().is_some_and(|e| e.advice().contains("MES is next") && e.advice().contains("CP and MEC are halted again")), "{what}: advice {:?}", r.error.as_ref().map(GfxError::advice));
        }
        if h == GfxHostile::Fence64Lost {
            kassert!(r.rungs[2].detail.contains("reads 0xcafedeadcafedead"), "{}", r.rungs[2].detail);
        }
    }
    // The CP reports its read pointer as the ring offset: accepted (compared within the ring).
    let (out, mut owner, _) = gfx_run(d, 512 << 20, StartAs::Cold, &|f| f.gfx.hostile = GfxHostile::RptrWrapped)?;
    check_run(d, &out, &mut owner, 512 << 20, 0, "rptr wrapped")?;
    kassert!(report(&out)?.rungs[4].rptr == Some(WPTRS[4] & (gfx11::RING_DW - 1)), "wrapped: {:?}", report(&out)?.rungs[4].rptr);
    // A stray CP write (GFXHUB records a VM fault): every rung passes, the stage fails on the fault rule and the
    // CP is halted again.
    let (out, mut owner, _) = gfx_run(d, 512 << 20, StartAs::Cold, &|f| f.gfx.hostile = GfxHostile::StrayWrite)?;
    let r = report(&out)?;
    kassert!(r.rungs.len() == gfxring::RUNGS.len() && r.rungs.iter().all(|x| x.pass) && !owner.hw().hw().hubs.faults.is_empty(), "stray: rungs {:?}", r.rungs);
    kassert!(!out.passed() && !r.passed() && !r.left_running && halted(owner.hw().hw()), "stray: passed or left running");
    kassert!(out.checks.iter().any(|c| c.name == "GFX no new VM fault" && !c.pass), "stray: fault rule not failed");
    // GC gated: refused before any write, and the Quiesce halt not armed.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    owner.hw().hw().gc.power = crate::gpucore::gcfake::Power::Off;
    let n0 = owner.log().totals().writes;
    let (r, armed) = direct_armed(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(matches!(r.error, Some(GfxError::GcOff { .. })) && owner.log().totals().writes == n0 && r.layout.is_none() && armed == 0, "GC off: {:?} armed {armed}", r.error);
    // MES running: refused before any write, not armed.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    // GX-7k: the MES fake answers CP_MES_CNTL (both pipes running, as Linux leaves it).
    owner.hw().hw().mes.as_linux_left();
    let n0 = owner.log().totals().writes;
    let (r, armed) = direct_armed(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(r.error == Some(GfxError::MesRunning { mes: 0x0c00_0000 }) && owner.log().totals().writes == n0 && armed == 0, "MES: {:?} armed {armed}", r.error);
    // CP_STAT never idle after the unhalt: CpBusy, the CP halted again, no rung run.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    owner.hw().hw().gc.hostile = GcHostile::CpBusy;
    let r = direct(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(r.error == Some(GfxError::CpBusy { what: "unhalt", stat: Some(crate::gpucore::gcfake::CP_STAT_BOOTING) }) && r.rungs.is_empty() && halted(owner.hw().hw()), "CP busy: {:?}", r.error);
    // CP_ME_CNTL refuses writes: the unhalt fails and the failure path's halt fails too. The report and the
    // render say the CP may still run (not "halted").
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let me = addrs(d)?.byte(R::CpMeCntl).ok_or("CP_ME_CNTL")?;
    owner.hw().hw().hostile = Hostile::RefuseWrite(me);
    let r = direct(d, &mut owner, &mut bo, &hub, None)?;
    let text = gfxring::render(&r).join("\n");
    kassert!(r.error == Some(GfxError::Write { byte: me }) && r.halt_failed == Some(GfxError::Write { byte: me }) && !r.left_running, "halt refused: {:?} / {:?}", r.error, r.halt_failed);
    kassert!(text.contains("CP halt FAILED") && !text.contains("gfx: CP halted"), "halt refused: {text}");
    kassert!(owner.hw().hw().gfx.mec_halted(), "halt refused: MEC left running");
    // A tail cut off inside the KFD VMID loop: VMID 0 and the broadcast index are put back before the halt.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let a = addrs(d)?;
    let spi = a.byte(R::SpiGdbgPerVmidCntl).ok_or("SPI_GDBG_PER_VMID_CNTL")?;
    owner.hw().hw().hostile = Hostile::RefuseWrite(spi);
    let r = direct(d, &mut owner, &mut bo, &hub, None)?;
    let (cntl, index) = (a.byte(R::GrbmGfxCntl).ok_or("GRBM_GFX_CNTL")?, a.byte(R::GrbmGfxIndex).ok_or("GRBM_GFX_INDEX")?);
    let f = owner.hw().hw();
    let (c, i) = (f.reg_read(cntl), f.reg_read(index));
    kassert!(r.error == Some(GfxError::Write { byte: spi }) && r.halt_failed.is_none() && halted(f), "tail cut: {:?}", r.error);
    kassert!(c == Some(0) && i == Some(gfx11::GFX_INDEX_BROADCAST), "tail cut: GRBM_GFX_CNTL {c:x?} GRBM_GFX_INDEX {i:x?}");
    // The pass rules on a passed report: a changed fault register fails the stage.
    let (out, ..) = gfx_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let ok = report(&out)?.clone();
    let mut x = ok.clone();
    x.fault_after = x.fault_after.map(|f| crate::gpucore::gmc::FaultRead { status: Some(0x100), ..f });
    kassert!(ok.passed() && !x.passed() && gfxring::checks(&x).iter().any(|c| c.name == "GFX no new VM fault" && !c.pass), "fault rule");
    Ok(())
}

/// A second stage on the running CP (`again=true`): halted first, the buffer reused, the pointers reset, every
/// rung again; then the Quiesce halt; then a ring an earlier kernel left.
pub(crate) fn check_again(d: &Discovery) -> ktest::TestResult {
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let (r1, armed) = direct_armed(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(r1.passed() && !r1.halted_first && !r1.reused_bo && armed == 1, "first: {:?} armed {armed}", r1.error);
    let r2 = direct(d, &mut owner, &mut bo, &hub, r1.bo)?;
    kassert!(r2.passed() && r2.halted_first && r2.reused_bo && r2.layout == r1.layout && r2.deactivated, "again: {:?} {:?}", r2.error, r2.rungs);
    let w = r2.rewind.ok_or("again: no restart recorded")?;
    kassert!(w.found.0 == WPTRS[4] && w.found.1 == WPTRS[4] && w.found.2 & f::DOORBELL_EN != 0 && w.after == (0, 0, w.after.2) && w.after.2 & f::DOORBELL_EN == 0 && w.start == 0, "again: {w:x?}");
    kassert!(gfxring::render(&r2).iter().any(|l| l.starts_with("gfx restart: found CP_RB0_RPTR")), "again: no restart line");
    kassert!(owner.hw().hw().gfx.running(), "not running after again");
    let a = addrs(d)?;
    // The Quiesce halt while a run holds the PSP lock: raw registers only, no wait, nothing when not armed.
    let t0 = owner.hw().hw().now;
    let n0 = owner.log().totals().writes;
    let (none, ok) = crate::psplive::with_live_held(|_| (crate::psplive::quiesce_halt_gfx(None, owner.hw()), crate::psplive::quiesce_halt_gfx(Some(&a), owner.hw())));
    kassert!(none.is_none() && ok == Some(true), "quiesce halt: {none:?} {ok:?}");
    kassert!(owner.hw().hw().now == t0 && owner.log().totals().writes == n0, "quiesce halt waited or went through the owner's log");
    kassert!(owner.hw().hw().gfx.get(R::CpMeCntl) & f::ALL_HALTS == f::ALL_HALTS && halted(owner.hw().hw()) && owner.hw().hw().gfx.mec_halted(), "halt left the CP or MEC running");
    // A restart whose CP_RB_RPTR_WR is ignored: the kept read pointer (a counter past the ring) is stale: refused.
    owner.hw().hw().gfx.hostile = GfxHostile::IgnoreRptrWr;
    let r3 = direct(d, &mut owner, &mut bo, &hub, r1.bo)?;
    kassert!(matches!(r3.error, Some(GfxError::StalePointers { .. })) && r3.rungs.is_empty() && halted(owner.hw().hw()), "stale: {:?}", r3.error);
    // Linux's plan from what the first stage found: the tail and the resume (the phase tests check the values).
    let seq = gfxring::linux_sequence(&r1, &a);
    kassert!(seq.len() == 196 && seq.first().map(|x| x.0) == a.byte(R::CgttGsNggClkCtrl) && seq.last().map(|x| x.0) == a.byte(R::CpDeviceId), "linux sequence {}", seq.len());
    // A ring the kernel before left (a Reload: halted by its Quiesce hook, doorbell on, at 0x500): reset to 0;
    // with CP_RB_RPTR_WR ignored the ring-aligned offset is taken over.
    for keep in [false, true] {
        let what = format!("stale ring, rptr kept {keep}");
        let (out, mut owner, _) = gfx_run(d, 512 << 20, StartAs::AfterBitsOs, &|f| {
            f.gfx.stale_ring(0x500);
            if keep {
                f.gfx.hostile = GfxHostile::IgnoreRptrWr;
            }
        })?;
        let start = if keep { 0x500 } else { 0 };
        check_run(d, &out, &mut owner, 512 << 20, start, &what)?;
        let w = report(&out)?.rewind.ok_or("no restart recorded")?;
        kassert!(w.found.0 == 0x500 && w.start == start && w.after.2 & f::DOORBELL_EN == 0, "{what}: {w:x?}");
    }
    Ok(())
}

/// What a stop puts back after the halt (`gfxring::doorbell_back`, as until=stop does): the ring's doorbell off,
/// the aperture as the boot's first doorbell stage found it; rerunnable without writes; a later stage starts again.
pub(crate) fn check_stop_back(d: &Discovery) -> ktest::TestResult {
    let a = addrs(d)?;
    let aper = a.aper_en.ok_or("aperture")?;
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let r1 = direct(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(r1.passed() && r1.aper_found() == Some(0) && r1.turned_aperture_on(), "first: {:?} {:x?}", r1.error, r1.aper);
    // psplive's keeping: the first doorbell stage's found aperture stays; ours now.
    let mut first = None;
    kassert!(crate::psplive::keep_aper_first(&mut first, r1.aper_found()) == Some(0) && crate::psplive::keep_aper_first(&mut first, Some(1)) == Some(0), "aper first");
    kassert!(crate::psplive::aperture_ours(first, r1.aper.1) && !crate::psplive::aperture_ours(Some(1), Some(1)) && !crate::psplive::aperture_ours(first, Some(0)), "aperture ours");
    kassert!(gfxring::halt(&mut *owner, &a), "halt");
    let (ok, lines) = gfxring::doorbell_back(&mut owner, &a, first);
    kassert!(ok && lines.iter().any(|l| l.contains("GFX ring 0's doorbell off")) && lines.iter().any(|l| l.contains("APER_EN back to 0x0")), "back: {lines:?}");
    {
        let f = owner.hw().hw();
        kassert!(halted(f) && f.gfx.get(R::CpRbDoorbellControl) & f::DOORBELL_EN == 0 && f.reg(aper) & 1 == 0, "not back: control {:#x} aperture {:#x}", f.gfx.get(R::CpRbDoorbellControl), f.reg(aper));
        kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "display touched");
    }
    // Rerunnable: a second stop writes nothing; without the aperture (an SDMA stage puts it back) only the doorbell.
    let n0 = owner.log().totals().writes;
    let (ok, lines) = gfxring::doorbell_back(&mut owner, &a, first);
    kassert!(ok && owner.log().totals().writes == n0 && lines.iter().filter(|l| l.contains("as found") || l.contains("already off")).count() == 2, "second stop: {lines:?}");
    let (ok, lines) = gfxring::doorbell_back(&mut owner, &a, None);
    kassert!(ok && lines.len() == 1, "no aperture: {lines:?}");
    // A stage after the stop starts again (its pointers reset) and passes.
    let r2 = direct(d, &mut owner, &mut bo, &hub, r1.bo)?;
    kassert!(r2.passed() && r2.rewind.is_some() && r2.aper_found() == Some(0), "after the stop: {:?}", r2.error);
    // The live owner's doorbell rule: only named doorbells of BAR2's first page.
    let ok = |l: &[u32], i| crate::psplive::PspHw::doorbell_ok(l, i);
    kassert!(ok(&[gfx11::DOORBELL_DW], gfx11::DOORBELL_DW) && !ok(&[crate::ipmod::sdma6::DOORBELL_DW], gfx11::DOORBELL_DW) && !ok(&[0x400], 0x400), "doorbell rule");
    Ok(())
}

/// The GFX stage after a passed SDMA stage on the same owner (the desk's `until=sdma` then `until=gfx_ring`): SDMA's
/// rung 4 left the aperture on, so the GFX stage finds it on and writes it not; both engines run, each doorbell
/// taken once; the CP halt leaves SDMA running and the SDMA halt leaves the CP running.
pub(crate) fn check_after_sdma(d: &Discovery) -> ktest::TestResult {
    let (out, mut owner, mut bo) = crate::ktests_sdma::sdma_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    kassert!(out.passed(), "sdma run failed");
    let hub = GmcInputs::new(d, carve(&mut owner)?).ok_or("gmc inputs")?.addrs.gfx;
    let aper = addrs(d)?.aper_en.ok_or("aperture")?;
    let n_aper = owner.log().count(aper);
    let r = direct(d, &mut owner, &mut bo, &hub, None)?;
    kassert!(r.passed() && r.aper == (Some(1), Some(1)) && !r.turned_aperture_on() && owner.log().count(aper) == n_aper, "after sdma: {:?} {:x?}", r.error, r.aper);
    let f = owner.hw().hw();
    kassert!(f.gfx.running() && f.sdma.running(), "both running: gfx {} sdma {}", f.gfx.running(), f.sdma.running());
    kassert!(f.gfx.doorbells.iter().filter(|x| x.2).count() == 1 && f.sdma.doorbells.iter().filter(|x| x.2).count() == 1, "doorbells {:?} {:?}", f.gfx.doorbells, f.sdma.doorbells);
    kassert!(f.display_writes.is_empty() && f.hubs.faults.is_empty(), "display or faults");
    let a = addrs(d)?;
    kassert!(gfxring::halt(f, &a) && !f.gfx.running() && f.sdma.running(), "the CP halt touched SDMA");
    Ok(())
}

/// The index of the first write of `r` at or after `from` in the run's entries.
fn at(out: &Outcome, a: &GfxAddrs, r: R, from: usize) -> Option<usize> {
    let b = a.byte(r)?;
    out.entries.iter().skip(from).position(|e| e.byte == b).map(|i| i + from)
}

/// GX-7j2: the three start orders through the tool's path, the chip's failure and the active-ring hypothesis on
/// the fake, the desk's sequence on one owner, the Linux plans.
pub(crate) fn check_variants(d: &Discovery) -> ktest::TestResult {
    let a = addrs(d)?;
    let fw = crate::gpucore::ucode::FwSet::default();
    for size in [512 << 20, 16 << 30] {
        // e1a: Linux's unhalt only, CP_STAT the capture's, halted again; no ring register, no rung.
        let what = format!("e1a at {size:#x}");
        let (out, mut owner, _) = gfx_run_v(d, size, StartAs::Cold, Variant::E1a, &|_| {})?;
        let r = report(&out)?.clone();
        kassert!(
            out.passed() && r.passed() && r.rungs.is_empty() && !r.left_running && r.rehalted,
            "{what}: {:?} {:?}",
            r.error,
            out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()
        );
        kassert!(r.unhalt_stat == capture_stat() && r.rehalt == Some((Some(0x1500_0000), Some(f::MEC_ME1_HALT), Some(0))), "{what}: {:x?} {:x?}", r.unhalt_stat, r.rehalt);
        let ring: Vec<u32> =
            [R::CpRb0Cntl, R::CpRb0Wptr, R::CpRb0Base, R::CpRbActive, R::CpRbVmid, R::CpRbDoorbellControl, R::CpMaxContext, R::CpDeviceId, R::CpRbRptrWr].iter().filter_map(|&x| a.byte(x)).collect();
        kassert!(!out.entries.iter().any(|e| ring.contains(&e.byte)), "{what}: a ring register written");
        let lo = at(&out, &a, R::CpRbDoorbellRangeLower, 0).ok_or("e1a: no doorbell range")?;
        let mec = at(&out, &a, R::CpMecCntl, lo).ok_or("e1a: no MEC unhalt")?;
        let me = at(&out, &a, R::CpMeCntl, mec).ok_or("e1a: no unhalt")?;
        let seq: Vec<(u32, u32)> = out.entries[lo..].iter().map(|e| (e.byte, e.value)).collect();
        let want: Vec<(u32, u32)> = gfx11::doorbell_ranges()
            .iter()
            .filter_map(|&(x, v)| Some((a.byte(x)?, v)))
            .chain([
                (a.byte(R::CpMecCntl).ok_or("mec")?, 0),
                (a.byte(R::CpMeCntl).ok_or("me")?, 0x0100_0000),
                (a.byte(R::CpMeCntl).ok_or("me")?, 0x1500_0000),
                (a.byte(R::CpMecCntl).ok_or("mec")?, f::MEC_ME1_HALT),
            ])
            .collect();
        kassert!(seq == want && mec == lo + 4 && me == mec + 1, "{what}: the start's writes {seq:x?}");
        {
            let f = owner.hw().hw();
            kassert!(
                halted(f) && f.gfx.mec_halted() && f.gfx.unhalts == [(false, false, false)] && f.display_writes.is_empty() && f.scanout_writes.is_empty(),
                "{what}: halted {} unhalts {:?}",
                halted(f),
                f.gfx.unhalts
            );
        }
        let text = pspbringup::render(&out, &crate::ktests_bringup::dummy_inputs(&fw));
        kassert!(text.iter().any(|l| l == "gfx verdict (e1a): PASS") && text.iter().any(|l| l.starts_with("gfx unhalt CP_STAT (e1a): 0x88028000 at 0 us, 0x80028000 at 3 us")), "{what}: answer lines");
        kassert!(out.checks.iter().any(|c| c.name.starts_with("GFX CP and MEC halted again") && c.pass) && !out.checks.iter().any(|c| gfxring::RUNGS.contains(&c.name)), "{what}: checks");
        // e1b: the unhalt first, then the ring with CP_RB_ACTIVE last (behind its doorbell writes), every rung.
        let what = format!("e1b at {size:#x}");
        let (out, mut owner, _) = gfx_run_v(d, size, StartAs::Cold, Variant::E1b, &|_| {})?;
        check_run(d, &out, &mut owner, size, 0, &what)?;
        let me = at(&out, &a, R::CpMeCntl, at(&out, &a, R::CpMecCntl, 0).ok_or("e1b: mec")?).ok_or("e1b: me")?;
        let first_ring = at(&out, &a, R::CpRbWptrDelay, 0).ok_or("e1b: no ring")?;
        let act = at(&out, &a, R::CpRbActive, 0).ok_or("e1b: no CP_RB_ACTIVE")?;
        let range = at(&out, &a, R::CpRbDoorbellRangeUpper, me).ok_or("e1b: no set_doorbell range")?;
        kassert!(me < first_ring && range < act && at(&out, &a, R::CpDeviceId, 0).is_some_and(|x| x > act), "{what}: order me {me} ring {first_ring} active {act}");
        kassert!(owner.hw().hw().gfx.unhalts == [(false, false, false)] && report(&out)?.ring_stat == Some(Some(0)), "{what}: unhalts {:?}", owner.hw().hw().gfx.unhalts);
        // e1c: MEC, the ring and CP_RB_ACTIVE on the halted CP, then the unhalt.
        let what = format!("e1c at {size:#x}");
        let (out, mut owner, _) = gfx_run_v(d, size, StartAs::Cold, Variant::E1c, &|_| {})?;
        check_run(d, &out, &mut owner, size, 0, &what)?;
        let mec = at(&out, &a, R::CpMecCntl, 0).ok_or("e1c: mec")?;
        let act = at(&out, &a, R::CpRbActive, 0).ok_or("e1c: active")?;
        let me = at(&out, &a, R::CpMeCntl, act).ok_or("e1c: me")?;
        kassert!(mec < act && act < me && at(&out, &a, R::CpDeviceId, 0).is_some_and(|x| x < me), "{what}: order mec {mec} active {act} me {me}");
        kassert!(owner.hw().hw().gfx.unhalts == [(false, true, false)], "{what}: unhalts {:?}", owner.hw().hw().gfx.unhalts);
    }
    // H1 (an active ring at the unhalt hangs the PFP): e1a and e1b pass, e1c hangs.
    for (v, ok) in [(Variant::E1a, true), (Variant::E1b, true), (Variant::E1c, false)] {
        let (out, mut owner, _) = gfx_run_v(d, 512 << 20, StartAs::Cold, v, &|f| f.gfx.hostile = GfxHostile::ActiveRingHang)?;
        let r = report(&out)?;
        kassert!(out.passed() == ok && (ok || matches!(r.error, Some(GfxError::CpBusy { what: "unhalt", .. }))), "H1 {}: {:?}", v.name(), r.error);
        kassert!(ok == r.left_running || v == Variant::E1a, "H1 {}: left running {}", v.name(), r.left_running);
        kassert!(r.left_running || (halted(owner.hw().hw()) && owner.hw().hw().gfx.mec_halted()), "H1 {}: not halted", v.name());
    }
    // The desk's sequence on one owner: e1a, e1b, e1c (each a later stage of the same boot), then the Quiesce halt.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let r1 = direct_v(d, &mut owner, &mut bo, &hub, None, Variant::E1a)?;
    kassert!(r1.passed() && !r1.left_running && !r1.halted_first && r1.rewind.is_none() && !r1.deactivated, "seq e1a: {:?}", r1.error);
    let r2 = direct_v(d, &mut owner, &mut bo, &hub, r1.bo, Variant::E1b)?;
    kassert!(r2.passed() && r2.left_running && !r2.halted_first && r2.rewind.is_none() && r2.reused_bo, "seq e1b: {:?}", r2.error);
    let r3 = direct_v(d, &mut owner, &mut bo, &hub, r1.bo, Variant::E1c)?;
    kassert!(r3.passed() && r3.left_running && r3.halted_first && r3.rewind.is_some() && !r3.deactivated, "seq e1c: {:?}", r3.error);
    // e1a after a running ring: halted first, CP_RB_ACTIVE set 0 before its unhalt, no pointer reset.
    let r4 = direct_v(d, &mut owner, &mut bo, &hub, r1.bo, Variant::E1a)?;
    kassert!(r4.passed() && r4.halted_first && r4.deactivated && r4.rewind.is_none(), "seq e1a again: {:?}", r4.error);
    kassert!(gfxring::render(&r4).iter().any(|l| l.starts_with("gfx: CP_RB_ACTIVE found 1")), "seq e1a again: no line");
    let r5 = direct_v(d, &mut owner, &mut bo, &hub, r1.bo, Variant::E1b)?;
    kassert!(r5.passed() && r5.left_running, "seq e1b again: {:?}", r5.error);
    kassert!(gfxring::halt(owner.hw().hw(), &a) && halted(owner.hw().hw()) && owner.hw().hw().gfx.mec_halted(), "quiesce halt");
    kassert!(gfxring::halt(owner.hw().hw(), &a), "quiesce halt again");
    // The Linux plans: e1a ends with the unhalt, e1b with CP_DEVICE_ID, e1c with the unhalt (GX-7j3: ring 0 adds
    // its three MQD pointer writes to e1b and e1c).
    for (r, n, last) in [(&r1, 173, R::CpMeCntl), (&r2, 196, R::CpDeviceId), (&r3, 196, R::CpMeCntl)] {
        let seq = gfxring::linux_sequence(r, &a);
        kassert!(seq.len() == n && seq.last().map(|x| x.0) == a.byte(last), "{} plan: {} writes", r.variant.name(), seq.len());
    }
    // The desk's check list on one owner: each variant followed by the GFX part of until=stop (CP and MEC halted,
    // the doorbell side put back), then the next variant as a later stage (again=true) of the same boot.
    let (mut owner, mut bo, hub) = after_gart(d)?;
    let mut first = None;
    let mut prev = None;
    for v in [Variant::E1a, Variant::E1b, Variant::E1c, Variant::E1a] {
        let what = format!("stop then {}", v.name());
        let r = direct_v(d, &mut owner, &mut bo, &hub, prev, v)?;
        kassert!(r.passed() && !r.halted_first && r.left_running == (v != Variant::E1a), "{what}: {:?}", r.error);
        prev = prev.or(r.bo);
        crate::psplive::keep_aper_first(&mut first, r.aper_found());
        kassert!(gfxring::halt(&mut *owner, &a), "{what}: stop halt");
        let (ok, lines) = gfxring::doorbell_back(&mut owner, &a, first);
        let f = owner.hw().hw();
        kassert!(ok && halted(f) && f.gfx.mec_halted() && f.gfx.get(R::CpRbDoorbellControl) & f::DOORBELL_EN == 0, "{what}: after the stop {lines:?}");
        kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "{what}: display touched");
    }
    // A bad name is no variant.
    kassert!(Variant::parse("e1d").is_none() && Variant::ALL.iter().all(|&v| Variant::parse(v.name()) == Some(v)), "parse");
    Ok(())
}

/// GX-7j2: the variants' failures (the chip's MEC that stays halted, a CP_STAT that flips for the whole 100 ms):
/// the CP and MEC end halted, the answer line is bounded, the tool's last line names the variant and says FAIL.
pub(crate) fn check_variant_failures(d: &Discovery) -> ktest::TestResult {
    // The chip's GX-7j failure (MEC stays halted): every order hangs in the unhalt with the chip's CP_STAT, the CP
    // and MEC halted again, no rung, the advice says the firmware is not RS64.
    for v in Variant::ALL {
        let what = format!("MEC ignored, {}", v.name());
        let (out, mut owner, _) = gfx_run_v(d, 512 << 20, StartAs::Cold, v, &|f| f.gfx.hostile = GfxHostile::MecIgnored)?;
        let r = report(&out)?;
        kassert!(!out.passed() && r.error == Some(GfxError::CpBusy { what: "unhalt", stat: Some(gfxfake::STUCK_STAT.1) }) && r.rungs.is_empty() && !r.left_running, "{what}: {:?}", r.error);
        kassert!(r.unhalt_stat == stuck_stat() && r.error.as_ref().is_some_and(|e| e.advice().contains("not RS64")), "{what}: {:x?}", r.unhalt_stat);
        let f = owner.hw().hw();
        kassert!(halted(f) && f.gfx.mec_halted() && f.gfx.cp_stat() == 0 && f.gfx.unhalts.last().is_some_and(|x| x.0 && x.2) && f.display_writes.is_empty(), "{what}: unhalts {:?}", f.gfx.unhalts);
        let text = gfxring::render(r);
        kassert!(text.iter().any(|l| l == &format!("gfx verdict ({}): FAIL", v.name())) && text.iter().any(|l| l.contains("(never 0)")), "{what}: {text:?}");
        let last = crate::psplive::gfx_verdict_line(v, crate::psplive::verdict(Until::GfxRing, out.passed(), true, true));
        kassert!(last == format!("gfx_ring verdict ({}): FAIL", v.name()), "{what}: {last}");
    }
    // A CP_STAT that flips at every poll for the whole 100 ms: the answer line keeps the first values and the
    // latest, counts the rest, and stays short; the CP and MEC end halted, the tool's last line says FAIL.
    for v in Variant::ALL {
        let what = format!("flapping CP_STAT, {}", v.name());
        let (out, mut owner, _) = gfx_run_v(d, 512 << 20, StartAs::Cold, v, &|f| f.gfx.hostile = GfxHostile::StatFlaps)?;
        let r = report(&out)?;
        kassert!(!out.passed() && matches!(r.error, Some(GfxError::CpBusy { what: "unhalt", .. })) && !r.left_running, "{what}: {:?}", r.error);
        kassert!(r.unhalt_stat.len() == gfxring::UNHALT_KEEP && r.unhalt_more > 1000, "{what}: kept {} more {}", r.unhalt_stat.len(), r.unhalt_more);
        let text = gfxring::render(r);
        let line = text.iter().find(|l| l.starts_with("gfx unhalt CP_STAT")).ok_or("no CP_STAT line")?;
        kassert!(line.len() < 4096 && line.contains(&format!("... {} more changes ...", r.unhalt_more)) && line.ends_with("(never 0) (Linux's capture: 0x88028000 ... 0x80008000, 0x0 after ~18 us)"), "{what}: {} bytes", line.len());
        let f = owner.hw().hw();
        kassert!(halted(f) && f.gfx.mec_halted() && f.display_writes.is_empty(), "{what}: not halted");
        let last = crate::psplive::gfx_verdict_line(v, crate::psplive::verdict(Until::GfxRing, out.passed(), true, true));
        kassert!(last == format!("gfx_ring verdict ({}): FAIL", v.name()), "{what}: {last}");
    }
    Ok(())
}

/// Linux's values from the capture's reads and writes, the byte addresses the capture names, the CSB and the
/// golden IB.
pub(crate) fn check_values() -> ktest::TestResult {
    kassert!(gfx11::rb0_cntl(gfx11::RING_DW) == 0x80a && gfx11::me_cntl(0x1500_0000, true) == 0x0100_0000 && gfx11::me_cntl(0x0100_0000, false) == 0x1500_0000, "RB0_CNTL / ME_CNTL");
    let r: Vec<u32> = gfx11::doorbell_ranges().iter().map(|x| x.1).collect();
    kassert!(r == [0x458, 0x7f8, 0, 0x450] && gfx11::DOORBELL_DW == 0x116, "doorbell ranges {r:x?}");
    kassert!(gfx11::doorbell_control(0, true) == 0x4000_0458 && gfx11::doorbell_control(0xc000_0458, false) == 0x8000_0458, "doorbell control");
    kassert!(gfx11::set_doorbell_range().map(|x| x.1) == [0x458, 0xffc], "set_doorbell range");
    // The golden registers: the capture's read -> write pairs (trace-load, Linux 7.1).
    let pairs: [(u32, u32); 10] = [(0x10, 0x10), (0x10, 0x10), (0xc200, 0xc200), (0xc0d4_0188, 0x00f0_0188), (0x0088_0007, 0x0088_0007), (0x8, 0x8), (0x5548_0100, 0x5548_0100), (0x0103_0000, 0x0103_0000), (0x0040_200a, 0xa), (0x039c_0020, 0x239c_0020)];
    for (g, (rd, wr)) in gfx11::GOLDEN.iter().zip(pairs) {
        kassert!(g.apply(rd) == wr, "{}: {rd:#x} -> {:#x}, Linux {wr:#x}", g.reg.name(), g.apply(rd));
    }
    kassert!(gfx11::SH_MEM_CONFIG == 0xc00c && gfx11::SH_MEM_BASES_GFX == 0x2000_1000 && gfx11::SH_MEM_BASES_KFD == 0x0001_0002, "SH_MEM");
    kassert!(gfx11::gfx_index(0, 0) == 0x4000_0000 && gfx11::gfx_index(0, 1) == 0x4000_0100 && gfx11::GFX_INDEX_BROADCAST == 0xe000_0000 && gfx11::grbm_vmid(9) == 0x90, "GRBM selects");
    let a = addrs(&crate::gpucore::fakechip::discovery_780m())?;
    // The capture's dword offsets.
    for (x, dw) in [
        (R::CpMeCntl, 0xa803),
        (R::ScratchReg0, 0xc040),
        (R::CpRbDoorbellRangeLower, 0x305a),
        (R::CpMecDoorbellRangeUpper, 0x305d),
        (R::RlcCsibAddrHi, 0xa988),
        (R::RlcSrmCntl, 0xec80),
        (R::CgttWdClkCtrl, 0xf086),
        (R::TcpCntl, 0xb9a2),
        (R::GrbmGfxCntl, 0xa900),
        (R::ShMemConfig, 0xa9e4),
        (R::SpiGdbgPerVmidCntl, 0x31d2),
        (R::CpMaxContext, 0x30ae),
        (R::CpDeviceId, 0x304b),
        (R::Gl2cCtrl3, 0xce0c),
        (R::GdsOa(15), 0x1260 + 0x20d0 + 15),
    ] {
        kassert!(a.dword(x) == Some(dw), "{}: {:x?} not {dw:#x}", x.name(), a.dword(x));
    }
    kassert!(a.aper_en == Some(0x3780), "aperture {:x?}", a.aper_en);
    // The CSB: 960 dwords, Linux's stream shape.
    let c = gfxring::csb();
    kassert!(c.len() == gfxring::CSB_DW as usize && c[..5] == [0xc000_4a00, 0x2000_0000, 0xc001_2800, 0x8000_0000, 0x8000_0000], "CSB head {:x?}", &c[..5.min(c.len())]);
    kassert!(c[c.len() - 7..] == [0xc001_6900, 0x0000_00d7, 0, 0xc000_4a00, 0x3000_0000, 0xc000_1200, 0], "CSB tail");
    let mut n = 0;
    let mut i = 5;
    while i < c.len() - 7 {
        let (p, len) = amdgpu_fmt::pm4::decode(&c[i..]).map_err(|e| format!("CSB packet at {i}: {e:?}"))?;
        kassert!(matches!(p, amdgpu_fmt::pm4::Packet::SetReg { space: amdgpu_fmt::pm4::RegSpace::Context, .. }), "CSB packet at {i} is no SET_CONTEXT_REG");
        i += len;
        n += 1;
    }
    kassert!(i == c.len() - 7 && n > 0, "CSB body");
    // The golden IB's RELEASE_MEM is Linux 6.6's fence (flush and invalidate everything, 64-bit data).
    kassert!(gfxring::GOLDEN_IB[4] == amdgpu_fmt::pm4::rm::GFX11_FENCE && gfxring::GOLDEN_IB[5] == 2 << 29, "golden IB fence bits");
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&crate::gpucore::fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.gfxring.starts", Safe, starts);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.failures", Safe, failures);

fn again() -> ktest::TestResult {
    check_again(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.again", Safe, again);

fn stop_back() -> ktest::TestResult {
    check_stop_back(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.stop_back", Safe, stop_back);

fn after_sdma() -> ktest::TestResult {
    check_after_sdma(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.after_sdma", Safe, after_sdma);

fn values() -> ktest::TestResult {
    check_values()
}
ktest::ktest!("gx.gfxring.values", Safe, values);
fn variants() -> ktest::TestResult {
    check_variants(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.variants", Safe, variants);
fn variant_failures() -> ktest::TestResult {
    check_variant_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.variant_failures", Safe, variant_failures);

/// GX-7j3: the CPG read of page 0 (the Mini-PC 29.10., fault 0x00000d33 on the direct path only): with the MQD
/// pointer lost the fake's CP reads page 0 and the stage fails on "GFX no new VM fault" alone; with it e1b and
/// e1c pass, CP_GFX_MQD_BASE_ADDR points at ring 0's MQD, whose memory says what the registers say. The fake was
/// taught the cause (`MqdPointerLost`), so this proves the fix only against that model: the Mini-PC check list
/// step 2 is the proof on the chip.
pub(crate) fn check_mqd_pointer(d: &Discovery) -> ktest::TestResult {
    let a = addrs(d)?;
    let (out, mut owner, _) = gfx_run(d, 512 << 20, StartAs::Cold, &|f| f.gfx.hostile = GfxHostile::MqdPointerLost)?;
    let r = report(&out)?;
    let bad: Vec<&str> = gfxring::checks(r).iter().filter(|c| !c.pass).map(|c| c.name).collect();
    kassert!(r.error.is_none() && r.rungs.iter().all(|x| x.pass) && bad == ["GFX no new VM fault"], "lost: {:?} {bad:?}", r.error);
    let f = owner.hw().hw();
    kassert!(!f.hubs.faults.is_empty() && f.hubs.faults.iter().all(|x| x.mc >> 12 == 0 && x.status >> 18 & 1 == 0 && x.status >> 9 & 0x1ff == gfxfake::CID_CPG), "lost: faults {:x?}", f.hubs.faults);
    for v in [Variant::E1b, Variant::E1c] {
        let (out, mut owner, _) = gfx_run_v(d, 512 << 20, StartAs::Cold, v, &|_| {})?;
        let r = report(&out)?;
        kassert!(r.passed() && out.passed(), "{}: {:?}", v.name(), r.error);
        let l = r.layout.ok_or("no layout")?;
        let f = owner.hw().hw();
        kassert!(f.hubs.faults.is_empty(), "{}: faults {:x?}", v.name(), f.hubs.faults);
        let got = |f: &mut FakeGpu, x: R| a.byte(x).and_then(|b| f.reg_read(b));
        kassert!(got(f, R::CpGfxMqdBaseAddr) == Some(l.mqd as u32) && got(f, R::CpGfxMqdBaseAddrHi) == Some((l.mqd >> 32) as u32), "{}: MQD pointer", v.name());
        kassert!(got(f, R::CpGfxMqdControl).is_some_and(|c| c & 0xf == 0 && c & 1 << 8 != 0), "{}: MQD control {:x?}", v.name(), got(f, R::CpGfxMqdControl));
        let mut m = alloc::vec![0u8; crate::ipmod::mes11::MQD_DW * 4];
        kassert!(f.mem_read(l.mqd, &mut m), "{}: MQD unreadable", v.name());
        kassert!(m == crate::ipmod::mesmqd::bytes(&gfxring::direct_mqd(&l, Some(gfx11::DOORBELL_DW))), "{}: the MQD differs from the registers", v.name());
        // GX-7j3 fixer: the rule still catches every other fault with the pointer set: a stray CP write at a
        // non-zero page, and a fault address that moved under an unchanged status.
        let (out, mut owner, _) = gfx_run_v(d, 512 << 20, StartAs::Cold, v, &|f| f.gfx.hostile = GfxHostile::StrayWrite)?;
        let x = report(&out)?;
        let f = owner.hw().hw();
        kassert!(!f.hubs.faults.is_empty() && f.hubs.faults.iter().all(|x| x.mc >> 12 != 0), "{} stray: faults {:x?}", v.name(), f.hubs.faults);
        kassert!(!x.passed() && gfxring::checks(x).iter().any(|c| c.name == "GFX no new VM fault" && !c.pass), "{} stray: fault rule not failed", v.name());
        let mut y = r.clone();
        y.fault_after = y.fault_after.map(|f| crate::gpucore::gmc::FaultRead { addr: Some(f.addr.unwrap_or(0) ^ 0x1000), ..f });
        kassert!(!y.passed() && gfxring::checks(&y).iter().any(|c| c.name == "GFX no new VM fault" && !c.pass), "{}: a moved fault address passed", v.name());
    }
    Ok(())
}

fn mqd_pointer() -> ktest::TestResult {
    check_mqd_pointer(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.mqd_pointer", Safe, mqd_pointer);

/// GX-7m2: when `gfx_ring again` restarts through a whole stop (`psplive::restart_by_stop`): only for gfx_ring with
/// again while the CP runs a direct ring. `gfxring::cp_running` on the fake: a passed e1b leaves the CP running,
/// the stop's halt halts it, GC off reads as not running.
pub(crate) fn check_restart_rule(d: &Discovery) -> ktest::TestResult {
    use crate::psplive::restart_by_stop;
    kassert!(restart_by_stop(Until::GfxRing, true, true), "gfx_ring again on a running ring");
    kassert!(!restart_by_stop(Until::GfxRing, false, true) && !restart_by_stop(Until::GfxRing, true, false), "without again, or a halted CP");
    for u in [Until::Gfx, Until::Gart, Until::Sdma, Until::Mes, Until::GfxMes, Until::Vm] {
        kassert!(!restart_by_stop(u, true, true), "{u:?}");
    }
    let a = addrs(d)?;
    let (_out, mut owner, _bo) = gfx_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    kassert!(gfxring::cp_running(&mut *owner, &a), "a passed e1b left the CP halted");
    kassert!(gfxring::halt(&mut *owner, &a) && !gfxring::cp_running(&mut *owner, &a), "running after the halt");
    let mut off = crate::gpucore::pspfake::FakeGpu::new(crate::gpucore::pspfake::Hostile::None);
    kassert!(!gfxring::cp_running(&mut off, &a), "GC off reads as running");
    Ok(())
}

fn restart_rule() -> ktest::TestResult {
    check_restart_rule(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gfxring.restart_rule", Safe, restart_rule);
