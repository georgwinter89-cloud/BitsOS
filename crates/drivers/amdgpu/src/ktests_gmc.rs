//! GX-7h in-kernel tests of the GMC stage (`gx_bringup {"until":"gart"}`) against the fakes: the same engine
//! (`pspbringup::run` with `Until::Gart`, `gpucore::gmc::enable`) over the same owner stack as the live run, for
//! three start states and carve-outs of 512 MiB, 8 GiB and 16 GiB; MMHUB stopped, written only where it differs
//! (context 0 last, the flush with its semaphore) or left equal; every failure; the table storage in the
//! carve-out under a 4-level VM. Nothing touches a device: Safe and rerunnable.

use crate::bo::{BoManager, DISCOVERY_RESERVE, Reserved};
use crate::gpucore::bringup::{Bringup, Gate, LOG_KEEP};
use crate::gpucore::carvetable::{CarveGart, CarveTables};
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::gmc::{self, GmcError, GmcInputs, GmcReport, MmDecision};
use crate::gpucore::hubfake::HubHostile;
use crate::gpucore::psp::Until;
use crate::gpucore::pspbringup::Outcome;
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::watch::{DisplayGuard, Mode, Watch};
use crate::ipmod::gmc11::{CPU_ENGINE, Fault, Hub, R};
use crate::ktests_bringup::{GOP, Owner, one_with};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::addr::Carveout;
use amdgpu_fmt::gart::{Gart, GartPage};
use amdgpu_fmt::pte::Target;
use ktest::kassert;

const SIZES: [u64; 3] = [512 << 20, 8 << 30, 16 << 30];

/// One `until=gart` run as the tool makes it (`setup` on the fake after the classifier read it).
pub(crate) fn gart(d: &Discovery, size: u64, s: StartAs, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager), String> {
    let (out, owner, bo, _) = one_with(d, size, s, Until::Gart, false, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, setup)?;
    Ok((out, owner, bo))
}

fn report(out: &Outcome) -> Result<&GmcReport, String> {
    out.gmc.as_ref().ok_or_else(|| format!("no GMC stage: gfx {:?}, checks {:?}", out.gfx.as_ref().map(|g| g.error), out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()))
}

/// Bytes of MMHUB registers the display uses (never written by the stage).
fn display_bytes(f: &FakeGpu) -> Vec<u32> {
    f.guard.named.iter().filter(|(_, c)| c.is_display()).map(|&(b, _)| b).collect()
}

/// What every passed run must show on the fake: the picture untouched, GFXHUB's VMID 0 on the table, the flush
/// acked, the semaphore free, the translation through GART and system aperture, a fault decoded.
fn check_run(out: &Outcome, owner: &mut Owner, size: u64, what: &str) -> ktest::TestResult {
    let r = report(out)?.clone();
    let failed: Vec<_> = out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect();
    kassert!(out.passed(), "{what}: {failed:?} {:?}", r.error);
    let g = r.geo.ok_or("no geometry")?;
    let f = owner.hw().hw();
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty() && out.violations_total == 0, "{what}: display {:?} scanout {:?}", f.display_writes, f.scanout_writes);
    let disp = display_bytes(f);
    kassert!(out.entries.iter().all(|e| !disp.contains(&e.byte)), "{what}: an MMHUB display field was written");
    // Any carve-out size: the aperture follows it, the table lies inside it, clear of the GOP and the discovery.
    kassert!(g.fb_end == f.fb_mc + size - 1 && g.gart_start == 0x7fff_0000_0000 && g.gart_end == 0x7fff_1fff_ffff, "{what}: {g:?}");
    let off = g.table_mc - f.fb_mc;
    kassert!(off >= GOP && off + gmc::table_bytes(gmc::LINUX_GART_SIZE) <= size - DISCOVERY_RESERVE && g.table_phys == f.fb_phys + off, "{what}: table at +{off:#x}");
    let pd = g.pd_addr();
    kassert!(pd == g.table_phys | 1, "{what}: pd_addr {pd:#x}");
    kassert!(f.hubs.get(Hub::Gfx, R::PtBaseLo(0)) == pd as u32 && f.hubs.get(Hub::Gfx, R::PtBaseHi(0)) == (pd >> 32) as u32 && f.hubs.get(Hub::Gfx, R::Ctx(0)) & 1 == 1, "{what}: GFXHUB context 0");
    kassert!(f.hubs.get(Hub::Gfx, R::ApertHigh) == (g.fb_end >> 18) as u32 && f.hubs.get(Hub::Gfx, R::L1Tlb) == 0x1859, "{what}: GFXHUB aperture/L1");
    kassert!(f.hubs.requests.iter().any(|&(h, e, v)| h == Hub::Gfx && e == CPU_ENGINE && v == 0x00f8_0001), "{what}: no GFXHUB flush {:?}", f.hubs.requests);
    kassert!(!f.hubs.sem_held(Hub::Mm, CPU_ENGINE) && !f.hubs.sem_held(Hub::Gfx, CPU_ENGINE), "{what}: a semaphore left taken");
    kassert!(f.gc.off_access.is_empty(), "{what}: GC touched while off {:?}", f.gc.off_access);
    live_lists_hold(f, &out.entries, what)?;
    // The GPU's view: the carve-out through the system aperture; the GART window unbound (a fault, decoded),
    // then a page bound through the table storage reaches it.
    kassert!(f.gpu_access(Hub::Gfx, f.fb_mc + 0x1000, false, 13) == Some(f.fb_phys + 0x1000), "{what}: system aperture");
    kassert!(f.gpu_access(Hub::Gfx, g.gart_start + 0x5000, true, 13).is_none(), "{what}: unbound GART page reached");
    let st = f.hubs.get(Hub::Gfx, R::FaultStatus);
    let fl = Fault::decode(st);
    kassert!(fl.client(Hub::Gfx) == Some("SDMA0") && fl.write && fl.permission == 1 && Fault::address(f.hubs.get(Hub::Gfx, R::FaultAddrLo), f.hubs.get(Hub::Gfx, R::FaultAddrHi)) == g.gart_start + 0x5000, "{what}: fault {}", fl.text(Hub::Gfx));
    let fault_text = gmc::read_fault(f, &crate::gpucore::gmc::HubAddrs::from_discovery(&crate::gpucore::fakechip::discovery_780m(), Hub::Gfx).ok_or("addrs")?).text();
    kassert!(fault_text.contains("client SDMA0") && fault_text.contains("write"), "{fault_text}");
    let carve = Carveout::new(f.fb_mc, f.fb_phys, size).ok_or("carve")?;
    let ga = Gart::new(g.gart_start, gmc::LINUX_GART_SIZE, g.dummy_phys, carve).map_err(|e| format!("{e:?}"))?;
    let target = f.fb_phys + (64 << 20);
    {
        let mut t = CarveGart::new(f, g.table_mc, g.entries());
        let mc = ga.map(&mut t, 0x7000, &[target], GartPage { target: Target::Local, writeable: true }).map_err(|e| format!("{e:?}"))?;
        kassert!(mc == g.gart_start + 0x7000 && !t.failed(), "{what}: map");
    }
    kassert!(f.gpu_access(Hub::Gfx, g.gart_start + 0x7123, false, 13) == Some(target + 0x123), "{what}: GART page not reached");
    let fw = crate::gpucore::ucode::FwSet::default();
    let out_text = crate::gpucore::pspbringup::render(out, &crate::ktests_bringup::dummy_inputs(&fw));
    kassert!(out_text.iter().any(|l| l.starts_with("gmc: graphics memory")) && out_text.iter().any(|l| l.starts_with("GMC GFXHUB context 0: PASS")), "{what}: answer lines");
    Ok(())
}

/// The live owner's lists (`GmcAddrs::readable/writable`) hold every hub register the stage read or wrote.
fn live_lists_hold(f: &FakeGpu, entries: &[crate::gpucore::bringup::Entry], what: &str) -> ktest::TestResult {
    let a = gmc::GmcAddrs::from_discovery(&crate::gpucore::fakechip::discovery_780m()).ok_or("addrs")?;
    let (r, w) = (a.readable(), a.writable(&f.guard));
    let hub = |b: u32| a.gfx.reg_at(b).is_some() || a.mm.reg_at(b).is_some() || f.hubs.owns(b).is_some();
    let bad_w: Vec<u32> = entries.iter().map(|e| e.byte).filter(|&b| hub(b) && !w.contains(&b)).collect();
    // Context 0's table base is read back by the stage after the run; the fault registers by the answer.
    let bad_r: Vec<u32> = f.hubs.read_set.iter().copied().filter(|b| !r.contains(b)).collect();
    kassert!(bad_w.is_empty() && bad_r.is_empty(), "{what}: not on the live lists: writes {bad_w:x?} reads {bad_r:x?}");
    Ok(())
}

/// Every start state that admits a GC stage, at three carve-out sizes: MMHUB is never written (a display field
/// differs: the firmware's L1/L2/context 0 or, after Linux, its table base), GFXHUB carries the GART.
pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    for size in SIZES {
        for s in [StartAs::Cold, StartAs::AfterLinux, StartAs::AfterBitsOs] {
            let what = format!("{s:?} {} MiB", size >> 20);
            let (out, mut owner, _bo) = gart(d, size, s, &|_| {})?;
            check_run(&out, &mut owner, size, &what)?;
            let r = report(&out)?;
            kassert!(matches!(&r.mm, Some(MmDecision::Stop(l)) if !l.is_empty() && l.iter().all(|p| p.display)) && r.mm_written == 0, "{what}: {:?}", r.mm);
            kassert!(!r.flushes.iter().any(|f| f.hub == Hub::Mm), "{what}: MMHUB flushed");
            if s == StartAs::Cold {
                let names: Vec<String> = match &r.mm { Some(MmDecision::Stop(l)) => l.iter().map(|p| p.r.name(Hub::Mm)).collect(), _ => Vec::new() };
                for n in ["MMMC_VM_MX_L1_TLB_CNTL", "MMVM_L2_CNTL", "MMVM_CONTEXT0_CNTL", "MMVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32"] {
                    kassert!(names.iter().any(|x| x == n), "{what}: {n} not among {names:?}");
                }
            }
        }
    }
    Ok(())
}

/// MMHUB's display fields already as Linux would set them: only the GART fields that differ are written,
/// context 0's last, then the flush with the semaphore (each write allowed once by the watcher); with every
/// field equal nothing is written.
pub(crate) fn check_mmhub(d: &Discovery) -> ktest::TestResult {
    let size = 512 << 20;
    let (first, ..) = gart(d, size, StartAs::Cold, &|_| {})?;
    let plan = report(&first)?.mm_plan.clone();
    let disp: Vec<(u32, u32)> = plan.iter().filter(|p| p.display).map(|p| (p.byte, p.value)).collect();
    let (out, mut owner, _bo) = gart(d, size, StartAs::Cold, &|f| disp.iter().for_each(|&(b, v)| f.set_reg(b, v)))?;
    check_run(&out, &mut owner, size, "mmhub write")?;
    let r = report(&out)?;
    let Some(MmDecision::Write(list)) = &r.mm else { return Err(format!("not written: {:?}", r.mm)) };
    kassert!(!list.is_empty() && list.iter().all(|p| !p.display && p.differs()) && r.mm_written == list.len(), "{list:?}");
    let ctx0 = |r: R| matches!(r, R::Ctx(0) | R::PtBaseLo(0) | R::PtBaseHi(0) | R::PtStartLo(0) | R::PtStartHi(0) | R::PtEndLo(0) | R::PtEndHi(0));
    let first_ctx0 = list.iter().position(|p| ctx0(p.r)).ok_or("context 0 not written")?;
    kassert!(list[first_ctx0..].iter().all(|p| ctx0(p.r)), "context 0 not last: {:?}", list.iter().map(|p| p.r).collect::<Vec<_>>());
    kassert!(!list.iter().any(|p| p.r == R::L2Cntl2), "the L2 invalidate action with an equal L2");
    let mm = gmc::HubAddrs::from_discovery(d, Hub::Mm).ok_or("mm")?;
    let (req, sem) = (mm.byte(R::Req(CPU_ENGINE)).ok_or("req")?, mm.byte(R::Sem(CPU_ENGINE)).ok_or("sem")?);
    let mm_entries: Vec<(u32, u32)> = out.entries.iter().filter(|e| list.iter().any(|p| p.byte == e.byte) || e.byte == req || e.byte == sem).map(|e| (e.byte, e.value)).collect();
    let want: Vec<(u32, u32)> = list.iter().map(|p| (p.byte, p.value)).chain([(req, 0x00f8_0001), (sem, 0)]).collect();
    kassert!(mm_entries == want, "MMHUB writes {mm_entries:x?}");
    kassert!(out.entries.iter().find(|e| e.byte == sem).is_some_and(|e| e.read_back.is_none()), "the semaphore was read back (that takes it again)");
    let f = owner.hw().hw();
    kassert!(f.hubs.sem_taken == 1 && f.hubs.requests.iter().any(|&(h, e, _)| h == Hub::Mm && e == CPU_ENGINE), "semaphore {} requests {:?}", f.hubs.sem_taken, f.hubs.requests);
    kassert!(f.hubs.get(Hub::Mm, R::PtStartLo(0)) == 0xfff0_0000 && f.hubs.get(Hub::Mm, R::PtStartHi(0)) == 7, "MMHUB GART window");
    let granted = owner.hw().granted().to_vec();
    kassert!(granted.len() == 2 && granted[0].0 == req && granted[1] .0 == sem && owner.hw().pending_allowed() == 0, "{granted:?}");
    // Every MMHUB register equal: nothing written to MMHUB, no MMHUB flush.
    let all: Vec<(u32, u32)> = plan.iter().map(|p| (p.byte, p.value)).collect();
    let (out, mut owner, _bo) = gart(d, size, StartAs::Cold, &|f| all.iter().for_each(|&(b, v)| f.set_reg(b, v)))?;
    check_run(&out, &mut owner, size, "mmhub equal")?;
    let r = report(&out)?;
    kassert!(r.mm == Some(MmDecision::Equal) && r.mm_written == 0 && !r.flushes.iter().any(|f| f.hub == Hub::Mm), "{:?}", r.mm);
    kassert!(!out.entries.iter().any(|e| mm.reg_at(e.byte).is_some()), "an MMHUB write");
    // A semaphore never free: Linux flushes anyway (gmc_v11_0.c:229-230); the stage says so.
    let (out, mut owner, _bo) = gart(d, size, StartAs::Cold, &|f| {
        disp.iter().for_each(|&(b, v)| f.set_reg(b, v));
        f.hubs.hostile = HubHostile::SemBusy(Hub::Mm);
    })?;
    check_run(&out, &mut owner, size, "semaphore busy")?;
    kassert!(report(&out)?.flushes.iter().any(|f| f.hub == Hub::Mm && f.sem == Some(false) && f.acked), "{:?}", report(&out)?.flushes);
    Ok(())
}

/// The GMC stage straight on a fake (no PSP or GC stage before), for the refusals.
fn bare(d: &Discovery, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<GmcReport>, Box<Owner>, BoManager), String> {
    let size = 512 << 20;
    let mut f = FakeGpu::with_carve(d, Hostile::None, size);
    setup(&mut f);
    let carve = Carveout::new(f.fb_mc, f.fb_phys, size).ok_or("carve")?;
    let inp = GmcInputs::new(d, carve).ok_or("no hub addresses")?;
    let mut guard = DisplayGuard::from_discovery(d);
    guard.vram_mc = Some(f.fb_mc);
    guard.protect(f.fb_mc, GOP);
    let reserved = alloc::vec![
        Reserved { name: String::from("gop"), range: 0..GOP },
        Reserved { name: String::from("discovery"), range: size - DISCOVERY_RESERVE..size },
    ];
    let mut bo = BoManager::new(size, reserved);
    let mut owner = Box::new(Bringup::new(Watch::new(f, guard, Mode::Block), Gate::Allow, LOG_KEEP));
    let r = Box::new(gmc::enable(&mut *owner, &mut bo, &inp));
    Ok((r, owner, bo))
}

fn hub_writes(owner: &mut Owner, d: &Discovery) -> usize {
    let a = gmc::GmcAddrs::from_discovery(d);
    owner.hw().hw().writes.iter().filter(|&&(b, _)| a.is_some_and(|a| a.gfx.reg_at(b).is_some() || a.mm.reg_at(b).is_some())).count()
}

/// Every refusal before a write, every failure after one, and the table of an earlier run reused.
pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    // GC off: GFXHUB reads all ones; nothing written, not even the table.
    let (r, mut owner, bo) = bare(d, &|_| {})?;
    kassert!(matches!(r.error, Some(GmcError::GcOff { .. })) && r.table.is_none() && bo.count() == 0 && hub_writes(&mut owner, d) == 0, "{:?}", r.error);
    kassert!(gmc::render(&r).iter().any(|l| l.contains("run the GFX stage first")), "advice");
    let on = |f: &mut FakeGpu| f.gc.set_booted();
    let (r, mut owner, _) = bare(d, &on)?;
    kassert!(r.error.is_none() && r.passed() && hub_writes(&mut owner, d) > 0, "{:?}", r.error);
    // GFXHUB's FB copy different: refused; read 0: copied from MMHUB's (Linux's SR-IOV branch).
    let gx = gmc::HubAddrs::from_discovery(d, Hub::Gfx).ok_or("gx")?;
    let (base, off) = (gx.byte(R::FbBase).ok_or("b")?, gx.byte(R::FbOffset).ok_or("o")?);
    let (r, mut owner, _) = bare(d, &|f| {
        on(f);
        f.set_reg(off, 0x123);
    })?;
    kassert!(matches!(r.error, Some(GmcError::FbMismatch { what: "FB_OFFSET", .. })) && hub_writes(&mut owner, d) == 0, "{:?}", r.error);
    let top = gx.byte(R::FbTop).ok_or("t")?;
    let (r, mut owner, _) = bare(d, &|f| {
        on(f);
        f.set_reg(base, 0);
        f.set_reg(top, 0);
    })?;
    kassert!(r.passed() && r.gfx_fb_copied && owner.hw().hw().hubs.get(Hub::Gfx, R::FbBase) == owner.hw().hw().hubs.get(Hub::Mm, R::FbBase), "{:?}", r.error);
    let entries: Vec<_> = owner.log().recent().copied().collect();
    live_lists_hold(owner.hw().hw(), &entries, "FB copy")?;
    // No ack from GFXHUB: the stage stops before MMHUB.
    let (r, mut owner, _) = bare(d, &|f| {
        on(f);
        f.hubs.hostile = HubHostile::NoAck(Hub::Gfx);
    })?;
    kassert!(r.error == Some(GmcError::Flush { hub: Hub::Gfx, what: "ack" }) && r.mm.is_none() && r.mm_plan.is_empty() && !r.passed(), "{:?}", r.error);
    kassert!(r.flushes.first().is_some_and(|f| !f.acked && f.after_us >= gmc::LINUX_FLUSH_US), "{:?}", r.flushes);
    let _ = &mut owner;
    // The table cannot be written (memory refused): no hub register written.
    let (r, mut owner, _) = bare(d, &|f| {
        on(f);
        f.hostile = Hostile::MemFail;
    })?;
    kassert!(matches!(r.error, Some(GmcError::TableWrite { .. } | GmcError::TableReadBack { .. })) && hub_writes(&mut owner, d) == 0, "{:?}", r.error);
    // A second run of the boot reuses the table: same place, same CONTEXT0_PAGE_TABLE_BASE.
    let (r1, mut owner, mut bo) = bare(d, &on)?;
    let carve = Carveout::new(owner.hw().hw().fb_mc, owner.hw().hw().fb_phys, 512 << 20).ok_or("carve")?;
    let mut inp = GmcInputs::new(d, carve).ok_or("addrs")?;
    inp.table = r1.table;
    let n = bo.count();
    let r2 = gmc::enable(&mut owner, &mut bo, &inp);
    kassert!(r2.passed() && r2.reused_table && r2.geo.map(|g| g.pd_addr()) == r1.geo.map(|g| g.pd_addr()) && bo.count() == n, "{:?}", r2.error);
    kassert!(r2.enabled_before(Hub::Gfx) == Some(true), "context 0 on before the second run");
    Ok(())
}

/// The carve-out table storage under GR-1's 4-level VM: tables in carve-out pages, entries reached by the
/// software walker through the fake's memory; the budget and freeing.
pub(crate) fn check_tables(d: &Discovery) -> ktest::TestResult {
    use amdgpu_fmt::pte::{MapFlags, Mtype};
    use amdgpu_fmt::store::{TableRead, TableStore};
    use amdgpu_fmt::vm::{Backing, Vm};
    let size = 512 << 20;
    let mut f = FakeGpu::with_carve(d, Hostile::None, size);
    let carve = Carveout::new(f.fb_mc, f.fb_phys, size).ok_or("carve")?;
    let mut s = CarveTables::new(&mut f, carve, (32 << 20)..(33 << 20)).ok_or("store")?;
    let budget = s.available();
    kassert!(budget == 256, "{budget}");
    let mut vm = Vm::new(&mut s, carve).map_err(|e| format!("{e:?}"))?;
    let flags = MapFlags { read: true, write: true, exec: false, mtype: Mtype::Uc, tmz: false, noalloc: false };
    vm.map(&mut s, 0x1_0000_0000, 0x2000, Backing::Local { phys: f_phys(&carve, 100 << 20) }, flags).map_err(|e| format!("{e:?}"))?;
    let w = amdgpu_fmt::walk::walk(&s, vm.root().phys, 0x1_0000_1234).map_err(|e| format!("{e:?}"))?;
    let leaf = match w {
        amdgpu_fmt::walk::Walk::Page(l) => l,
        other => return Err(format!("{other:?}")),
    };
    kassert!(leaf.phys == f_phys(&carve, 100 << 20) + 0x1234 && !s.failed() && s.available() == budget - vm.table_count(), "{leaf:?} {}", s.available());
    kassert!(s.read(vm.root().phys, 512).is_none() && CarveTables::new(&mut FakeGpu::new(Hostile::None), carve, 1..4096).is_none(), "bounds");
    vm.destroy(&mut s);
    kassert!(s.available() == budget, "freed {}", s.available());
    Ok(())
}

fn f_phys(c: &Carveout, off: u64) -> u64 {
    c.phys_of(off).unwrap_or(0)
}

/// The fault decode on Linux's fields and client tables.
pub(crate) fn check_decode() -> ktest::TestResult {
    let gfx = Fault::decode(13 << 9 | 1 << 18 | 1 << 4 | 3 << 20 | 1);
    kassert!(gfx.client(Hub::Gfx) == Some("SDMA0") && gfx.write && gfx.vmid == 3 && gfx.more && gfx.permission == 1, "{gfx:?}");
    let t = gfx.text(Hub::Gfx);
    kassert!(t.contains("SDMA0") && t.contains("VMID 3") && t.contains("MORE_FAULTS 1"), "{t}");
    let mm = Fault::decode(6 << 9 | 1 << 8);
    kassert!(mm.client(Hub::Mm) == Some("MP0") && !mm.write && mm.mapping, "{mm:?}");
    kassert!(Fault::decode(15 << 9 | 1 << 18).client(Hub::Mm) == Some("OSSSYS") && Fault::decode(15 << 9).client(Hub::Mm).is_none(), "RW picks the table");
    kassert!(Fault::address(0xfff0_0005, 0x7) == 0x7fff_0000_5000 && Fault::decode(0).text(Hub::Mm) == "no fault recorded", "address");
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&crate::gpucore::fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.gmc.starts", Safe, starts);

fn mmhub() -> ktest::TestResult {
    check_mmhub(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gmc.mmhub", Safe, mmhub);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.gmc.failures", Safe, failures);

fn tables() -> ktest::TestResult {
    check_tables(&crate::gpucore::fakechip::discovery_780m())?;
    check_decode()
}
ktest::ktest!("gx.gmc.tables", Safe, tables);
