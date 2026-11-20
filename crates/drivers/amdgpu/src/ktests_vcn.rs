//! GX-8v in-kernel tests of the boot's VCN stage against the fakes: a kept `until=sdma` run with VCN's firmware in
//! the PSP's list (BitsOS's list), then `gpucore::vcn::enable` on the same owner stack (`Bringup<Watch<..>>` in
//! `Mode::Block`) and PSP, the VCPU modelled by `vcnfake`: Linux's order (NBIO range, PowerUpVcn, the CPU's DPG
//! writes, the SRAM list through the PSP, the ring, MMHUB's contexts 1-7 and engine 0's range), no packet on the ring
//! (no self-test), no display field touched; each failure the fake can stage ends with VCN stopped and powered down;
//! stop and start again; the Quiesce halt. Nothing touches a device: Safe.

use crate::bo::BoManager;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::psp::{Psp, Until};
use crate::gpucore::pspbringup::{GfxInputs, Outcome};
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::smu::SmuTimeouts;
use crate::gpucore::smufake::SmuHostile;
use crate::gpucore::ucode::{ListOpts, UcodeId};
use crate::gpucore::vcn::{self, VcnAddrs, VcnError, VcnInputs, VcnReport};
use crate::gpucore::vcnfake::VcnHostile;
use crate::gpucore::vcnvm;
use crate::ipmod::psp13::Mailbox13;
use crate::ipmod::smu13::msg;
use crate::ipmod::vcn4::{self, R};
use crate::ktests_bringup::{GOP, Owner, one_with};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::kassert;

/// GB_ADDR_CONFIG of the 780M (the capture's UVD/JPEG writes of it).
pub(crate) const GB_ADDR_CONFIG: u32 = 0x242;

/// A kept chain with BitsOS's firmware list (`until=sdma`: SDMA's stage turned the doorbell aperture on).
pub(crate) struct Kept {
    pub out: Box<Outcome>,
    pub owner: Box<Owner>,
    pub bo: BoManager,
    pub psp: Psp<Mailbox13>,
    pub mm: Vec<u32>,
}

pub(crate) fn kept(d: &Discovery, size: u64, setup: &dyn Fn(&mut FakeGpu)) -> Result<Kept, String> {
    let opts = ListOpts::PSP13_0_4;
    let (mut out, owner, bo, _) = one_with(d, size, StartAs::Cold, Until::Sdma, true, Hostile::None, &opts, &[], GOP, setup)?;
    kassert!(out.passed(), "the chain: {:?} {:?}", out.error, out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>());
    let psp = out.kept.take().ok_or("no kept PSP")?;
    Ok(Kept { out, owner, bo, psp, mm: vcn::mm_display_bytes(d) })
}

pub(crate) fn inputs<'a>(d: &Discovery, f: &FakeGpu, rep: &crate::gpucore::psp::Report, mm: &'a [u32]) -> Result<VcnInputs<'a>, String> {
    let carve = amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, f.fb_size).ok_or("carve")?;
    Ok(VcnInputs {
        addrs: VcnAddrs::from_discovery(d).ok_or("no VCN addresses")?,
        smu: GfxInputs::from_discovery(d).ok_or("no SMU")?.smu,
        smu_t: SmuTimeouts::LINUX,
        carve,
        gb_addr_config: GB_ADDR_CONFIG,
        fw_tmr: vcn::fw_tmr(rep),
        drm_key: true,
        timeout_us: vcn4::LINUX_TIMEOUT_US,
        mm_display: mm,
        mm: crate::gpucore::gmc::HubAddrs::from_discovery(d, crate::ipmod::gmc11::Hub::Mm).ok_or("no MMHUB")?,
        bo: None,
        arm: None,
    })
}

/// The stage on a kept chain (`bo`: an earlier run's buffer): its report, and the owner's write count when it began.
pub(crate) fn stage_with(d: &Discovery, k: &mut Kept, bo: Option<vcn::VcnBo>, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<VcnReport>, u64), String> {
    setup(k.owner.hw().hw());
    let seq0 = k.owner.log().totals().writes;
    let mut inp = inputs(d, k.owner.hw().hw(), &k.out.report, &k.mm)?;
    inp.bo = bo;
    let r = vcn::enable(&mut k.owner, &mut k.bo, &mut k.psp, &inp);
    Ok((Box::new(r), seq0))
}

pub(crate) fn stage(d: &Discovery, k: &mut Kept, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<VcnReport>, u64), String> {
    stage_with(d, k, None, setup)
}

/// What every passed stage shows on the fake.
fn check_pass(d: &Discovery, k: &mut Kept, r: &VcnReport, seq0: u64, what: &str) -> ktest::TestResult {
    kassert!(r.passed(), "{what}: {:?}", r.error);
    // No self-test: nothing on the ring, the job half begins at 0.
    kassert!(r.wptr == 0, "{what}: wptr {:#x}", r.wptr);
    kassert!(r.sram.len() == 27 && r.sram_status == Some(0) && r.doorbell, "{what}: sram {} status {:?} doorbell {}", r.sram.len(), r.sram_status, r.doorbell);
    let a = VcnAddrs::from_discovery(d).ok_or("addrs")?;
    let f = k.owner.hw().hw();
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty(), "{what}: display {:?} scanout {:?}", f.display_writes, f.scanout_writes);
    kassert!(f.vcn.errors.is_empty() && f.vcn.ibs.is_empty() && f.vcn.traps == 0 && f.vcn.fences.is_empty(), "{what}: VCPU {:?} {:?} traps {}", f.vcn.errors, f.vcn.ibs, f.vcn.traps);
    // What the VCPU needs to run its ring is all there (the fake's own rule, vcn_v4_0.c's start).
    kassert!(f.vcn_booted(), "{what}: the VCPU would not run its ring");
    let f = k.owner.hw().hw();
    kassert!(f.vcn.off_access.is_empty() && f.order_errors.is_empty(), "{what}: gated {:x?} order {:?}", f.vcn.off_access, f.order_errors);
    kassert!(f.smu.vcn_on && !f.smu.jpeg_on, "{what}: VCN powered {} JPEG {}", f.smu.vcn_on, f.smu.jpeg_on);
    // GX-D9: MMHUB's writes are the contexts 1-7 and engine 0's range only, every display field as before.
    let entries: Vec<_> = k.owner.log().since(seq0).copied().collect();
    let f = k.owner.hw().hw();
    let hub = crate::gpucore::gmc::HubAddrs::from_discovery(d, crate::ipmod::gmc11::Hub::Mm).ok_or("MMHUB")?;
    let ctx = vcnvm::writable(&hub);
    let mm: Vec<u32> = entries.iter().filter(|e| f.hubs.owns(e.byte) == Some(crate::ipmod::gmc11::Hub::Mm)).map(|e| e.byte).collect();
    kassert!(!mm.is_empty() && mm.iter().all(|b| ctx.contains(b)), "{what}: MMHUB writes {mm:x?}");
    kassert!(r.contexts.len() == 7 * 3 + 2 && r.mm_before == r.mm_after && !r.mm_before.is_empty(), "{what}: contexts {}", r.contexts.len());
    // Every VCN register written is on the live owner's list.
    let wl = a.writable();
    let bad: Vec<u32> = entries.iter().filter(|e| a.name_at(e.byte).is_some() && !wl.contains(&e.byte)).map(|e| e.byte).collect();
    kassert!(bad.is_empty(), "{what}: VCN writes off the list {bad:x?}");
    // The firmware's order: VCN (13) after RLC_G and its autoload.
    let types: Vec<u32> = f.loaded.iter().map(|x| x.fw_type).collect();
    kassert!(types.last() == Some(&UcodeId::Vcn.fw_type()), "{what}: load order {types:?}");
    kassert!(f.vcn.sram.as_ref().is_some_and(|s| s.len() == 27), "{what}: the VCPU's SRAM");
    order(d, &entries, f, what)
}

/// Linux's order on the owner's log: the NBIO range, PowerUpVcn, UVD_POWER_STATUS (twice), the PSP's SRAM load,
/// the ring's registers, the doorbell control.
fn order(d: &Discovery, e: &[crate::gpucore::bringup::Entry], f: &FakeGpu, what: &str) -> ktest::TestResult {
    let a = VcnAddrs::from_discovery(d).ok_or("addrs")?;
    let n = a.nbio.ok_or("nbio")?;
    let smu = GfxInputs::from_discovery(d).ok_or("smu")?.smu;
    let at = |p: &dyn Fn(&crate::gpucore::bringup::Entry) -> bool| e.iter().position(p);
    let range = at(&|x| x.byte == n.rsmu_data);
    let up = at(&|x| x.byte == smu.msg && x.value == msg::POWER_UP_VCN);
    let ps = at(&|x| Some(x.byte) == a.byte(R::PowerStatus));
    let sram = at(&|x| x.byte == f.a.c2pmsg_67);
    let base = at(&|x| Some(x.byte) == a.byte(R::RbBaseLo));
    let db = at(&|x| Some(x.byte) == a.byte(R::Rb1DbCtrl));
    let o = [range, up, ps, sram, base, db];
    kassert!(o.iter().all(Option::is_some) && o.windows(2).all(|w| w[0] < w[1]), "{what}: order {o:?}");
    let first_vcn = at(&|x| a.name_at(x.byte).is_some());
    kassert!(first_vcn > up, "{what}: a VCN register written before PowerUpVcn ({first_vcn:?} {up:?})");
    let ps_w: Vec<u32> = e.iter().filter(|x| Some(x.byte) == a.byte(R::PowerStatus)).map(|x| x.value).collect();
    kassert!(ps_w == [0x1, 0x105], "{what}: UVD_POWER_STATUS writes {ps_w:x?} (the capture: 0x1, 0x105)");
    let db_w = e.iter().find(|x| Some(x.byte) == a.byte(R::Rb1DbCtrl)).map(|x| x.value);
    kassert!(db_w == Some(0x4000_0c48), "{what}: VCN_RB1_DB_CTRL {db_w:x?}");
    Ok(())
}

pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    for size in [512u64 << 20, 8 << 30] {
        let what = format!("{size:#x}");
        let mut k = kept(d, size, &|_| {})?;
        let (r, seq0) = stage(d, &mut k, &|_| {})?;
        check_pass(d, &mut k, &r, seq0, &what)?;
        let text = vcn::render(&r);
        kassert!(text.iter().any(|l| l.contains("MMHUB registers written by this stage: 23 ")) && !text.iter().any(|l| l.contains("rung")), "{what}: lines {text:?}");
    }
    Ok(())
}

/// The video messages are only on their own list; the bring-up and running lists never carry them.
pub(crate) fn check_lists(d: &Discovery) -> ktest::TestResult {
    let mut k = kept(d, 512 << 20, &|_| {})?;
    let f = k.owner.hw().hw();
    kassert!(f.loaded.iter().any(|x| x.fw_type == UcodeId::Vcn.fw_type()) && f.vcn.fw_loaded, "VCN firmware not loaded with the chain");
    let smu_a = GfxInputs::from_discovery(d).ok_or("smu")?.smu;
    let w0 = k.owner.log().totals().writes;
    let e = crate::gpucore::smu::send_video(&mut *k.owner, &smu_a, SmuTimeouts::LINUX, msg::POWER_UP_VCN, 1);
    kassert!(matches!(e, Err(crate::gpucore::smu::SmuError::Forbidden { .. })) && k.owner.log().totals().writes == w0, "PowerUpVcn for instance 1: {e:?}");
    for id in [msg::POWER_UP_VCN, msg::POWER_DOWN_VCN, msg::POWER_UP_JPEG, msg::POWER_DOWN_JPEG] {
        kassert!(!crate::gpucore::smu::allowed(id, 0) && !crate::gpucore::smu::allowed_running(id, 0) && crate::gpucore::smu::allowed_video(id, 0) && !crate::gpucore::smu::allowed_video(id, 1), "{id:#x} lists");
    }
    Ok(())
}

/// Each failure the fakes stage ends with VCN stopped and powered down, the PSP's ring still answering.
pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    let mut k = kept(d, 512 << 20, &|_| {})?;
    let (r, _) = stage(d, &mut k, &|f| f.vcn.hostile.push(VcnHostile::SramRefused))?;
    kassert!(matches!(r.error, Some(VcnError::SramStatus(_))) && !r.passed() && !r.left_running, "SRAM refused: {:?}", r.error);
    let s = r.stop.as_ref().ok_or("no stop")?;
    let f = k.owner.hw().hw();
    kassert!(s.ok() && !f.smu.vcn_on, "SRAM refused: stop {s:?}");
    kassert!(!k.psp.wedged() && f.display_writes.is_empty(), "SRAM refused: PSP wedged {} display {:?}", k.psp.wedged(), f.display_writes);
    // PMFW refuses PowerUpVcn: not one VCN register is written.
    let mut k = kept(d, 512 << 20, &|_| {})?;
    let (r, seq0) = stage(d, &mut k, &|f| f.smu.hostile.push(SmuHostile::Answer { id: msg::POWER_UP_VCN, code: 0xff }))?;
    let a = VcnAddrs::from_discovery(d).ok_or("addrs")?;
    let vcn_w = k.owner.log().since(seq0).filter(|e| a.name_at(e.byte).is_some()).count();
    kassert!(matches!(r.error, Some(VcnError::Smu(_))) && vcn_w == 0 && r.stop.is_none(), "PowerUpVcn refused: {:?}, {vcn_w} VCN writes", r.error);
    // A PSP run that did not load VCN's firmware: refused before anything is written.
    let (r, seq0) = {
        let mut inp = inputs(d, k.owner.hw().hw(), &k.out.report, &k.mm)?;
        inp.fw_tmr = None;
        let seq0 = k.owner.log().totals().writes;
        (vcn::enable(&mut k.owner, &mut k.bo, &mut k.psp, &inp), seq0)
    };
    let n = k.owner.log().totals().writes - seq0;
    kassert!(r.error == Some(VcnError::NoFirmware) && n == 0 && r.power_up.is_none(), "no firmware: {:?}, {n} writes", r.error);
    Ok(())
}

/// A stop, then the stage again on the same buffer; the Quiesce halt powers VCN down.
pub(crate) fn check_again(d: &Discovery) -> ktest::TestResult {
    let mut k = kept(d, 512 << 20, &|_| {})?;
    let (r, _) = stage(d, &mut k, &|_| {})?;
    kassert!(r.passed(), "first: {:?}", r.error);
    let a = VcnAddrs::from_discovery(d).ok_or("addrs")?;
    let smu = GfxInputs::from_discovery(d).ok_or("smu")?.smu;
    let s = vcn::stop(&mut k.owner, &a, r.layout.as_ref(), &smu, SmuTimeouts::LINUX, vcn4::LINUX_TIMEOUT_US);
    kassert!(s.idle && s.ok() && !k.owner.hw().hw().smu.vcn_on, "stop: {s:?}");
    let ps = k.owner.log().recent().filter(|e| Some(e.byte) == a.byte(R::PowerStatus)).last().map(|e| e.value);
    kassert!(ps == Some(0x101), "the stop's UVD_POWER_STATUS {ps:x?} (the capture: 0x101)");
    let (r2, _) = stage_with(d, &mut k, r.bo, &|_| {})?;
    kassert!(r2.passed() && r2.reused_bo && r2.wptr == 0, "again: {:?} wptr {:#x}", r2.error, r2.wptr);
    let f = k.owner.hw().hw();
    kassert!(vcn::halt(f, &a, &smu) && !f.smu.vcn_on, "the Quiesce halt");
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vcn.starts", Safe, starts);

fn lists() -> ktest::TestResult {
    check_lists(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vcn.lists", Safe, lists);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vcn.failures", Safe, failures);

fn again() -> ktest::TestResult {
    check_again(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vcn.again", Safe, again);
