//! GX-7m in-kernel tests of the VM stage (`gx_bringup {"until":"vm"}`) against the fakes: the fake engine's chain
//! (`pspbringup::run` with `Until::Vm`: the SDMA stage, the GFX stage in Linux's MES order, MES with the compute ring
//! and GFX ring 0 mapped, then `vmring::enable`) over the same owner stack as the live run, the hubs modelled by
//! `hubfake` (per-VMID 4-level walks from the base latched at the last invalidate, faults with the VMID, the default
//! page, the IH's PASID table), for carve-outs of 512 MiB, 8 GiB and 16 GiB; the MES process queue (ADD_QUEUE with
//! the VM, a VMID of MES's 8-15, REMOVE_QUEUE at the teardown, then MES's stop); the failures the fakes can stage
//! (no ack, a lost in-ring base write, a fault that does not clear, a busy queue, no SDMA queue, no VMID, MES
//! refusing the process queue, a teardown that fails and is retried); the encodings against Linux's packets and
//! frames; the teardown; the execute permission of the IB pages (GX-7m2). Nothing touches a device: rerunnable; Safe
//! except `stack_margin` (QemuOnly: it adds 32 KiB of frames on purpose).

use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::gfxring::Variant;
use crate::gpucore::hubfake::{Access, HubHostile};
use crate::gpucore::mesfake::MesHostile;
use crate::gpucore::vmproc::{self, ProcAddrs};
use crate::gpucore::psp::Until;
use crate::gpucore::pspbringup::Outcome;
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::vmring::{self, AckWait, Q, VmAddrs, VmError, VmInputs, VmReport};
use crate::ipmod::gmc11::{self, Fault, Hub, R};
use crate::ktests_bringup::{GOP, Owner, one_variant};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::kassert;

const SIZES: [u64; 3] = [512 << 20, 8 << 30, 16 << 30];

/// One `until=vm` run as the fake engine makes it (`setup` on the fake after the classifier read it).
pub(crate) fn vm_run(d: &Discovery, size: u64, s: StartAs, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>), String> {
    let (out, owner, _, _) = one_variant(d, size, s, Until::Vm, false, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, setup)?;
    Ok((out, owner))
}

pub(crate) fn report(out: &Outcome) -> Result<&VmReport, String> {
    out.vm.as_deref().ok_or_else(|| format!("no VM stage: failed checks {:?}", out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect::<Vec<_>>()))
}

fn failed(out: &Outcome) -> Vec<String> {
    out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect()
}

/// What every passed run must show: a kernel VMID, every rung of the three queues, the base the rings wrote
/// latched by the hub, the PASID in the LUT, one intended fault per queue (VMID, page, write) recorded by the hub
/// and cleared, nothing left faulted, the VM kept, the picture untouched, every write on the live lists.
fn check_pass(d: &Discovery, out: &Outcome, owner: &mut Owner, what: &str) -> ktest::TestResult {
    let r = report(out)?.clone();
    kassert!(out.passed(), "{what}: run failed: {:?} {:?}", failed(out), r.error);
    kassert!(r.passed(), "{what}: VM stage failed: {:?}", r.error);
    let vmid = r.vmid.ok_or("no VMID")?;
    kassert!((1..=7).contains(&vmid), "{what}: VMID {vmid} outside the kernel range");
    kassert!(r.sdma_skipped.is_none(), "{what}: SDMA skipped: {:?}", r.sdma_skipped);
    // GX-7m3: the 16 rungs plus the recovery rungs of the two queues on CPC (a stalled compute and process queue).
    kassert!(r.rungs.len() == 18 && r.rungs.iter().all(|x| x.pass), "{what}: rungs {:?}", r.rungs.iter().map(|x| (x.name, x.pass)).collect::<Vec<_>>());
    kassert!(vmring::expected(&r) == r.rungs.iter().map(|x| x.name).collect::<Vec<_>>(), "{what}: the rungs are not the expected ones");
    kassert!(r.faults.iter().map(|x| x.stalled).eq([false, true, false, true]), "{what}: which queues stalled {:?}", r.faults);
    let up = r.kept.ok_or("VM not kept after a pass")?;
    kassert!(up.vmid == vmid && Some(up.pd) == r.pd && up.proc.is_some_and(|p| p.answered && p.doorbell == vmproc::DOORBELL_DW), "{what}: kept {up:?}");
    let pv = r.proc_vmid.ok_or("the CPU found no MES context holding the root")?;
    kassert!(vmproc::MES_VMIDS.contains(&pv) && r.proc_lut == Some(up.pasid), "{what}: process VMID {pv}, LUT {:?}", r.proc_lut);
    let f = owner.hw().hw();
    kassert!(f.hubs.latched(vmid) == r.pd, "{what}: the hub latched {:?}, the VM's base {:?}", f.hubs.latched(vmid), r.pd);
    kassert!(f.hubs.latched(pv).map(|b| b & !0xfff) == r.pd.map(|b| b & 0x0000_ffff_ffff_f000), "{what}: MES's context {pv} latched {:?}", f.hubs.latched(pv));
    kassert!(f.mes.processes() == alloc::vec![(vmproc::DOORBELL_DW, pv)], "{what}: process queues {:?}", f.mes.processes());
    kassert!(f.hubs.lut_writes.iter().filter(|w| w.0 == vmid).count() == 3 && f.hubs.lut_writes.iter().all(|w| w.1 == up.pasid), "{what}: LUT writes {:?}", f.hubs.lut_writes);
    let faults: Vec<_> = f.hubs.faults.iter().filter(|x| x.vmid != 0).copied().collect();
    kassert!(faults.len() == 4, "{what}: VM faults {faults:?}");
    for (x, q) in faults.iter().zip(Q::ALL.into_iter().chain([Q::Proc])) {
        let want = if q == Q::Proc { pv } else { vmid };
        kassert!(x.mc == q.hole_va() && x.vmid == want && Fault::decode(x.status).write, "{what}: fault {x:?} for {q:?}");
        // The chip's clients (30.10.): CPG on the GFX ring, CPC on the compute queues.
        let cid = (x.status >> 9) & 0x1ff;
        kassert!(q == Q::Sdma || cid == if q == Q::Gfx { 6 } else { 5 }, "{what}: client {cid} for {q:?}");
    }
    kassert!(f.hubs.get(Hub::Gfx, R::FaultStatus) == 0, "{what}: a fault is left");
    // Each ring flushed on its own engine (0, 1, 12), never on the CPU's 17, with the VMID's request.
    let req = gmc11::invalidate_req(vmid, 0);
    for q in Q::ALL {
        kassert!(f.hubs.requests.iter().any(|&(h, e, v)| h == Hub::Gfx && e == q.engine() && v == req), "{what}: no flush of {q:?} on engine {}", q.engine());
    }
    kassert!(!f.hubs.requests.iter().any(|&(_, e, v)| e == gmc11::CPU_ENGINE && v == req), "{what}: a CPU flush of the VMID during a pass");
    kassert!(f.display_writes.is_empty(), "{what}: display writes");
    kassert!(owner.hw().total() == 0, "{what}: watcher violations");
    // Every hub/LUT register the stage touched is on the live owner's lists.
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let (rd, wr) = (a.readable(), a.writable());
    for b in [a.hub.byte(R::PtBaseLo(vmid)), a.hub.byte(R::FaultCntl), a.lut(vmid), a.hub.byte(R::FaultStatus)].into_iter().flatten() {
        kassert!(rd.contains(&b), "{what}: {b:#x} not readable");
    }
    for b in [a.hub.byte(R::PtBaseHi(vmid)), a.hub.byte(R::FaultCntl), a.lut(vmid), a.hub.byte(R::Req(gmc11::CPU_ENGINE))].into_iter().flatten() {
        kassert!(wr.contains(&b), "{what}: {b:#x} not writable");
    }
    Ok(())
}

pub(crate) fn check_runs(d: &Discovery) -> ktest::TestResult {
    for size in SIZES {
        let (out, mut owner) = vm_run(d, size, StartAs::Cold, &|_| {})?;
        check_pass(d, &out, &mut owner, &format!("cold {} MiB", size >> 20))?;
    }
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::AfterLinux, &|_| {})?;
    check_pass(d, &out, &mut owner, "after Linux")
}

/// The VM stage's inputs of a passed fake run (as the live side builds them from the kept buffers).
fn inputs_of(d: &Discovery, out: &Outcome, owner: &mut Owner) -> Result<vmring::VmInputs, String> {
    let f = owner.hw().hw();
    let carve = amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, f.fb_size).ok_or("carve")?;
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    crate::gpucore::pspbringup::vm_inputs(a, carve, crate::gpucore::sdma::SdmaAddrs::from_discovery(d), out).ok_or_else(|| String::from("no VM inputs"))
}

/// A failed stage: a named error, the VM torn down (base and LUT 0, a CPU flush on engine 17 acked), no fault
/// left, nothing kept.
fn check_torn_down(d: &Discovery, r: &VmReport, owner: &mut Owner, what: &str) -> ktest::TestResult {
    kassert!(r.error.is_some() && !r.passed() && r.kept.is_none(), "{what}: {:?}", r.error);
    let vmid = r.vmid.ok_or("no VMID")?;
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let f = owner.hw().hw();
    kassert!(r.teardown.iter().any(|l| l.contains("acked")), "{what}: teardown {:?}", r.teardown);
    kassert!(f.hubs.latched(vmid) == Some(0) && f.hubs.get(Hub::Gfx, R::PtBaseLo(vmid)) == 0, "{what}: base left");
    kassert!(f.hubs.lut_read(a.lut(vmid).ok_or("lut")?) == Some(0), "{what}: LUT left");
    kassert!(f.hubs.requests.iter().any(|&(_, e, v)| e == gmc11::CPU_ENGINE && v == gmc11::invalidate_req(vmid, 0)), "{what}: no CPU flush");
    Ok(())
}

/// The failures the fakes can stage, each bounded and torn down; a second run on the kept VM (torn down first,
/// the queues continued from their write pointers) and the teardown `until=stop` uses.
pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    check_pass(d, &out, &mut owner, "first run")?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let kept = report(&out)?.kept;
    // A second run (the next until=vm): the earlier VM torn down first, the kept queues continue.
    let r = vmring::enable(&mut owner, &vi, kept);
    kassert!(r.passed(), "second run: {:?} {:?}", r.error, r.rungs.iter().filter(|x| !x.pass).map(|x| &x.detail).collect::<Vec<_>>());
    kassert!(r.teardown.iter().any(|l| l.ends_with("before this run)")), "no teardown of the first VM: {:?}", r.teardown);
    // A ring's engine that never acks: the flush stalls the ring, the rung times out (bounded).
    owner.hw().hw().hubs.hostile = HubHostile::NoAckEngine(Q::Gfx.engine());
    let r = vmring::enable(&mut owner, &VmInputs { timeout_us: 2000, ..vi }, r.kept);
    kassert!(matches!(r.error, Some(VmError::Timeout { rung, .. }) if rung == Q::Gfx.rungs()[0]), "NoAckEngine: {:?}", r.error);
    check_torn_down(d, &r, &mut owner, "NoAckEngine")?;
    // That ring is stuck now: the next run finds it busy and writes nothing to it.
    owner.hw().hw().hubs.hostile = HubHostile::None;
    let r = vmring::enable(&mut owner, &vi, None);
    kassert!(matches!(r.error, Some(VmError::NotIdle { queue, .. }) if queue == Q::Gfx.name()) && r.vmid.is_none(), "busy ring: {:?}", r.error);
    // A fault that does not clear (fresh chain).
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.hubs.hostile = HubHostile::NoClear(Hub::Gfx))?;
    let r = report(&out)?.clone();
    kassert!(!out.passed() && matches!(r.error, Some(VmError::Result { rung }) if rung == Q::Gfx.rungs()[2]), "NoClear: {:?}", r.error);
    // The fault the clear left is found again after the teardown and reported.
    kassert!(r.late_fault.is_some() && r.teardown.iter().any(|l| l.contains("after the teardown")), "NoClear: no late fault reported: {:?}", r.teardown);
    // The next queue found it, and the report says the clear did not take.
    kassert!(r.left_faults.iter().any(|x| x.2.is_some_and(|s| s != 0)) && vmring::render(&r).iter().any(|l| l.contains("left a fault, NOT cleared")), "NoClear: the left fault's clear: {:?}", r.left_faults);
    owner.hw().hw().hubs.hostile = HubHostile::None;
    check_torn_down(d, &r, &mut owner, "NoClear")?;
    // A stray fault before the stage (an earlier run's) is reported and cleared first; the run passes.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let kept = report(&out)?.kept;
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let f = owner.hw().hw();
    let none = |_: u64| None;
    let _ = f.hubs.translate(Hub::Gfx, 0, false, 6, &mut |p| none(p));
    kassert!(f.hubs.get(Hub::Gfx, R::FaultStatus) != 0, "no stray fault staged");
    let r = vmring::enable(&mut owner, &vi, kept);
    kassert!(r.passed() && r.found_fault.is_some_and(|x| x.addr == Some(0)), "stray before: {:?} {:?}", r.error, r.found_fault);
    // The teardown until=stop uses (before MES stops): the process queue removed, then the kernel VMID.
    let up = r.kept.ok_or("not kept")?;
    let t = vmring::teardown(&mut owner, &a, &up, 1000, 100_000);
    kassert!(t.ok && t.retry.is_none() && !t.stranded, "teardown: {:?}", t.lines);
    let f = owner.hw().hw();
    kassert!(f.hubs.latched(up.vmid) == Some(0) && f.hubs.lut_read(a.lut(up.vmid).ok_or("lut")?) == Some(0), "teardown left the base or the LUT");
    kassert!(f.mes.processes().is_empty() && f.mes.mapped().len() == 2, "after the teardown: process queues {:?}, legacy {:?}", f.mes.processes(), f.mes.mapped());
    // Without an SDMA queue the SDMA rungs are skipped, the rest passes.
    let r = vmring::enable(&mut owner, &VmInputs { sdma: None, ..vi }, None);
    kassert!(r.passed() && r.sdma_skipped.is_some() && r.rungs.len() == 14, "no SDMA: {:?} {}", r.error, r.rungs.len());
    // Without the process queue rungs: the kernel queues' 12 and the compute ring's recovery.
    let t = vmring::teardown(&mut owner, &a, &r.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    let r = vmring::enable(&mut owner, &VmInputs { proc: false, ..vi }, None);
    kassert!(r.passed() && r.proc_skipped.is_some() && r.rungs.len() == 13 && r.kept.is_some_and(|u| u.proc.is_none()), "no process queue: {:?} {}", r.error, r.rungs.len());
    Ok(())
}

/// GX-7m fixer: the negative cases. No VMID (nothing built, nothing written); a ring whose base write is lost (rung 1
/// fails by the CPU's readback); MES refusing the process queue (bounded, stranded, the kernel VM torn down); a
/// teardown whose CPU flush is not acked (kept, the next run refuses to build over it, then retries and passes); the
/// stop order (teardown, then MES's stop unmaps the legacy queues and halts with nothing stranded).
pub(crate) fn check_negative(d: &Discovery) -> ktest::TestResult {
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    check_pass(d, &out, &mut owner, "first run")?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let t = vmring::teardown(&mut owner, &a, &report(&out)?.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    // No VMID: nothing built, no register written.
    let w0 = owner.log().totals().writes;
    let (rq0, lut0) = (owner.hw().hw().hubs.requests.len(), owner.hw().hw().hubs.lut_writes.len());
    let r = vmring::enable(&mut owner, &VmInputs { vmids: (1, 0), ..vi }, None);
    kassert!(r.error == Some(VmError::NoVmid) && r.vmid.is_none() && r.pd.is_none() && r.kept.is_none(), "no VMID: {:?}", r.error);
    let w1 = owner.log().totals().writes;
    let f = owner.hw().hw();
    kassert!(w1 == w0 && f.hubs.requests.len() == rq0 && f.hubs.lut_writes.len() == lut0, "no VMID: something was written");
    // A ring's base write is lost: the CPU's readback fails rung 1 of the first queue; torn down.
    owner.hw().hw().hubs.hostile = HubHostile::RingBaseLost;
    let r = vmring::enable(&mut owner, &vi, None);
    kassert!(matches!(r.error, Some(VmError::Result { rung }) if rung == Q::Gfx.rungs()[0]), "RingBaseLost: {:?}", r.error);
    owner.hw().hw().hubs.hostile = HubHostile::None;
    check_torn_down(d, &r, &mut owner, "RingBaseLost")?;
    // MES refuses the process queue's ADD_QUEUE: bounded, stranded, REMOVE_QUEUE not tried, the kernel VM torn down.
    owner.hw().hw().mes.hostile = MesHostile::ProcRefused;
    let r = vmring::enable(&mut owner, &VmInputs { api_us: 50_000, ..vi }, None);
    owner.hw().hw().mes.hostile = MesHostile::None;
    kassert!(r.error == Some(VmError::ProcRefused) && r.stranded && r.teardown.iter().any(|l| l.contains("not attempted")), "MapRefused: {:?} {:?}", r.error, r.teardown);
    kassert!(r.rungs.iter().filter(|x| x.pass).count() == 13, "MapRefused: the kernel rungs {:?}", r.rungs.iter().map(|x| (x.name, x.pass)).collect::<Vec<_>>());
    check_torn_down(d, &r, &mut owner, "MapRefused")?;
    // The rest of this chain on a fresh one (the refused frame left the fake scheduler's status slot behind).
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let kept = report(&out)?.kept.ok_or("not kept")?;
    // The CPU's flush is not acked: the teardown keeps the VM for a retry (the process queue removed once).
    owner.hw().hw().hubs.hostile = HubHostile::NoAckEngine(gmc11::CPU_ENGINE);
    let t = vmring::teardown(&mut owner, &a, &kept, 1000, 100_000);
    kassert!(!t.ok && !t.stranded && t.retry == Some(vmring::VmUp { proc: None, ..kept }), "no CPU ack: {:?} {:?}", t.retry, t.lines);
    kassert!(owner.hw().hw().mes.processes().is_empty(), "the process queue was not removed");
    // The next until=vm refuses to build over it and keeps it.
    let r = vmring::enable(&mut owner, &vi, t.retry);
    kassert!(r.error == Some(VmError::PrevTeardown) && r.retry == t.retry && r.pd.is_none(), "over a failed teardown: {:?} {:?}", r.error, r.retry);
    // Acked again: the retry tears it down and the run passes.
    owner.hw().hw().hubs.hostile = HubHostile::None;
    let r = vmring::enable(&mut owner, &vi, r.retry);
    kassert!(r.passed() && r.retry.is_none() && r.teardown.iter().any(|l| l.contains("acked") && l.ends_with("before this run)")), "retry: {:?} {:?}", r.error, r.teardown);
    // until=stop's order: the VM torn down (process queue removed), then MES's stop unmaps the legacy queues and
    // halts, nothing stranded.
    let t = vmring::teardown(&mut owner, &a, &r.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown before the stop: {:?}", t.lines);
    let ma = crate::gpucore::mes::MesAddrs::from_discovery(d).ok_or("no MES addresses")?;
    let st = crate::gpucore::mes::stop(&mut owner, &ma, Some(vi.mes.bo_mc), true);
    kassert!(st.ok && st.stranded.is_empty(), "MES stop after the VM: {:?}", st.lines);
    let f = owner.hw().hw();
    kassert!(f.mes.processes().is_empty() && f.mes.mapped().is_empty(), "after the stop: {:?} {:?}", f.mes.processes(), f.mes.mapped());
    Ok(())
}

/// The packets against Linux's: the PM4 flush (WRITE_DATA register writes from the PFP on GFX, the ack wait,
/// PFP_SYNC_ME on GFX only, the LUT entry) and the SDMA flush (sdma_v6_0's reg_write_reg_wait).
pub(crate) fn check_packets(d: &Discovery) -> ktest::TestResult {
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let dw = |r: R| a.hub.byte(r).map(|b| b / 4).unwrap_or(0);
    let pd = 0x0000_0080_1234_5001u64;
    let (vmid, pasid) = (3u8, 0x8000u32);
    let req = gmc11::invalidate_req(vmid, 0);
    let lut = a.lut(vmid).ok_or("no LUT")? / 4;
    // GFX, Linux's single packet.
    let mut v = Vec::new();
    vmring::pm4_flush(&mut v, &a, vmid, pd, 0, true, pasid, AckWait::Linux).map_err(|e| format!("{e:?}"))?;
    let wd = |reg: u32, val: u32, pfp: bool| [amdgpu_fmt::pm4::packet3(amdgpu_fmt::pm4::op::WRITE_DATA, 3), (if pfp { 1 << 30 } else { 0 }) | amdgpu_fmt::pm4::WR_CONFIRM, reg, 0, val];
    let mut want: Vec<u32> = Vec::new();
    want.extend(wd(dw(R::PtBaseLo(vmid)), pd as u32, true));
    want.extend(wd(dw(R::PtBaseHi(vmid)), (pd >> 32) as u32, true));
    // WAIT_REG_MEM: function 3, register, operation 1, engine PFP (gfx_v11_0_wait_reg_mem's control word).
    want.extend([amdgpu_fmt::pm4::packet3(amdgpu_fmt::pm4::op::WAIT_REG_MEM, 5), 3 | 1 << 6 | 1 << 8, dw(R::Req(0)), dw(R::Ack(0)), req, 1 << vmid, 0x20]);
    want.extend([amdgpu_fmt::pm4::packet3(amdgpu_fmt::pm4::op::PFP_SYNC_ME, 0), 0]);
    want.extend(wd(lut, pasid, true));
    kassert!(v == want, "GFX flush {v:x?}\nwant {want:x?}");
    // Compute, the helper form: no PFP, no PFP_SYNC_ME.
    let mut v = Vec::new();
    vmring::pm4_flush(&mut v, &a, vmid, pd, 1, false, pasid, AckWait::Helper).map_err(|e| format!("{e:?}"))?;
    let mut want: Vec<u32> = Vec::new();
    want.extend(wd(dw(R::PtBaseLo(vmid)), pd as u32, false));
    want.extend(wd(dw(R::PtBaseHi(vmid)), (pd >> 32) as u32, false));
    want.extend(wd(dw(R::Req(1)), req, false));
    want.extend([amdgpu_fmt::pm4::packet3(amdgpu_fmt::pm4::op::WAIT_REG_MEM, 5), 3, dw(R::Ack(1)), 0, 1 << vmid, 1 << vmid, 0x20]);
    want.extend(wd(lut, pasid, false));
    kassert!(v == want, "compute flush {v:x?}\nwant {want:x?}");
    // SDMA: SRBM_WRITE (op 14, byte enable 0xf), POLL_REGMEM (op 8) of the request register, then of the ack.
    let mut v = Vec::new();
    vmring::sdma_flush(&mut v, &a, vmid, pd, 12, pasid).map_err(|e| format!("{e:?}"))?;
    let srbm = |reg: u32, val: u32| [14u32 | 0xf << 28, reg, val];
    let poll = |reg: u32, val: u32, m: u32| [8u32 | 3 << 28, reg << 2, 0, val, m, 10 | 0xfff << 16];
    let mut want: Vec<u32> = Vec::new();
    want.extend(srbm(dw(R::PtBaseLo(vmid)), pd as u32));
    want.extend(srbm(dw(R::PtBaseHi(vmid)), (pd >> 32) as u32));
    want.extend(srbm(dw(R::Req(12)), req));
    want.extend(poll(dw(R::Req(12)), 0, 0));
    want.extend(poll(dw(R::Ack(12)), 1 << vmid, 1 << vmid));
    want.extend(srbm(lut, pasid));
    kassert!(v == want, "SDMA flush {v:x?}\nwant {want:x?}");
    // The process queue's ADD_QUEUE and REMOVE_QUEUE against mes_v11_api_def.h's offsets, and its MQD.
    let p = ProcAddrs::new(0x0080_0000_0000);
    let (root, cntl) = (0x0000_0080_0004_0000u64, 0x01ff_fe07u32);
    let f = crate::ipmod::mesapi::Frame::add_process_queue(&p.frame(root, pasid, cntl)).0;
    let g64 = |at: usize| crate::ipmod::mesapi::get64(&f, at);
    kassert!(f[0] == crate::ipmod::mesapi::header(crate::ipmod::mesapi::op::ADD_QUEUE) && f[1] == pasid && g64(2) == root && g64(4) == 0 && g64(6) == 0x0000_ffff_ffff_f000, "ADD_QUEUE process {:x?}", &f[..8]);
    kassert!(g64(8) == 0 && g64(10) == p.process_ctx && g64(12) == 0 && g64(14) == p.gang_ctx && f[16] == 1 && f[17] == 1, "ADD_QUEUE gang {:x?}", &f[8..18]);
    kassert!(f[18] == vmproc::DOORBELL_DW && g64(19) == p.mqd && g64(21) == p.page_mc + 8 && f[27] == 1 && g64(33) == 0 && f[35] == cntl && f[36] == 0, "ADD_QUEUE queue {:x?}", &f[18..37]);
    let f = crate::ipmod::mesapi::Frame::remove_process_queue(vmproc::DOORBELL_DW, p.gang_ctx).0;
    kassert!(f[1] == vmproc::DOORBELL_DW && crate::ipmod::mesapi::get64(&f, 2) == p.gang_ctx && f[4] == 0, "REMOVE_QUEUE process {:x?}", &f[..6]);
    use crate::ipmod::mesmqd::c;
    let m = p.mqd();
    let lo = |x: u64| x as u32;
    kassert!(m[c::PQ_BASE_LO] == lo(p.ring_va >> 8) && m[c::RPTR_REPORT_LO] == lo(p.page_va) && m[c::WPTR_POLL_LO] == lo(p.page_va + 8) && m[c::EOP_BASE_LO] == lo((p.page_va + 0x800) >> 8), "MQD addresses");
    kassert!(m[c::HQD_VMID] == 0 && m[c::HQD_ACTIVE] == 0 && m[c::PIPE_PRIORITY] == 1 && m[c::QUEUE_PRIORITY] == 0 && m[c::MQD_BASE_LO] == lo(p.mqd), "MQD fields");
    // GX-7m3: the process queue's IB as gfx_v11_0_ring_emit_ib_compute's for a MES queue: VMID field 0, inherit bit.
    let mut v = Vec::new();
    vmring::proc_ib(&mut v, 0x1_0000_0100, 5).map_err(|e| format!("{e:?}"))?;
    kassert!(v.len() == 4 && v[1] == 0x0000_0100 && v[3] == amdgpu_fmt::pm4::IB_VALID | vmring::IB_INHERIT_VMID | 5, "process queue IB {v:x?}");
    Ok(())
}

/// A run that stopped: the rung it stopped at, whether the queues after it still had their turn, nothing on the live
/// lists violated, the picture untouched.
fn check_stopped(out: &Outcome, owner: &mut Owner, r: &VmReport, rung: &str, what: &str) -> ktest::TestResult {
    kassert!(!out.passed() && !r.passed(), "{what}: passed");
    kassert!(matches!(r.error, Some(VmError::Result { rung: n } | VmError::Timeout { rung: n, .. }) if n == rung), "{what}: error {:?}", r.error);
    // Every queue's first rung still ran (a stopped queue is on record, the next one has its turn).
    for q in [Q::Compute, Q::Sdma] {
        kassert!(r.rungs.iter().any(|x| x.name == q.rungs()[0]), "{what}: {q:?} did not have its turn");
    }
    kassert!(r.kept.is_none(), "{what}: the VM was kept");
    let f = owner.hw().hw();
    kassert!(f.display_writes.is_empty(), "{what}: display writes");
    kassert!(owner.hw().total() == 0, "{what}: watcher violations");
    Ok(())
}

/// GX-7m3: a queue stopped at its intended fault. The chip's case first (CPC halts: the compute and the process queue
/// recover through MES, the sticky bit written 0, no HQD reset needed when the unmap took it off, CPC's client in the
/// lines; two runs in a row, then the stop's order). Then every way it can go otherwise on the chip.
pub(crate) fn check_stall(d: &Discovery) -> ktest::TestResult {
    use crate::gpucore::mes as gmes;
    use crate::gpucore::sdmafake::SdmaHostile;
    let compute = Q::Compute.recover_rung();
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    check_pass(d, &out, &mut owner, "stall")?;
    let r = report(&out)?.clone();
    let db = crate::ipmod::mes11::COMPUTE_DOORBELL_DW;
    let f = owner.hw().hw();
    kassert!(f.mes.maps.iter().filter(|&&m| m == db).count() == 2, "the compute ring's maps {:?}", f.mes.maps);
    kassert!(f.mes.resets.is_empty(), "an HQD reset the unmap made needless: {:?}", f.mes.resets);
    kassert!(f.mes.cpc_error == 0 && f.mes.cpc_error_writes.iter().all(|&v| v == 0) && !f.mes.cpc_error_writes.is_empty(), "the halt bit: {:#x}, writes {:?}", f.mes.cpc_error, f.mes.cpc_error_writes);
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(x.detail.contains("REMOVE_QUEUE") && x.detail.contains("ADD_QUEUE") && x.detail.contains("CP_HQD_ACTIVE after the unmap 0x0") && x.detail.contains("halt bit set"), "the recovery's line: {}", x.detail);
    let x = r.rungs.iter().find(|x| x.name == Q::Compute.rungs()[2]).ok_or("no fault rung")?;
    kassert!(x.detail.contains("the queue stalled at its fault") && x.detail.contains("client CPC") && x.detail.contains("CP_HQD_ACTIVE 0x1") && x.detail.contains("CPC_UTCL1_ERROR 0x1"), "the fault's line: {}", x.detail);
    let text = vmring::render(&r);
    kassert!(text.iter().any(|l| l.starts_with("gfx vm fault: compute") && l.ends_with("intended, the queue stalled at it")), "the fault lines: {text:?}");
    // Twice in a row on the recovered queues, then until=stop's order: nothing stranded.
    let vi = inputs_of(d, &out, &mut owner)?;
    let r2 = vmring::enable(&mut owner, &vi, r.kept);
    kassert!(r2.passed() && r2.left_faults.is_empty(), "second run: {:?} {:?}", r2.error, r2.rungs.iter().filter(|x| !x.pass).map(|x| &x.detail).collect::<Vec<_>>());
    let r3 = vmring::enable(&mut owner, &vi, r2.kept);
    kassert!(r3.passed(), "third run: {:?}", r3.error);
    let t = vmring::teardown(&mut owner, &a, &r3.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok && !t.stranded, "teardown: {:?}", t.lines);
    let ma = gmes::MesAddrs::from_discovery(d).ok_or("no MES addresses")?;
    let st = gmes::stop(&mut owner, &ma, Some(vi.mes.bo_mc), true);
    kassert!(st.ok && st.stranded.is_empty(), "stop: {:?}", st.lines);
    kassert!(owner.hw().hw().mes.unknown_removes == 0 && !st.lines.iter().any(|l| l.contains("no REMOVE_QUEUE")), "stop: a remove of nothing, or the recovered ring not removed: {:?}", st.lines);
    // CPC goes on after all: no recovery rung, nothing unmapped, the 16 rungs.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.hubs.hostile = HubHostile::CpcGoesOn)?;
    let r = report(&out)?.clone();
    kassert!(out.passed() && r.rungs.len() == 16 && r.faults.iter().all(|x| !x.stalled), "CpcGoesOn: {:?} {}", r.error, r.rungs.len());
    kassert!(owner.hw().hw().mes.maps.iter().filter(|&&m| m == db).count() == 1, "CpcGoesOn: a remap");
    // SDMA stalls too: reset, restarted, its rung 4 runs on that.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.sdma.hostile = SdmaHostile::FaultStalls)?;
    let r = report(&out)?.clone();
    kassert!(out.passed() && r.rungs.len() == 19 && r.faults.iter().map(|x| x.stalled).eq([false, true, true, true]), "SDMA stalls: {:?} {:?}", r.error, r.rungs.iter().filter(|x| !x.pass).map(|x| &x.detail).collect::<Vec<_>>());
    let x = r.rungs.iter().find(|x| x.name == Q::Sdma.recover_rung()).ok_or("no SDMA recovery rung")?;
    kassert!(x.pass && x.detail.contains("SDMA0_QUEUE_RESET_REQ") && x.detail.contains("cleared"), "SDMA's line: {}", x.detail);
    let x = r.rungs.iter().find(|x| x.name == Q::Sdma.rungs()[2]).ok_or("no SDMA fault rung")?;
    kassert!(x.detail.contains("SDMA0_STATUS_REG") && x.detail.contains("SDMA0_UTCL1_WR_XNACK0"), "SDMA at the stall: {}", x.detail);
    kassert!(owner.hw().total() == 0 && owner.hw().hw().display_writes.is_empty(), "SDMA stalls: watcher {} display {:?}", owner.hw().total(), owner.hw().hw().display_writes.len());
    let vi = inputs_of(d, &out, &mut owner)?;
    let r2 = vmring::enable(&mut owner, &vi, r.kept);
    kassert!(r2.passed(), "SDMA stalls, second run: {:?}", r2.error);
    // The unmap leaves the HQD active: the 7.1 reset dequeue takes it off, ME 1's queue selected.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::UnmapLeavesActive)?;
    let r = report(&out)?.clone();
    kassert!(out.passed(), "UnmapLeavesActive: {:?}", r.error);
    let want = crate::ipmod::mes11::grbm_select(crate::ipmod::mes11::COMPUTE_ME, crate::ipmod::mes11::COMPUTE_PIPE, crate::ipmod::mes11::COMPUTE_QUEUE, 0);
    kassert!(owner.hw().hw().mes.resets == alloc::vec![want], "UnmapLeavesActive: resets {:?}", owner.hw().hw().mes.resets);
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(x.detail.contains("reset (DEQUEUE_REQUEST 2, SPI_COMPUTE_QUEUE_RESET 1): now 0x0"), "UnmapLeavesActive: {}", x.detail);
    // The reset does nothing: the rung fails with its reason; the others still ran.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::HqdStuck)?;
    let r = report(&out)?.clone();
    check_stopped(&out, &mut owner, &r, compute, "HqdStuck")?;
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(!x.pass && x.detail.contains("stays active"), "HqdStuck: {}", x.detail);
    // Mapped again, it never runs: bounded, and the rung's line says what the chip held.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::RemapDead)?;
    let r = report(&out)?.clone();
    check_stopped(&out, &mut owner, &r, compute, "RemapDead")?;
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(!x.pass && !x.done && x.detail.contains("ring test") && x.detail.contains("CP_CPC_STATUS"), "RemapDead: {}", x.detail);
    // REMOVE_QUEUE of the stalled ring is not answered: stranded, on record, MES deaf for the process queue too.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::RemoveHangs)?;
    let r = report(&out)?.clone();
    check_stopped(&out, &mut owner, &r, compute, "RemoveHangs")?;
    kassert!(r.stranded && vmring::render(&r).iter().any(|l| l.starts_with("vm: STRANDED")), "RemoveHangs: not stranded");
    // MES refuses the process queue's first ADD_QUEUE: the kernel queues' rungs stand.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::ProcRefused)?;
    let r = report(&out)?.clone();
    kassert!(r.error == Some(VmError::ProcRefused) && r.rungs.iter().filter(|x| x.pass).count() == 13, "ProcRefused: {:?}", r.error);
    kassert!(owner.hw().total() == 0, "ProcRefused: watcher violations");
    // The stalled ring is unmapped, but its map is not answered: stranded, on record, nothing runs on it.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let t = vmring::teardown(&mut owner, &a, &report(&out)?.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    owner.hw().hw().mes.hostile = MesHostile::MapRefused;
    let r = vmring::enable(&mut owner, &VmInputs { api_us: 50_000, ..vi }, None);
    kassert!(r.stranded && matches!(r.error, Some(VmError::Result { rung }) if rung == compute), "MapRefused: {:?} {}", r.error, r.stranded);
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(!x.pass && x.detail.contains("ADD_QUEUE FAILED"), "MapRefused: {}", x.detail);
    // GFX ring 0 halts at its fault (CP_DEBUG's bit lost): never recovered, a failure; the other queues still run.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.hubs.hostile = HubHostile::CpgHalts)?;
    let r = report(&out)?.clone();
    check_stopped(&out, &mut owner, &r, Q::Gfx.rungs()[2], "CpgHalts")?;
    let fx = r.faults.iter().find(|x| x.queue == Q::Gfx).ok_or("CpgHalts: no fault read")?;
    kassert!(fx.intended && !fx.stalled, "CpgHalts: {fx:?}");
    kassert!(r.rungs.iter().any(|x| x.name == Q::Sdma.rungs()[3] && x.pass), "CpgHalts: SDMA's rungs did not run");
    // The queue stops and no register says why: a timeout, no recovery, the fault line says it is not the intended one.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.hubs.hostile = HubHostile::FaultUnrecorded)?;
    let r = report(&out)?.clone();
    let rung = Q::Gfx.rungs()[2];
    kassert!(!out.passed() && matches!(r.error, Some(VmError::Result { rung: n } | VmError::Timeout { rung: n, .. }) if n == rung), "FaultUnrecorded: {:?}", r.error);
    kassert!(r.faults.iter().all(|x| !x.intended && !x.stalled), "FaultUnrecorded: {:?}", r.faults);
    let x = r.rungs.iter().find(|x| x.name == Q::Compute.rungs()[2]).ok_or("no compute fault rung")?;
    kassert!(!x.pass && x.detail.contains("the ring stopped") && x.detail.contains("NOT the intended fault") && x.detail.contains("at the stall"), "FaultUnrecorded: {}", x.detail);
    kassert!(!r.rungs.iter().any(|x| x.name == compute), "FaultUnrecorded: recovered a queue with no fault on record");
    kassert!(owner.hw().total() == 0, "FaultUnrecorded: watcher violations");
    Ok(())
}

/// GX-7m3: what a recovery that fails after MES's REMOVE_QUEUE leaves: nothing removed twice, an HQD no firmware owns is
/// stranded and refuses the CP's restart, until=vm refuses a compute ring that is not mapped.
pub(crate) fn check_left(d: &Discovery) -> ktest::TestResult {
    use crate::gpucore::mes as gmes;
    let compute = Q::Compute.recover_rung();
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let ma = gmes::MesAddrs::from_discovery(d).ok_or("no MES addresses")?;
    let db = crate::ipmod::mes11::COMPUTE_DOORBELL_DW;
    // The reset does nothing: stranded. A later gfx_mes is refused, until=vm too, the stop leaves it out and says so.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::HqdStuck)?;
    let r = report(&out)?.clone();
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(r.stranded && x.detail.contains("STRANDED") && vmring::render(&r).iter().any(|l| l.starts_with("vm: STRANDED") && l.contains("stayed active")), "HqdStuck: not stranded: {}", x.detail);
    let pre = crate::psplive::Pre { reboot_needed: false, quarantined: false, display: Ok(()), has_bos: true, until: Until::GfxMes, m2_runs: 0, again: true, kept: false, gfx_runs: 1, gc_failed: false, reset: false, gmc_runs: 1, kept_gc_booted: false, sdma_runs: 1, gart_up: false, gfx_ring_runs: 0, mes_runs: 1, mes_up: true, mes_stranded: r.stranded, mes_queues: true };
    kassert!(crate::psplive::preflight(&pre).is_err_and(|e| e.contains("STRANDED")), "HqdStuck: until=gfx_mes not refused");
    let vi = inputs_of(d, &out, &mut owner)?;
    let again = vmring::enable(&mut owner, &vi, None);
    kassert!(again.error == Some(VmError::ComputeUnmapped) && again.rungs.is_empty() && again.vmid.is_none(), "HqdStuck, until=vm again: {:?}", again.error);
    let st = gmes::stop(&mut owner, &ma, Some(vi.mes.bo_mc), true);
    kassert!(!st.ok && st.stranded == ["compute ring"] && st.lines.iter().any(|l| l.contains("no REMOVE_QUEUE")) && st.lines.iter().any(|l| l.contains("STRANDED")), "HqdStuck, stop: {:?} {:?}", st.stranded, st.lines);
    let f = owner.hw().hw();
    kassert!(f.mes.unknown_removes == 0 && f.mes.mapped().is_empty(), "HqdStuck: removes of nothing {}, mapped {:?}", f.mes.unknown_removes, f.mes.mapped());
    // The map is not answered: stranded, the same stop.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let t = vmring::teardown(&mut owner, &a, &report(&out)?.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    owner.hw().hw().mes.hostile = MesHostile::MapRefused;
    let r = vmring::enable(&mut owner, &VmInputs { api_us: 50_000, ..vi }, None);
    kassert!(r.stranded, "MapRefused: not stranded");
    let st = gmes::stop(&mut owner, &ma, Some(vi.mes.bo_mc), true);
    kassert!(st.stranded.first() == Some(&"compute ring") && owner.hw().hw().mes.unknown_removes == 0, "MapRefused, stop: {:?}", st.lines);
    // Its memory refused with the HQD off: not stranded, not mapped: until=vm refuses, the stop is clean, a fresh map follows.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let t = vmring::teardown(&mut owner, &a, &report(&out)?.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    owner.hw().hw().refuse_mem = Some((vi.mes.compute_mqd, 0));
    let r = vmring::enable(&mut owner, &vi, None);
    owner.hw().hw().refuse_mem = None;
    kassert!(!r.stranded && r.teardown.iter().all(|l| !l.contains("STRANDED")), "memory: stranded: {:?}", r.teardown);
    let x = r.rungs.iter().find(|x| x.name == compute).ok_or("no recovery rung")?;
    kassert!(!x.pass && x.detail.contains("the MQD's memory"), "memory: {}", x.detail);
    let maps = owner.hw().hw().mes.maps.iter().filter(|&&m| m == db).count();
    kassert!(vmring::enable(&mut owner, &vi, None).error == Some(VmError::ComputeUnmapped), "memory: until=vm again");
    let st = gmes::stop(&mut owner, &ma, Some(vi.mes.bo_mc), true);
    kassert!(st.ok && st.stranded.is_empty() && st.lines.iter().any(|l| l.contains("no REMOVE_QUEUE")), "memory, stop: {:?}", st.lines);
    let f = owner.hw().hw();
    kassert!(f.mes.unknown_removes == 0 && f.mes.maps.iter().filter(|&&m| m == db).count() == maps && f.mes.mapped().is_empty(), "memory: removes of nothing {}", f.mes.unknown_removes);
    Ok(())
}

/// GX-7m3: the process queue's recovery failing after its REMOVE_QUEUE: the teardown sends no second one.
pub(crate) fn check_left_proc(d: &Discovery) -> ktest::TestResult {
    let rung = Q::Proc.recover_rung();
    // ADD_QUEUE not answered: stranded, MES may hold it, REMOVE_QUEUE not tried.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.mes.hostile = MesHostile::ProcAgainRefused)?;
    let r = report(&out)?.clone();
    let x = r.rungs.iter().find(|x| x.name == rung).ok_or("no recovery rung")?;
    kassert!(!x.pass && x.detail.contains("ADD_QUEUE not answered") && x.detail.contains("STRANDED") && r.stranded, "ProcAgainRefused: {} {}", x.detail, r.stranded);
    kassert!(r.teardown.iter().any(|l| l.contains("not attempted")) && owner.hw().hw().mes.unknown_removes == 0, "ProcAgainRefused: {:?} {}", r.teardown, owner.hw().hw().mes.unknown_removes);
    // Its memory refused before MES is asked: gone from MES and the report, not stranded, the teardown leaves it alone.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let vi = inputs_of(d, &out, &mut owner)?;
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let t = vmring::teardown(&mut owner, &a, &report(&out)?.kept.ok_or("not kept")?, 1000, 100_000);
    kassert!(t.ok, "teardown: {:?}", t.lines);
    owner.hw().hw().refuse_mem = Some((ProcAddrs::new(vi.mes.bo_mc).mqd, 1));
    let r = vmring::enable(&mut owner, &vi, None);
    owner.hw().hw().refuse_mem = None;
    let x = r.rungs.iter().find(|x| x.name == rung).ok_or("no recovery rung")?;
    kassert!(!x.pass && x.detail.contains("the queue's memory") && !r.stranded && r.proc.is_none(), "memory: {} stranded {} {:?}", x.detail, r.stranded, r.proc);
    kassert!(r.teardown.iter().all(|l| !l.contains("process queue")) && owner.hw().hw().mes.unknown_removes == 0, "memory: {:?} {}", r.teardown, owner.hw().hw().mes.unknown_removes);
    Ok(())
}

/// The fake hub itself: a base written without an invalidate is not used; an unmapped VA faults with the VMID and
/// goes to the default page; the clear clears; `NoClear` keeps it.
pub(crate) fn check_hub(d: &Discovery) -> ktest::TestResult {
    let mut h = crate::gpucore::hubfake::HubFake::new(d);
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let b = |r: R| a.hub.byte(r).unwrap_or(0);
    h.write(b(R::Ctx(2)), gmc11::ctxn_cntl(0x01ff_fe00));
    h.write(b(R::PtEndLo(2)), gmc11::MAX_PFN_LAST as u32);
    h.write(b(R::PtEndHi(2)), (gmc11::MAX_PFN_LAST >> 32) as u32);
    h.write(b(R::FaultCntl), gmc11::fault_cntl(0x3fff_fffc));
    h.write(b(R::FaultDefaultLo), 0x1234);
    h.write(b(R::PtBaseLo(2)), 0x5001);
    let none = |_: u64| None;
    kassert!(h.latched(2).is_none(), "latched before any flush");
    let p = h.translate_vm(Hub::Gfx, 2, 0x1_0000_0040, Access::Write, 13, &none);
    kassert!(p == Some(0x1234_000 | 0x40), "fault not sent to the default page: {p:x?}");
    let st = Fault::decode(h.get(Hub::Gfx, R::FaultStatus));
    kassert!(st.vmid == 2 && st.write && st.cid == 13, "status {st:?}");
    h.write(b(R::Req(4)), gmc11::invalidate_req(2, 0));
    kassert!(h.latched(2) == Some(0x5001), "not latched at the flush");
    h.write(b(R::FaultCntl), gmc11::fault_cntl(0x3fff_fffc) | gmc11::f::FAULT_CLEAR_STATUS);
    kassert!(h.get(Hub::Gfx, R::FaultStatus) == 0, "not cleared");
    h.hostile = HubHostile::NoClear(Hub::Gfx);
    let _ = h.translate_vm(Hub::Gfx, 2, 0x2000, Access::Read, 13, &none);
    h.write(b(R::FaultCntl), gmc11::fault_cntl(0x3fff_fffc) | gmc11::f::FAULT_CLEAR_STATUS);
    kassert!(h.get(Hub::Gfx, R::FaultStatus) != 0, "NoClear cleared");
    Ok(())
}

/// GX-7m2: the execute permission (the 29.10. chip run). A passed run's pages carry EXECUTABLE: every queue's IB
/// page fetches through the latched base without a fault. A hub that finds no EXECUTABLE in any PTE (the 29.10.
/// tables) fails the first IB rung of GFX ring 0 with the chip's decode (CPF, read, the kernel VMID,
/// PERMISSION_FAULTS 0x8 at the IB's own page), the rung's line names it, and the VM is torn down. The hub alone:
/// a page mapped read/write without EXECUTABLE reads and writes, but a fetch faults 0x8.
pub(crate) fn check_exec(d: &Discovery) -> ktest::TestResult {
    use crate::gpucore::gfxfake;
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
    check_pass(d, &out, &mut owner, "exec")?;
    let vmid = report(&out)?.vmid.ok_or("no VMID")?;
    let f = owner.hw().hw();
    let n0 = f.hubs.faults.len();
    for q in Q::ALL {
        let p = f.vm_access(vmid, q.ib_va(), Access::Fetch, gfxfake::CID_CPF);
        kassert!(p.is_some() && f.hubs.faults.len() == n0, "{q:?}: the IB page does not fetch: {:x?}", f.hubs.faults.get(n0..));
    }
    kassert!(vmring::MAP_FLAGS.exec && vmring::MAP_FLAGS.read && vmring::MAP_FLAGS.write, "the test VM's flags {:?}", vmring::MAP_FLAGS);
    // The 29.10. tables: no EXECUTABLE anywhere.
    let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|f| f.hubs.hostile = HubHostile::NoExecute)?;
    let r = report(&out)?.clone();
    let ib = Q::Gfx.rungs()[1];
    kassert!(!out.passed() && matches!(r.error, Some(VmError::Result { rung } | VmError::Timeout { rung, .. }) if rung == ib), "NoExecute: {:?}", r.error);
    let x = r.rungs.iter().find(|x| x.name == ib).ok_or("no IB rung")?;
    kassert!(!x.pass && x.detail.contains("refused for execute"), "NoExecute: the IB rung's line {}", x.detail);
    let vmid = r.vmid.ok_or("no VMID")?;
    let f = owner.hw().hw();
    let fx = f.hubs.faults.iter().find(|x| x.vmid == vmid).copied().ok_or("NoExecute: no VM fault")?;
    let fd = Fault::decode(fx.status);
    kassert!(fx.mc & !0xfff == Q::Gfx.ib_va() && !fd.write && fd.permission == 0x8 && fd.cid == gfxfake::CID_CPF && fd.vmid == u32::from(vmid), "NoExecute: fault {fx:x?} {fd:?}");
    f.hubs.hostile = HubHostile::None;
    check_torn_down(d, &r, &mut owner, "NoExecute")?;
    // The hub alone: a read/write page without EXECUTABLE.
    let mut h = crate::gpucore::hubfake::HubFake::new(d);
    let a = VmAddrs::from_discovery(d).ok_or("no VM addresses")?;
    let b = |r: R| a.hub.byte(r).unwrap_or(0);
    let mut t = Tables::default();
    let carve = amdgpu_fmt::addr::Carveout::new(0x80_0000_0000, 0x1000_0000, 64 << 20).ok_or("carve")?;
    let mut vm = amdgpu_fmt::vm::Vm::new(&mut t, carve).map_err(|e| format!("{e:?}"))?;
    let rw = amdgpu_fmt::pte::MapFlags { exec: false, ..vmring::MAP_FLAGS };
    vm.map(&mut t, vmring::VA_BASE, 4096, amdgpu_fmt::vm::Backing::Local { phys: 0x1010_0000 }, rw).map_err(|e| format!("{e:?}"))?;
    h.write(b(R::Ctx(3)), gmc11::ctxn_cntl(0x01ff_fe00));
    h.write(b(R::PtEndLo(3)), gmc11::MAX_PFN_LAST as u32);
    h.write(b(R::PtEndHi(3)), (gmc11::MAX_PFN_LAST >> 32) as u32);
    h.write(b(R::PtBaseLo(3)), vm.pd_addr() as u32);
    h.write(b(R::PtBaseHi(3)), (vm.pd_addr() >> 32) as u32);
    h.write(b(R::Req(4)), gmc11::invalidate_req(3, 0));
    let mem = |p: u64| t.entry(p);
    kassert!(h.translate_vm(Hub::Gfx, 3, vmring::VA_BASE + 8, Access::Read, 13, &mem) == Some(0x1010_0008), "read");
    kassert!(h.translate_vm(Hub::Gfx, 3, vmring::VA_BASE + 8, Access::Write, 13, &mem) == Some(0x1010_0008) && h.faults.is_empty(), "write {:x?}", h.faults);
    let _ = h.translate_vm(Hub::Gfx, 3, vmring::VA_BASE, Access::Fetch, gfxfake::CID_CPF, &mem);
    let st = Fault::decode(h.get(Hub::Gfx, R::FaultStatus));
    kassert!(h.faults.len() == 1 && st.permission == 0x8 && !st.write && st.vmid == 3 && st.cid == gfxfake::CID_CPF, "fetch without EXECUTABLE: {st:?}");
    Ok(())
}

/// GX-7m2: page tables on the heap for the hub test (one 4 KiB table per entry of the map).
#[derive(Default)]
struct Tables {
    t: alloc::collections::BTreeMap<u64, Vec<u64>>,
    next: u64,
}

impl Tables {
    /// 8 bytes at a host-physical address, as the hub fake reads a table entry.
    fn entry(&self, p: u64) -> Option<u64> {
        self.t.get(&(p & !0xfff)).and_then(|v| v.get(((p & 0xfff) / 8) as usize)).copied()
    }
}

impl amdgpu_fmt::store::TableRead for Tables {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        self.t.get(&table).and_then(|v| v.get(index)).copied()
    }
}

impl amdgpu_fmt::store::TableStore for Tables {
    fn alloc(&mut self) -> Option<amdgpu_fmt::store::TableAddr> {
        let phys = 0x1000_0000 + self.next * 4096;
        self.next += 1;
        self.t.insert(phys, alloc::vec![0; 512]);
        Some(amdgpu_fmt::store::TableAddr { phys, target: amdgpu_fmt::pte::Target::Local })
    }
    fn available(&self) -> usize {
        16usize.saturating_sub(self.next as usize)
    }
    fn free(&mut self, table: amdgpu_fmt::store::TableAddr) {
        self.t.remove(&table.phys);
    }
    fn write(&mut self, table: u64, index: usize, value: u64) {
        if let Some(e) = self.t.get_mut(&table).and_then(|v| v.get_mut(index)) {
            *e = value;
        }
    }
}

// INT-W28 post-integration made runs, failures and negative QemuOnly (79, 96 and 87 KiB of stack in a release
// build, over MCP's 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every stage of
// the engine its own frame: 21-33 KiB measured with `ktest_stack=48` (29.10., release, QEMU): Safe again.
fn runs() -> ktest::TestResult {
    check_runs(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.runs", Safe, runs);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.failures", Safe, failures);

/// GX-7m fixer: the stack margin of the deepest fake chain (until=vm after gfx_mes, with the process queue). The
/// in-kernel tests run on the boot stack (128 KiB); a debug build once overflowed it in `gx.gfxring.failures`,
/// seen only as "kernel panic inside the panic path". This runs the chain below `PAD_KIB` KiB of extra frames, so a
/// stage that grows the chain's frames fails here first (QemuOnly: on the Mini-PC MCP's threads have 64 KiB).
/// Measured 29.10. (debug build): 52 KiB of pad passes, 62 KiB overflows, so the chain needed about 70 KiB; 32 KiB
/// leaves room for growth and still fails long before the boot stack does. GX-7m2 (release, `ktest_stack`): the
/// chain about 24 KiB, with the pad 56 (the boot stack of a ktest build has 512 KiB since INT-W28).
pub const PAD_KIB: u32 = 32;

#[inline(never)]
fn below(n: u32, f: &mut dyn FnMut() -> ktest::TestResult) -> ktest::TestResult {
    let pad = core::hint::black_box([n as u8; 1024]);
    let r = if n == 0 { f() } else { below(n - 1, f) };
    core::hint::black_box(&pad);
    r
}

pub(crate) fn check_stack_margin(d: &Discovery) -> ktest::TestResult {
    below(PAD_KIB, &mut || {
        let (out, mut owner) = vm_run(d, 512 << 20, StartAs::Cold, &|_| {})?;
        check_pass(d, &out, &mut owner, "below the pad")
    })
}

fn stack_margin() -> ktest::TestResult {
    check_stack_margin(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.stack_margin", QemuOnly, stack_margin);

fn negative() -> ktest::TestResult {
    check_negative(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.negative", Safe, negative);

fn packets() -> ktest::TestResult {
    check_packets(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.packets", Safe, packets);

fn hub() -> ktest::TestResult {
    check_hub(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.hub", Safe, hub);

fn exec() -> ktest::TestResult {
    check_exec(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.exec", Safe, exec);

fn stall() -> ktest::TestResult {
    check_stall(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.stall", Safe, stall);

fn left() -> ktest::TestResult {
    check_left(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.left", Safe, left);

fn left_proc() -> ktest::TestResult {
    check_left_proc(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.vm.left_proc", Safe, left_proc);
