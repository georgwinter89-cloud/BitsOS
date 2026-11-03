//! GX-7i in-kernel tests of the SDMA stage (`gx_bringup {"until":"sdma"}`) against the fakes: the same engine
//! (`pspbringup::run` with `Until::Sdma`, `gpucore::sdma::enable`) over the same owner stack as the live run, the
//! rungs executed by GR-2's interpreter inside the chip fake (`sdmafake`), for three start states and carve-outs
//! of 512 MiB, 8 GiB and 16 GiB; every failure the fake can stage (a dead engine, no writeback, MMIO wptr
//! ignored, doorbells dropped, a dword byte-fill, GC gated, a lost GART flush); a second stage on a running
//! engine; the Quiesce halt. Nothing touches a device: Safe and rerunnable.

use crate::bo::BoManager;
use crate::gpucore::carvetable::CarveGart;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::hubfake::HubHostile;
use crate::gpucore::psp::Until;
use crate::gpucore::pspbringup::{self, Outcome};
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::sdma::{self, SdmaAddrs, SdmaError, SdmaInputs, SdmaReport, SysAlloc};
use crate::gpucore::sdmafake::SdmaHostile;
use crate::gpucore::ucode::ListOpts;
use crate::ipmod::gmc11::Hub;
use crate::ipmod::sdma6::{self, R, f};
use crate::ktests_bringup::{GOP, Owner, one_with};
use alloc::boxed::Box;
use alloc::format;
use core::cell::Cell;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::gart::GartRead;
use ktest::kassert;

const SIZES: [u64; 3] = [512 << 20, 8 << 30, 16 << 30];

/// One `until=sdma` run as the tool makes it (`setup` on the fake after the classifier read it).
pub(crate) fn sdma_run(d: &Discovery, size: u64, s: StartAs, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager), String> {
    let (out, owner, bo, _) = one_with(d, size, s, Until::Sdma, false, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, setup)?;
    Ok((out, owner, bo))
}

pub(crate) fn report(out: &Outcome) -> Result<&SdmaReport, String> {
    out.sdma.as_ref().ok_or_else(|| format!("no SDMA stage: gmc {:?}, checks {:?}", out.gmc.as_ref().map(|g| g.error), out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()))
}

fn addrs(d: &Discovery) -> Result<SdmaAddrs, String> {
    SdmaAddrs::from_discovery(d).ok_or_else(|| String::from("no SDMA addresses"))
}

/// What every passed run must show on the fake: every rung through the interpreter, the doorbell taken, the
/// engine running, the GART pages unbound again, the picture untouched, every write on the live owner's lists.
fn check_run(d: &Discovery, out: &Outcome, owner: &mut Owner, size: u64, what: &str) -> ktest::TestResult {
    let r = report(out)?.clone();
    let failed: Vec<_> = out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect();
    kassert!(out.passed(), "{what}: {failed:?} {:?}", r.error);
    kassert!(r.rungs.len() == sdma::RUNGS.len() && r.rungs.iter().all(|x| x.pass && x.done) && r.left_running, "{what}: rungs {:?}", r.rungs);
    kassert!(r.resumes.len() == 2 && !r.resumes[0].0 && r.resumes[1].0, "{what}: resumes {:?}", r.resumes);
    // The ring's pointers as the desk's check list gives them (dwords; the writeback is in bytes).
    let w: Vec<u64> = r.rungs.iter().map(|x| x.wptr).collect();
    kassert!(w == [16, 32, 48, 80, 16] && r.rungs.iter().all(|x| x.rptr == Some(x.wptr << 2)), "{what}: wptrs {w:?} {:?}", r.rungs.iter().map(|x| x.rptr).collect::<Vec<_>>());
    let l = r.layout.ok_or("no layout")?;
    let a = addrs(d)?;
    let f = owner.hw().hw();
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty() && out.violations_total == 0, "{what}: display {:?} scanout {:?}", f.display_writes, f.scanout_writes);
    kassert!(f.gc.off_access.is_empty(), "{what}: GC touched while off {:?}", f.gc.off_access);
    // Any carve-out size: the buffer inside it, clear of the GOP and the discovery reserve.
    let off = l.bo_mc - f.fb_mc;
    kassert!(off >= GOP && off + sdma::BO_BYTES <= size - crate::bo::DISCOVERY_RESERVE, "{what}: buffer at +{off:#x}");
    // The engine wrote only into its buffer and the two system pages (never the GOP surface).
    let bo_phys = f.fb_phys + off..f.fb_phys + off + sdma::BO_BYTES;
    let sys: Vec<u64> = r.gart.as_ref().map(|g| g.phys.clone()).unwrap_or_default();
    let stray: Vec<_> = f.sdma.gpu_writes.iter().filter(|&&(p, n)| !(bo_phys.contains(&p) && p + n as u64 <= bo_phys.end) && !sys.iter().any(|&s| p >= s && p + n as u64 <= s + 4096)).collect();
    kassert!(stray.is_empty(), "{what}: engine writes outside its buffer {stray:x?}");
    kassert!(f.sdma.running() && f.sdma.doorbells.iter().filter(|x| x.2).count() == 1, "{what}: running {} doorbells {:?}", f.sdma.running(), f.sdma.doorbells);
    kassert!(f.sdma.get(R::Doorbell) & f::DOORBELL_ENABLE != 0 && f.sdma.get(R::DoorbellOffset) == 0x800, "{what}: doorbell regs");
    let nb = a.nbio.ok_or("no nbio")?;
    kassert!(f.sdma.port.get(&nb.csdma_range) == Some(&0x14_0800) && f.reg(nb.aper_en) & 1 == 1, "{what}: NBIO {:x?}", f.sdma.port);
    // The 64-bit fence moved both halves; the GART window is empty again.
    let mut b = [0u8; 8];
    kassert!(crate::gpucore::psp::Hw::mem_read(f, l.fence64, &mut b) && u64::from_le_bytes(b) == sdma::FENCE64_START + 1, "{what}: fence64 {:x?}", b);
    let g = out.gmc.as_ref().and_then(|g| g.geo).ok_or("no GART")?;
    let gp = r.gart.as_ref().ok_or("no GART pages")?;
    kassert!(gp.mc == g.gart_start && gp.unbound && !gp.leaked && gp.flushes.len() == 2 && gp.flushes.iter().all(|x| x.acked), "{what}: {gp:?}");
    {
        let t = CarveGart::new(f, g.table_mc, g.entries());
        kassert!(t.read(0) == Some(g.dummy_phys) && t.read(1) == Some(g.dummy_phys), "{what}: GART entries not unbound");
    }
    kassert!(f.hubs.faults.is_empty(), "{what}: VM faults {:?}", f.hubs.faults);
    // Every SDMA/NBIO write of the run is on the live owner's lists.
    let w = a.writable();
    let bad: Vec<u32> = out.entries.iter().filter(|e| a.name_at(e.byte).is_some() && !w.contains(&e.byte) && !nb.index_ok(e.byte, e.value)).map(|e| e.byte).collect();
    kassert!(bad.is_empty(), "{what}: writes off the live lists {bad:x?}");
    let fw = crate::gpucore::ucode::FwSet::default();
    let text = pspbringup::render(out, &crate::ktests_bringup::dummy_inputs(&fw));
    kassert!(text.iter().any(|l| l.starts_with("SDMA rung 4 doorbell: PASS")) && text.iter().any(|l| l.contains("sdma SDMA golden IB (capture): ok")), "{what}: answer lines");
    kassert!(text.iter().any(|l| l.starts_with("# sdma resume 2 (doorbell on")), "{what}: no resume mark");
    Ok(())
}

pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    for size in SIZES {
        for s in [StartAs::Cold, StartAs::AfterLinux, StartAs::AfterBitsOs] {
            let what = format!("{s:?} at {size:#x}");
            let (out, mut owner, _) = sdma_run(d, size, s, &|_| {})?;
            check_run(d, &out, &mut owner, size, &what)?;
            let r = report(&out)?;
            kassert!(r.stopped_first == (s == StartAs::AfterLinux), "{what}: stopped first {}", r.stopped_first);
        }
    }
    Ok(())
}

/// A passed `until=gart` run and the SDMA inputs for a direct stage on its owner.
fn after_gart(d: &Discovery) -> Result<(Box<Owner>, BoManager, sdma::GartUse), String> {
    let (out, mut owner, bo) = crate::ktests_gmc::gart(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let gu = out.gmc.as_ref().zip(crate::gpucore::gmc::GmcInputs::new(d, carve(&mut owner)?).as_ref()).and_then(|(g, m)| pspbringup::gart_use(g, m)).ok_or("no GART")?;
    Ok((owner, bo, gu))
}

fn carve(owner: &mut Owner) -> Result<amdgpu_fmt::addr::Carveout, String> {
    let f = owner.hw().hw();
    amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, f.fb_size).ok_or_else(|| String::from("carve"))
}

fn direct(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, gu: &sdma::GartUse, prev: Option<sdma::SdmaBo>) -> Result<SdmaReport, String> {
    direct_armed(d, owner, bo, gu, prev).map(|x| x.0)
}

/// The same, with the arm hook counting: (report, times armed, the owner's write count when armed).
fn direct_armed(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, gu: &sdma::GartUse, prev: Option<sdma::SdmaBo>) -> Result<(SdmaReport, u32), String> {
    let sys = owner.hw().hw().sdma.sys_alloc();
    let armed = Cell::new(0u32);
    let arm = |_: SdmaAddrs| armed.set(armed.get() + 1);
    let inp = SdmaInputs { bo: prev, sys: Some(&sys as &dyn SysAlloc), arm: Some(&arm), ..SdmaInputs::new(d, carve(owner)?, false).ok_or("inputs")? };
    let r = sdma::enable(owner, bo, &inp, gu);
    Ok((r, armed.get()))
}

/// No GART entry of rung 3's window left on a system page.
fn gart_empty(out_geo: crate::gpucore::gmc::Geometry, f: &mut FakeGpu) -> bool {
    let t = CarveGart::new(f, out_geo.table_mc, out_geo.entries());
    (0..sdma::SYS_PAGES).all(|i| t.read(i) == Some(out_geo.dummy_phys))
}

pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    // Each hostile: the failing rung named, the engine halted again, nothing left bound in the GART.
    for (h, rung, err) in [
        (SdmaHostile::Dead, 0, "Timeout"),
        (SdmaHostile::NoWriteback, 0, "Timeout"),
        (SdmaHostile::IgnoreMmioWptr, 0, "Timeout"),
        (SdmaHostile::FillByteIsDword, 1, "Result"),
        (SdmaHostile::NoDoorbell, 4, "Timeout"),
        (SdmaHostile::IbRefused, 2, "Timeout"),
        (SdmaHostile::IbWriteLost, 2, "Result"),
        (SdmaHostile::CopyCorrupt, 3, "Result"),
        (SdmaHostile::FenceLowHalfOnly, 3, "Timeout"),
    ] {
        let what = format!("{h:?}");
        let (out, mut owner, _) = sdma_run(d, 512 << 20, StartAs::Cold, &|f| f.sdma.hostile = h)?;
        let r = report(&out)?;
        kassert!(!out.passed() && r.rungs.len() == rung + 1 && !r.rungs[rung].pass && r.rungs[..rung].iter().all(|x| x.pass), "{what}: rungs {:?}", r.rungs);
        kassert!(r.error.as_ref().is_some_and(|e| format!("{e:?}").starts_with(err)), "{what}: error {:?}", r.error);
        kassert!(!r.left_running && !owner.hw().hw().sdma.running() && owner.hw().hw().sdma.get(R::F32Cntl) & f::F32_HALT != 0, "{what}: engine not halted");
        kassert!(r.gart.as_ref().is_none_or(|g| g.unbound && !g.leaked), "{what}: GART pages {:?}", r.gart);
        kassert!((rung >= 3) == r.gart.is_some(), "{what}: GART pages bound by rung 3 only: {:?}", r.gart);
        let geo = out.gmc.as_ref().and_then(|g| g.geo).ok_or("no GART")?;
        kassert!(gart_empty(geo, owner.hw().hw()), "{what}: a GART entry still on a system page");
        if h == SdmaHostile::FenceLowHalfOnly {
            kassert!(r.rungs[3].fence == Some(sdma::FENCE64_START & !0xffff_ffff), "{what}: fence {:x?}", r.rungs[3].fence);
        }
        if h == SdmaHostile::IbWriteLost {
            kassert!(r.rungs[2].detail.contains("reads cafedead"), "{}", r.rungs[2].detail);
        }
        if h == SdmaHostile::FillByteIsDword {
            kassert!(r.rungs[1].detail.contains("fillsize 0 is a DWORD fill"), "{}", r.rungs[1].detail);
        }
        kassert!(out.checks.iter().any(|c| c.name == sdma::RUNGS[rung] && !c.pass), "{what}: no failed check");
    }
    // A stray engine write (GFXHUB records a VM fault): every rung passes, the stage fails on the fault rule and
    // the engine is halted again.
    let (out, mut owner, _) = sdma_run(d, 512 << 20, StartAs::Cold, &|f| f.sdma.hostile = SdmaHostile::StrayWrite)?;
    let r = report(&out)?;
    kassert!(r.rungs.len() == sdma::RUNGS.len() && r.rungs.iter().all(|x| x.pass) && !owner.hw().hw().hubs.faults.is_empty(), "stray: rungs {:?}", r.rungs);
    kassert!(!out.passed() && !r.passed() && !r.left_running && !owner.hw().hw().sdma.running(), "stray: passed or left running");
    kassert!(out.checks.iter().any(|c| c.name == "SDMA no new VM fault" && !c.pass), "stray: fault rule not failed");
    // The F32 ignores HALT and rung 3's fence never completes: the halt is not proven, so the pages are kept for
    // the boot (never freed) although their entries are unbound; the GART rule fails.
    let (out, mut owner, _) = sdma_run(d, 512 << 20, StartAs::Cold, &|f| {
        f.sdma.hostile = SdmaHostile::FenceLowHalfOnly;
        f.sdma.ignore_halt = true;
    })?;
    let r = report(&out)?;
    let gp = r.gart.as_ref().ok_or("no halt: no GART pages")?;
    kassert!(!out.passed() && r.rungs.len() == 4 && !r.rungs[3].pass && gp.leaked && gp.unbound, "no halt: {:?} {gp:?}", r.rungs);
    kassert!(r.rungs[3].detail.contains("not proven halted"), "{}", r.rungs[3].detail);
    kassert!(out.checks.iter().any(|c| c.name == "SDMA GART pages unbound again" && !c.pass), "no halt: GART rule not failed");
    let geo = out.gmc.as_ref().and_then(|g| g.geo).ok_or("no GART")?;
    kassert!(gart_empty(geo, owner.hw().hw()), "no halt: a GART entry still on a system page");
    // The pass rules on a passed report: a changed fault register or kept pages fail the stage.
    let (out, ..) = sdma_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let ok = report(&out)?.clone();
    kassert!(ok.passed(), "pass rules: base run {:?}", ok.error);
    let mut x = ok.clone();
    x.fault_after = x.fault_after.map(|f| crate::gpucore::gmc::FaultRead { status: Some(0x100), ..f });
    kassert!(!x.passed() && sdma::checks(&x).iter().any(|c| c.name == "SDMA no new VM fault" && !c.pass), "fault rule");
    let mut x = ok.clone();
    if let Some(g) = x.gart.as_mut() {
        g.leaked = true;
    }
    kassert!(!x.passed() && sdma::checks(&x).iter().any(|c| c.name == "SDMA GART pages unbound again" && !c.pass), "GART rule");
    // GC gated: refused before any SDMA write, and the Quiesce halt not armed.
    let (mut owner, mut bo, gu) = after_gart(d)?;
    owner.hw().hw().gc.power = crate::gpucore::gcfake::Power::Off;
    let n0 = owner.log().totals().writes;
    let (r, armed) = direct_armed(d, &mut owner, &mut bo, &gu, None)?;
    kassert!(matches!(r.error, Some(SdmaError::GcOff { .. })) && owner.log().totals().writes == n0 && r.layout.is_none(), "GC off: {:?}", r.error);
    kassert!(armed == 0, "GC off: armed {armed}");
    // GFXHUB never acks the flush after binding rung 3's pages: they are kept, never freed; the engine halted.
    let (mut owner, mut bo, gu) = after_gart(d)?;
    owner.hw().hw().hubs.hostile = HubHostile::NoAck(Hub::Gfx);
    let r = direct(d, &mut owner, &mut bo, &gu, None)?;
    kassert!(r.error == Some(SdmaError::Flush) && r.gart.as_ref().is_some_and(|g| g.leaked) && r.rungs.len() == 3 && !owner.hw().hw().sdma.running(), "lost flush: {:?} {:?}", r.error, r.gart);
    // No system pages: rung 3 fails, the engine halted.
    let (mut owner, mut bo, gu) = after_gart(d)?;
    let inp = SdmaInputs { sys: None, ..SdmaInputs::new(d, carve(&mut owner)?, false).ok_or("inputs")? };
    let r = sdma::enable(&mut owner, &mut bo, &inp, &gu);
    kassert!(matches!(r.error, Some(SdmaError::NoSys(_))) && !owner.hw().hw().sdma.running(), "no sys: {:?}", r.error);
    Ok(())
}

/// A second stage on the running engine (`again=true`): stopped first, the buffer reused, every rung again;
/// then the Quiesce halt.
pub(crate) fn check_again(d: &Discovery) -> ktest::TestResult {
    let (mut owner, mut bo, gu) = after_gart(d)?;
    let (r1, armed) = direct_armed(d, &mut owner, &mut bo, &gu, None)?;
    kassert!(r1.passed() && !r1.stopped_first && !r1.reused_bo, "first: {:?}", r1.error);
    // Armed once, before the stage's first write (the first resume's writes come after it).
    kassert!(armed == 1, "armed {armed}");
    let r2 = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
    kassert!(r2.passed() && r2.stopped_first && r2.reused_bo && r2.layout == r1.layout, "again: {:?} {:?}", r2.error, r2.rungs);
    kassert!(owner.hw().hw().sdma.running(), "not running after again");
    let a = addrs(d)?;
    // The Quiesce halt while a run holds the PSP lock: raw registers only, no wait (atomic mode), nothing when
    // not armed.
    let t0 = owner.hw().hw().now;
    let n0 = owner.log().totals().writes;
    let (none, ok) = crate::psplive::with_live_held(|_| (crate::psplive::quiesce_halt(None, owner.hw()), crate::psplive::quiesce_halt(Some(&a), owner.hw())));
    kassert!(none.is_none() && ok == Some(true), "quiesce halt: {none:?} {ok:?}");
    kassert!(owner.hw().hw().now == t0 && owner.log().totals().writes == n0, "quiesce halt waited or went through the owner's log");
    let f = owner.hw().hw();
    kassert!(!f.sdma.running() && f.sdma.get(R::RbCntl) & f::RB_ENABLE == 0 && f.sdma.get(R::IbCntl) & f::IB_ENABLE == 0 && f.sdma.get(R::F32Cntl) & f::F32_HALT != 0, "halt left it running");
    // Linux's plan from what the first stage found is the capture's sequence (the fixture test checks the values).
    let seq = sdma::linux_sequence(&r1, &a);
    kassert!(seq.len() == 29 && seq.first().map(|x| x.0) == a.byte(R::F32Cntl) && seq.last().map(|x| x.0) == a.byte(R::IbCntl), "linux sequence {}", seq.len());
    Ok(())
}

/// GX-7i2: the restart in one boot on a fake that keeps the old pointers (the Mini-PC 28.10.: `again` found
/// DOORBELL 0x50000000 and RB_RPTR = RB_WPTR = 0x40 survived the resume). With CAPTURED cleared by a 0 write or by
/// a 1 write, the reset brings both pointers to 0; with RB_RPTR never writable the queue takes the kept value over;
/// a kept value off the ring's alignment fails as StalePointers with the engine halted.
pub(crate) fn check_restart(d: &Discovery) -> ktest::TestResult {
    for (w1c, keep) in [(false, false), (true, false), (false, true)] {
        let what = format!("CAPTURED w1c {w1c}, RPTR kept {keep}");
        let (mut owner, mut bo, gu) = after_gart(d)?;
        {
            let f = &mut owner.hw().hw().sdma;
            f.captured_w1c = w1c;
            if keep {
                f.hostile = SdmaHostile::KeepRptr;
            }
        }
        let r1 = direct(d, &mut owner, &mut bo, &gu, None)?;
        kassert!(r1.passed(), "{what}: first {:?} {:?}", r1.error, r1.rewinds);
        let (db, rp, wp) = {
            let f = &owner.hw().hw().sdma;
            (f.get(R::Doorbell), f.get(R::RbRptr), f.get(R::RbWptr))
        };
        kassert!(db & f::DOORBELL_CAPTURED != 0 && db & f::DOORBELL_ENABLE != 0, "{what}: doorbell after rung 4 {db:#x}");
        kassert!(keep || (rp == 0x40 && wp == 0x40), "{what}: pointers after rung 4 {rp:#x} {wp:#x}");
        // The fake keeps the pointers while CAPTURED is set: a bare write of 0 (the resume's) is lost.
        if !keep {
            let b = addrs(d)?.byte(R::RbRptr).ok_or("RB_RPTR")?;
            let f = &mut owner.hw().hw().sdma;
            f.write(b, 0);
            kassert!(f.get(R::RbRptr) == 0x40 && f.lost.last() == Some(&(b, 0)), "{what}: the fake took RB_RPTR 0 with CAPTURED set");
        }
        let seq = klog::next_seq();
        let r2 = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
        kassert!(r2.passed() && r2.stopped_first && r2.rungs.iter().all(|x| x.pass), "{what}: again {:?} {:?} {:x?}", r2.error, r2.rungs, r2.rewinds);
        // The kept RB_RPTR is a warn fallback (B6); a reset that took is no fallback.
        let kept_ev = klog::read_since(seq, 4096).iter().any(|e| e.name == "sdma.ptr_kept" && e.level == klog::Level::Warn);
        kassert!(kept_ev == keep, "{what}: sdma.ptr_kept warn {kept_ev}");
        let w = r2.rewinds.first().ok_or("again: no restart recorded")?;
        kassert!(w.found.2 & f::DOORBELL_CAPTURED != 0 && w.after.2 & (f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED) == 0, "{what}: doorbell {:x?}", w);
        if keep {
            kassert!(w.found.0 != 0 && w.after.0 == w.found.0 && w.start == w.found.0 && w.after.1 == 0, "{what}: {w:x?}");
            kassert!(r2.rungs[0].wptr == w.start / 4 + 16, "{what}: rung 1 wptr {}", r2.rungs[0].wptr);
        } else {
            kassert!(w.found.0 == 0x40 && w.found.1 == 0x40 && w.after == (0, 0, w.after.2) && w.start == 0, "{what}: {w:x?}");
            kassert!(r2.rungs[0].wptr == 16, "{what}: rung 1 wptr {}", r2.rungs[0].wptr);
        }
        // Rung 4's restart inside each stage reset the pointers too (resume 2 begins at the start).
        kassert!(r2.rewinds.len() == 2 && r2.resumes.len() == 2, "{what}: rewinds {:x?}", r2.rewinds);
        kassert!(sdma::render(&r2).iter().any(|l| l.starts_with("sdma restart: found RB_RPTR")), "{what}: no restart line");
    }
    // A kept RB_RPTR off the ring's alignment: StalePointers, the engine halted again, nothing run.
    let (mut owner, mut bo, gu) = after_gart(d)?;
    owner.hw().hw().sdma.hostile = SdmaHostile::KeepRptr;
    let r1 = direct(d, &mut owner, &mut bo, &gu, None)?;
    kassert!(r1.passed(), "odd: first {:?}", r1.error);
    owner.hw().hw().sdma.set(R::RbRptr, 0x44);
    let r2 = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
    kassert!(matches!(r2.error, Some(SdmaError::StalePointers { rptr: 0x44, .. })) && r2.rungs.is_empty() && !owner.hw().hw().sdma.running(), "odd: {:?} {:?}", r2.error, r2.rungs);
    kassert!(sdma::render(&r2).iter().any(|l| l.contains("StalePointers") && l.contains("sdma restart")), "odd: no advice line");
    // A reset only partly taken: RB_RPTR goes to 0 but RB_WPTR keeps the earlier 0x40 through the reset and the
    // resume: StalePointers after the resume, the engine halted again, nothing run.
    let (mut owner, mut bo, gu) = after_gart(d)?;
    let r1 = direct(d, &mut owner, &mut bo, &gu, None)?;
    kassert!(r1.passed(), "wptr: first {:?}", r1.error);
    {
        let f = &mut owner.hw().hw().sdma;
        f.captured_w1c = false;
        f.hostile = SdmaHostile::KeepWptr;
    }
    let r2 = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
    let w = r2.rewinds.first().ok_or("wptr: no restart recorded")?;
    kassert!(w.found == (0x40, 0x40, w.found.2) && w.after.0 == 0 && w.after.1 == 0x40, "wptr: {w:x?}");
    kassert!(matches!(r2.error, Some(SdmaError::StalePointers { rptr: 0, wptr: 0x40 })) && r2.rungs.is_empty() && !owner.hw().hw().sdma.running(), "wptr: {:?} {:?}", r2.error, r2.rungs);
    {
        let f = &owner.hw().hw().sdma;
        kassert!(f.get(R::RbCntl) & f::RB_ENABLE == 0 && f.get(R::F32Cntl) & f::F32_HALT != 0, "wptr: not halted again");
    }
    kassert!(sdma::render(&r2).iter().any(|l| l.contains("StalePointers") && l.contains("sdma restart")), "wptr: no advice line");
    Ok(())
}

/// GX-7i2: what a stop puts back after the halt (`sdma::doorbell_back`, as until=stop does): the queue doorbell
/// off, NBIO's CSDMA range and the doorbell aperture as the first stage found them; rerunnable without writes;
/// the classifier reads its own trace before the Linux signs; a later stage starts again from there.
pub(crate) fn check_stop_back(d: &Discovery) -> ktest::TestResult {
    use crate::gpucore::classify::{self, Own, Start};
    use crate::gpucore::psp::Hw as _;
    let a = addrs(d)?;
    let nb = a.nbio.ok_or("no nbio")?;
    let (mut owner, mut bo, gu) = after_gart(d)?;
    let r1 = direct(d, &mut owner, &mut bo, &gu, None)?;
    let found = r1.nbio_found().ok_or("NBIO not read")?;
    kassert!(r1.passed() && found.aper & 1 == 0 && r1.turned_aperture_on(), "first: {:?} {:x?}", r1.error, r1.nbio);
    {
        let f = owner.hw().hw();
        // The 28.10. probe after a stop: the aperture on reads as Linux without BitsOS's own trace.
        kassert!(classify::classify_traced(d, &mut |b| f.reg_read(b), None, Own::default()).start == Start::AfterLinux, "no trace: not after Linux");
        let st = classify::classify_traced(d, &mut |b| f.reg_read(b), None, Own { doorbell_aperture: true });
        kassert!(st.start != Start::AfterLinux && st.why.iter().any(|w| w.contains("this boot's SDMA or GFX ring stage turned it on")), "own trace: {:?} {:?}", st.start, st.why);
    }
    // psplive's keeping (as a stage does): the first stage's found values; a second stage before any stop finds the
    // aperture this driver turned on and must not replace them.
    let mut kept = None;
    kassert!(crate::psplive::keep_nbio_found(&mut kept, &r1) && kept == Some(found), "kept after the first stage: {kept:x?}");
    let ra = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
    kassert!(ra.passed() && ra.nbio_found().is_some_and(|x| x.aper & 1 != 0), "second stage: {:?} {:x?}", ra.error, ra.nbio);
    kassert!(crate::psplive::keep_nbio_found(&mut kept, &ra) && kept == Some(found), "kept after the second stage: {kept:x?}");
    kassert!(sdma::halt(&mut *owner, &a), "halt");
    // The stop as until=stop wires it (psplive::put_doorbell_back): unknown found values leave all as it is.
    let n0 = owner.log().totals().writes;
    let mut lines = Vec::new();
    kassert!(!crate::psplive::put_doorbell_back(&mut owner, &a, None, &mut lines) && owner.log().totals().writes == n0, "unknown found: wrote");
    kassert!(lines.len() == 1 && lines[0].contains("as found is unknown"), "unknown found: {lines:?}");
    let mut lines = Vec::new();
    kassert!(crate::psplive::put_doorbell_back(&mut owner, &a, kept, &mut lines), "stop did not clear the aperture trace: {lines:?}");
    kassert!(lines.last().is_some_and(|l| l.starts_with("stop: SDMA 0's doorbell side back as found")), "stop line: {lines:?}");
    kassert!(lines.iter().any(|l| l.contains("RCC_DEV0_EPF0_0_RCC_DOORBELL_APER_EN back to")) && lines.iter().any(|l| l.contains("GDC0_BIF_CSDMA_DOORBELL_RANGE back to")), "back: {lines:?}");
    {
        let f = owner.hw().hw();
        let db = f.sdma.get(R::Doorbell);
        kassert!(f.reg(nb.aper_en) == found.aper && f.sdma.port.get(&nb.csdma_range).copied().unwrap_or(0) == found.range && db & (f::DOORBELL_ENABLE | f::DOORBELL_CAPTURED) == 0, "not back: aper {:#x} doorbell {db:#x} port {:x?}", f.reg(nb.aper_en), f.sdma.port);
        kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "display touched");
        kassert!(classify::classify_traced(d, &mut |b| f.reg_read(b), None, Own::default()).start != Start::AfterLinux, "aperture off: still after Linux");
    }
    // Rerunnable: a second stop writes nothing.
    let n0 = owner.log().totals().writes;
    let (ok, lines) = sdma::doorbell_back(&mut owner, &a, found);
    kassert!(ok && owner.log().totals().writes == n0 + 1, "second stop wrote {} (only the RSMU index read)", owner.log().totals().writes - n0);
    kassert!(lines.iter().filter(|l| l.contains("as found") || l.contains("already off")).count() == 3, "{lines:?}");
    // A stage after the stop starts again (its found values are the first stage's) and passes.
    let r2 = direct(d, &mut owner, &mut bo, &gu, r1.bo)?;
    kassert!(r2.passed() && r2.nbio_found() == Some(found), "after the stop: {:?} {:x?}", r2.error, r2.nbio);
    Ok(())
}

/// Linux's values from the capture's reads, the doorbell numbers, the burst NOP rule, the fill checks.
pub(crate) fn check_values() -> ktest::TestResult {
    kassert!(sdma6::rb_cntl_size(0x4_0800, sdma6::RING_DW) == 0x84_0816 && sdma6::rb_cntl_run(0x84_0816, true) == 0x84_1817, "RB_CNTL");
    kassert!(sdma6::utcl1_cntl(0x1f00_0288) == 0x1f00_0689 && sdma6::utcl1_page(0x010c_ec20) == 0x010c_ec20, "UTCL1");
    kassert!(sdma6::f32_cntl(0x0808_4001, true) == 0x0808_4000 && sdma6::f32_cntl(0x0808_4000, false) == 0x0808_4001, "F32");
    // GX-7i2: a halted engine that ran (0x08084601) and a stopped ring (0x00841816) give Linux's cold values; the
    // queue doorbell never writes CAPTURED back; the restart's doorbell off ends with ENABLE and CAPTURED 0.
    kassert!(sdma6::f32_cntl(0x0808_4601, true) == 0x0808_4000 && sdma6::rb_cntl_size(0x0084_1816, sdma6::RING_DW) == 0x84_0816 && sdma6::rb_cntl_size(0x0084_1817, sdma6::RING_DW) == 0x84_0816, "resume 2 values");
    kassert!(sdma6::doorbell(0x5000_0000, false) == 0 && sdma6::doorbell(0x5000_0000, true) == 0x1000_0000 && sdma6::doorbell_off_steps(0x5000_0000) == [0x4000_0000, 0], "doorbell off");
    kassert!(sdma6::watchdog(0, sdma6::LINUX_TIMEOUT_US) == 1 && sdma6::doorbell(0, true) == 0x1000_0000 && sdma6::doorbell_offset(0, sdma6::DOORBELL_DW) == 0x800, "doorbell");
    kassert!(sdma6::csdma_range(0, sdma6::DOORBELL_DW, sdma6::DOORBELL_RANGE) == 0x14_0800, "CSDMA range");
    let d = crate::gpucore::fakechip::discovery_780m();
    let a = addrs(&d)?;
    // The capture's bytes (dword offsets x4): F32_CNTL 0xf89a, RB_CNTL 0x12e0, the RSMU window 0x14/0x15, the
    // CSDMA range's RSMU index 0x1403bec.
    let nb = a.nbio.ok_or("nbio")?;
    kassert!(a.byte(R::F32Cntl) == Some(0xf89a * 4) && a.byte(R::RbCntl) == Some(0x12e0 * 4) && a.byte(R::WptrPollLo) == Some(0x1313 * 4), "SDMA bytes");
    kassert!(nb.rsmu_index == 0x14 * 4 && nb.rsmu_data == 0x15 * 4 && nb.csdma_range == 0x0140_3bec && nb.aper_en == 0x3780, "NBIO {nb:x?}");
    let (ok, t) = sdma::check_byte_fill(Some(&[0xa5; 16]));
    kassert!(ok, "{t}");
    let (ok, t) = sdma::check_byte_fill(Some(&[0xa5, 0, 0, 0, 0xa5, 0, 0, 0]));
    kassert!(!ok && t.contains("DWORD fill"), "{t}");
    kassert!(!sdma::burst_nop(&crate::gpucore::ucode::FwSet::default()), "no SDMA file: no burst NOP");
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&crate::gpucore::fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.sdma.starts", Safe, starts);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.sdma.failures", Safe, failures);

fn again() -> ktest::TestResult {
    check_again(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.sdma.again", Safe, again);

fn restart() -> ktest::TestResult {
    check_restart(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.sdma.restart", Safe, restart);

fn stop_back() -> ktest::TestResult {
    check_stop_back(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.sdma.stop_back", Safe, stop_back);

fn values() -> ktest::TestResult {
    check_values()
}
ktest::ktest!("gx.sdma.values", Safe, values);
