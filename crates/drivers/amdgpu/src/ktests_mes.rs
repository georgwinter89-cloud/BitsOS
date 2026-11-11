//! GX-7k in-kernel tests of the MES stage (`gx_bringup {"until":"mes"}` and `{"until":"gfx_mes"}`) against the
//! fakes: the same engine (`pspbringup::run` with `Until::Mes`/`GfxMes`: the GFX stage in Linux's MES order,
//! then `gpucore::mes::enable`) over the same owner stack as the live run, MES modelled by `mesfake` (both pipes,
//! the KIQ's PM4, the scheduler's API frames, the mapped queues run by GR-2's PM4 interpreter), for carve-outs of
//! 512 MiB, 8 GiB and 16 GiB and the start after Linux (MES found running); every failure the fake can stage (no
//! firmware start, firmware too old, a dead KIQ, a silent scheduler, no api_status, wrong masks, a refused map,
//! dead queues, a lost register write, no firmware header, a halted CP); a second run on a running MES (its
//! queues unmapped first), the stop, the Quiesce halt and the stale HQDs it leaves. Nothing touches a device:
//! Safe and rerunnable.

use crate::bo::BoManager;
use crate::gpucore::disc::Discovery;
use crate::gpucore::fakechip::StartAs;
use crate::gpucore::gfxring::{self, GfxError, GfxRingInputs, Variant};
use crate::gpucore::gmc::{GmcInputs, HubAddrs};
use crate::gpucore::mes::{self, MesAddrs, MesError, MesInputs, MesReport};
use crate::gpucore::mesfake::{self, MesHostile};
use crate::gpucore::mesq;
use crate::gpucore::psp::{Hw as _, Until};
use crate::gpucore::pspbringup::Outcome;
use crate::gpucore::pspfake::{FakeGpu, Hostile};
use crate::gpucore::ucode::ListOpts;
use crate::ipmod::gfx11;
use crate::ipmod::mes11::{self, HwMasks, M};
use crate::ipmod::mesapi::{self, Frame, op};
use crate::ipmod::mesmqd;
use crate::ktests_bringup::{GOP, Owner, one_variant};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use ktest::kassert;

const SIZES: [u64; 3] = [512 << 20, 8 << 30, 16 << 30];

/// The frames a passed `until=mes` sends, each followed by its fence frame (QUERY_SCHEDULER_STATUS).
const MES_FRAMES: [u32; 12] = [
    op::SET_HW_RSRC,
    op::QUERY_SCHEDULER_STATUS,
    op::SET_HW_RSRC_1,
    op::QUERY_SCHEDULER_STATUS,
    op::QUERY_SCHEDULER_STATUS,
    op::QUERY_SCHEDULER_STATUS,
    op::MISC,
    op::QUERY_SCHEDULER_STATUS,
    op::MISC,
    op::QUERY_SCHEDULER_STATUS,
    op::ADD_QUEUE,
    op::QUERY_SCHEDULER_STATUS,
];

/// One run as the tool makes it (`setup` on the fake after the classifier read it).
pub(crate) fn mes_run(d: &Discovery, size: u64, s: StartAs, until: Until, setup: &dyn Fn(&mut FakeGpu)) -> Result<(Box<Outcome>, Box<Owner>, BoManager), String> {
    let (out, owner, bo, _) = one_variant(d, size, s, until, false, Hostile::None, &ListOpts::PSP13_0_4, &[], GOP, Variant::DEFAULT, setup)?;
    Ok((out, owner, bo))
}

pub(crate) fn report(out: &Outcome) -> Result<&MesReport, String> {
    out.mes.as_ref().ok_or_else(|| format!("no MES stage: gfx {:?}, checks {:?}", out.gfx_ring.as_ref().map(|g| &g.error), out.checks.iter().filter(|c| !c.pass).map(|c| c.name).collect::<Vec<_>>()))
}

fn addrs(d: &Discovery) -> Result<MesAddrs, String> {
    MesAddrs::from_discovery(d).ok_or_else(|| String::from("no MES addresses"))
}

fn carve(owner: &mut Owner) -> Result<amdgpu_fmt::addr::Carveout, String> {
    let f = owner.hw().hw();
    amdgpu_fmt::addr::Carveout::new(f.fb_mc, f.fb_phys, f.fb_size).ok_or_else(|| String::from("carve"))
}

fn halted(f: &FakeGpu) -> bool {
    !f.mes.running() && f.mes.cntl() & mes11::f::MES_HALT != 0
}

/// What every passed run must show: MES started as Linux (the capture's first CP_MES_CNTL writes), the frames in
/// order with Linux's masks, the self test, the queues mapped (gfx_mes) and every rung, the picture untouched,
/// every write of the run on the live owner's lists, the buffer inside the carve-out.
fn check_run(d: &Discovery, out: &Outcome, owner: &mut Owner, size: u64, queues: bool, what: &str) -> ktest::TestResult {
    let r = report(out)?.clone();
    let failed: Vec<_> = out.checks.iter().filter(|c| !c.pass).map(|c| format!("{}: {}", c.name, c.why)).collect();
    kassert!(out.passed() && r.passed() && r.left_running, "{what}: {failed:?} {:?}", r.error);
    let a = addrs(d)?;
    let l = r.layout.ok_or("no layout")?;
    let f = owner.hw().hw();
    kassert!(f.mes.running(), "{what}: MES not running");
    // Linux's enable: both pipe resets on what was read (cold: the capture's 0x40000000 -> 0x40030000), then run.
    let w = &f.mes.cntl_writes;
    let reset_then_run = w.windows(2).any(|x| x[1] == mes11::CNTL_RUN && x[0] & 0x3_0000 == 0x3_0000 && x[0] & mes11::f::MES_HALT != 0);
    kassert!(reset_then_run && (r.halted_first.is_some() || !out.mes_pre.is_empty() || w.first() == Some(&0x4003_0000)), "{what}: CP_MES_CNTL writes {w:x?}");
    let n = if queues { MES_FRAMES.len() + 2 } else { MES_FRAMES.len() - 2 };
    let want: Vec<u32> = MES_FRAMES.iter().copied().chain([op::ADD_QUEUE, op::QUERY_SCHEDULER_STATUS]).take(n).collect();
    kassert!(f.mes.frames == want, "{what}: frames {:?}", f.mes.frames);
    let hw = f.mes.hw_res.clone().ok_or("no SET_HW_RESOURCES")?;
    let m = HwMasks::LINUX_780M;
    kassert!(hw[mesapi::at::HW_VMID_GFXHUB] == m.vmid_gfxhub && hw[mesapi::at::HW_GFX_HQD] == 2 && hw[mesapi::at::HW_COMPUTE_HQD] == 0xc && hw[mesapi::at::HW_SDMA_HQD] == 0xfc, "{what}: masks {:x?}", &hw[..20]);
    kassert!(hw[mesapi::at::HW_GC_BASE..mesapi::at::HW_GC_BASE + 5] == a.gc_base && hw[mesapi::at::HW_FLAGS] == mesapi::hwflag::LINUX && hw[mesapi::at::HW_OVERSUB_TIMER] == 50, "{what}: bases/flags");
    // The KIQ ran MAP_QUEUES (the scheduler) and WRITE_DATA (its ring test), gfx_mes also SET_RESOURCES.
    let kp: Vec<u8> = f.mes.kiq_packets.iter().copied().filter(|&o| o != amdgpu_fmt::pm4::op::NOP).collect();
    let kw: Vec<u8> = if queues { alloc::vec![0xA2, 0x37, 0xA0, 0x37] } else { alloc::vec![0xA2, 0x37] };
    kassert!(kp == kw, "{what}: KIQ packets {kp:x?}");
    kassert!(r.versions == (Some(mesfake::VERSIONS[0]), Some(mesfake::VERSIONS[1])) && r.rlc == Some((mesfake::RLC_COLD, 0x30e8)), "{what}: versions {:x?} rlc {:x?}", r.versions, r.rlc);
    if queues {
        kassert!(f.mes.mapped() == [(mesapi::qtype::COMPUTE, mes11::COMPUTE_DOORBELL_DW), (mesapi::qtype::GFX, gfx11::DOORBELL_DW)], "{what}: mapped {:?}", f.mes.mapped());
        kassert!(r.rungs.len() == mesq::RUNGS.len() && r.rungs.iter().all(|x| x.pass && x.done), "{what}: rungs {:?}", r.rungs);
        let mut b = [0u8; 8];
        kassert!(f.mem_read(l.comp_fence64, &mut b) && u64::from_le_bytes(b) == gfxring::FENCE64_START + 1, "{what}: compute fence64 {b:x?}");
        kassert!(f.gfx.doorbells.iter().all(|x| !x.2), "{what}: GFX ring 0 rang the direct ring's fake");
    } else {
        kassert!(f.mes.mapped().is_empty() && r.rungs.is_empty(), "{what}: queues {:?}", f.mes.mapped());
    }
    kassert!(f.display_writes.is_empty() && f.scanout_writes.is_empty() && out.violations_total == 0, "{what}: display {:?} scanout {:?}", f.display_writes, f.scanout_writes);
    kassert!(f.gc.off_access.is_empty() && f.hubs.faults.is_empty(), "{what}: GC off access {:?} faults {:?}", f.gc.off_access, f.hubs.faults);
    let off = l.bo_mc - f.fb_mc;
    kassert!(off >= GOP && off + mes::BO_BYTES <= size - crate::bo::DISCOVERY_RESERVE, "{what}: buffer at +{off:#x}");
    // Every GC write of the stage on the live owner's lists (GFX stage's and MES stage's), GRBM_GFX_CNTL back at 0.
    let xa = gfxring::GfxAddrs::from_discovery(d).ok_or("gfx addrs")?;
    let mut wl = xa.writable();
    wl.extend(a.writable());
    let mes_bytes: Vec<u32> = M::ALL.iter().filter_map(|&x| a.byte(x)).collect();
    let bad: Vec<_> = out.entries.iter().filter(|e| mes_bytes.contains(&e.byte) && !wl.contains(&e.byte)).collect();
    kassert!(bad.is_empty(), "{what}: writes off the lists {bad:x?}");
    kassert!(a.gfx(gfx11::R::GrbmGfxCntl).is_some_and(|b| f.reg_read(b) == Some(0)), "{what}: GRBM_GFX_CNTL not back at 0");
    // The answer's stage line and the regtrace mark.
    let text = mes::render(&r);
    let v = if queues { "mes verdict (gfx_mes): PASS" } else { "mes verdict (mes): PASS" };
    kassert!(text.last().map(String::as_str) == Some(v), "{what}: last line {:?}", text.last());
    kassert!(r.marks.iter().any(|&(n, a0, b0)| n == "mes start" && b0 - a0 == 30), "{what}: marks {:?}", r.marks);
    Ok(())
}

pub(crate) fn check_starts(d: &Discovery) -> ktest::TestResult {
    for size in SIZES {
        for (until, queues) in [(Until::Mes, false), (Until::GfxMes, true)] {
            let what = format!("{until:?} at {size:#x}");
            let (out, mut owner, _) = mes_run(d, size, StartAs::Cold, until, &|_| {})?;
            check_run(d, &out, &mut owner, size, queues, &what)?;
            let r = report(&out)?;
            kassert!(r.halted_first.is_none() && r.stale.is_empty() && out.mes_pre.is_empty() && !r.reused_bo, "{what}: halted first {:?} stale {:?}", r.halted_first, r.stale);
            kassert!(out.gfx_ring.as_ref().is_some_and(|g| g.variant == Variant::Mes && g.left_running && g.rungs.is_empty()), "{what}: the GFX stage");
        }
    }
    // After Linux: MES runs (Linux mapped its queues): dequeued and halted before the GFX stage, then a whole run.
    let (out, mut owner, _) = mes_run(d, 512 << 20, StartAs::AfterLinux, Until::GfxMes, &|_| {})?;
    kassert!(out.mes_pre.first().is_some_and(|l| l.contains("not this boot's")), "after Linux: {:?}", out.mes_pre);
    kassert!(owner.hw().hw().mes.dequeues.len() >= 2, "after Linux: dequeues {:?}", owner.hw().hw().mes.dequeues);
    check_run(d, &out, &mut owner, 512 << 20, true, "after Linux")?;
    Ok(())
}

/// A passed `until=gart` run, GFXHUB's addresses, and the GFX stage in the MES order on its owner.
fn after_cp(d: &Discovery) -> Result<(Box<Owner>, BoManager, HubAddrs, gfxring::Layout), String> {
    let (_, mut owner, mut bo) = crate::ktests_gmc::gart(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let hub = GmcInputs::new(d, carve(&mut owner)?).ok_or("gmc inputs")?.addrs.gfx;
    let l = gfx_mes_order(d, &mut owner, &mut bo, &hub)?;
    Ok((owner, bo, hub, l))
}

fn gfx_mes_order(d: &Discovery, owner: &mut Owner, bo: &mut BoManager, hub: &HubAddrs) -> Result<gfxring::Layout, String> {
    let inp = GfxRingInputs { variant: Variant::Mes, ..GfxRingInputs::new(d, carve(owner)?).ok_or("gfx inputs")? };
    let g = gfxring::enable(owner, bo, &inp, hub);
    kassert!(g.passed() && g.left_running, "the GFX stage in the MES order: {:?}", g.error);
    g.layout.ok_or_else(|| String::from("no GFX layout"))
}

fn inputs(d: &Discovery, owner: &mut Owner) -> Result<MesInputs<'static>, String> {
    MesInputs::new(d, carve(owner)?, [Some(mesfake::UC_START); 2]).ok_or_else(|| String::from("MES inputs"))
}

pub(crate) fn check_failures(d: &Discovery) -> ktest::TestResult {
    let cases: [(MesHostile, Until, &str); 9] = [
        (MesHostile::NoFirmware, Until::Mes, "NoVersion"),
        (MesHostile::OldFirmware, Until::Mes, "TooOld"),
        (MesHostile::KiqDead, Until::Mes, "KiqTimeout"),
        (MesHostile::SchedDead, Until::Mes, "ApiTimeout"),
        (MesHostile::NoApiStatus, Until::Mes, "ApiTimeout"),
        (MesHostile::MiscLost, Until::Mes, "SelfTest"),
        (MesHostile::MapRefused, Until::GfxMes, "ApiTimeout"),
        (MesHostile::QueuesDead, Until::GfxMes, "Timeout"),
        (MesHostile::UnmapHangs, Until::GfxMes, "Timeout"),
    ];
    for (h, until, want) in cases {
        let what = format!("{h:?}");
        let (out, owner, _) = mes_run(d, 512 << 20, StartAs::Cold, until, &|f| f.mes.hostile = h)?;
        let r = report(&out)?;
        let e = r.error.clone().ok_or_else(|| format!("{what}: no error"))?;
        kassert!(format!("{e:?}").starts_with(want) && !out.passed() && !r.left_running, "{what}: {e:?}");
        kassert!(!e.advice().is_empty() && mes::render(r).iter().any(|l| l.starts_with("mes error:")), "{what}: no advice line");
        kassert!(mes::render(r).last().is_some_and(|l| l.ends_with("FAIL")), "{what}: verdict");
        let mut o = owner;
        let f = o.hw().hw();
        kassert!(halted(f) && r.halt_failed.is_none(), "{what}: MES left running");
        kassert!(f.display_writes.is_empty(), "{what}: display touched");
        // A failure after ADD_QUEUE: the mapped queues unmapped through MES before the halt (or reported
        // stranded when MES wedges on the first REMOVE_QUEUE: the second is not sent behind it).
        let removes = f.mes.frames.iter().filter(|&&op| op == mesapi::op::REMOVE_QUEUE).count();
        match h {
            MesHostile::QueuesDead => kassert!(r.mapped.len() == 2 && f.mes.mapped().is_empty() && r.stranded.is_empty() && removes == 2 && r.unmap.iter().filter(|l| l.contains("answered")).count() == 2, "{what}: unmap after the failure: {:?} {:?}", r.unmap, f.mes.mapped()),
            MesHostile::UnmapHangs => {
                kassert!(r.stranded == ["compute ring", "GFX ring 0"] && removes == 1 && r.unmap.iter().any(|l| l.contains("not attempted")), "{what}: stranded {:?} removes {removes} {:?}", r.stranded, r.unmap);
                kassert!(mes::render(r).iter().any(|l| l.contains("STRANDED")), "{what}: no stranded line");
            }
            MesHostile::MapRefused => kassert!(r.mapped.is_empty() && r.stranded.is_empty() && removes == 0, "{what}: unmap of nothing mapped"),
            _ => kassert!(r.unmap.is_empty() && r.stranded.is_empty(), "{what}: unmap without mapped queues"),
        }
    }
    let a = addrs(d)?;
    // Wrong masks: the scheduler hangs silently on SET_HW_RESOURCES (no status, no fence).
    let (mut owner, mut bo, hub, gl) = after_cp(d)?;
    let inp = MesInputs { masks: HwMasks { compute_hqd: [0xff; 8], ..HwMasks::LINUX_780M }, ..inputs(d, &mut owner)? };
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, Some(&gl));
    kassert!(matches!(r.error, Some(MesError::ApiTimeout { what: "SET_HW_RESOURCES", .. })) && halted(owner.hw().hw()), "wrong masks: {:?}", r.error);
    // No firmware header: refused before any MES write; the CP halted: refused too.
    let n0 = owner.hw().hw().mes.cntl_writes.len();
    let inp = MesInputs { uc_start: [Some(mesfake::UC_START), None], ..inputs(d, &mut owner)? };
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, Some(&gl));
    kassert!(r.error == Some(MesError::NoFirmware { pipe: 1 }) && r.layout.is_none() && owner.hw().hw().mes.cntl_writes.len() == n0, "no header: {:?}", r.error);
    // A wrong program counter: the firmware never starts (GP3 0).
    let inp = MesInputs { uc_start: [Some(0x1000); 2], ..inputs(d, &mut owner)? };
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, Some(&gl));
    kassert!(matches!(r.error, Some(MesError::NoVersion { pipe: 0, .. })) && halted(owner.hw().hw()), "wrong pc: {:?}", r.error);
    let (_, mut owner, mut bo) = crate::ktests_gmc::gart(d, 512 << 20, StartAs::Cold, &|_| {})?;
    let hub = GmcInputs::new(d, carve(&mut owner)?).ok_or("gmc inputs")?.addrs.gfx;
    let n0 = owner.log().totals().writes;
    let inp = inputs(d, &mut owner)?;
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, None);
    kassert!(matches!(r.error, Some(MesError::CpNotRunning { .. })) && owner.log().totals().writes == n0 && !r.left_running, "CP halted: {:?}", r.error);
    // gfx_mes without the GFX stage's buffer.
    let inp = MesInputs { queues: true, ..inp };
    let _ = gfx_mes_order(d, &mut owner, &mut bo, &hub)?;
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, None);
    kassert!(r.error == Some(MesError::NoGfxRing), "no GFX ring: {:?}", r.error);
    let _ = a;
    Ok(())
}

/// A second run on a running MES (an earlier run's queues unmapped first, the buffer reused), the stop, the
/// Quiesce halt and the stale HQDs it leaves for the next run, the GFX ring stage refused while MES runs.
pub(crate) fn check_again(d: &Discovery) -> ktest::TestResult {
    let (mut owner, mut bo, hub, gl) = after_cp(d)?;
    let a = addrs(d)?;
    let armed = Cell::new(0u32);
    let arm = |_: MesAddrs| armed.set(armed.get() + 1);
    let inp = MesInputs { queues: true, arm: Some(&arm), ..inputs(d, &mut owner)? };
    let r1 = mes::enable(&mut owner, &mut bo, &inp, &hub, Some(&gl));
    kassert!(r1.passed() && armed.get() == 1 && !r1.reused_bo, "first: {:?} armed {}", r1.error, armed.get());
    // The direct GFX ring stage is refused while MES runs (nothing written).
    let n0 = owner.log().totals().writes;
    let x = GfxRingInputs::new(d, carve(&mut owner)?).ok_or("gfx inputs")?;
    let g = gfxring::enable(&mut owner, &mut bo, &x, &hub);
    kassert!(matches!(g.error, Some(GfxError::MesRunning { .. })) && owner.log().totals().writes == n0, "gfx_ring while MES runs: {:?}", g.error);
    // Again: the running MES stopped first (its queues unmapped through MES), the GFX stage, MES with the buffer.
    let prev = Some((r1.layout.map(|l| l.bo_mc), true));
    let inp2 = MesInputs { bo: r1.bo, prev, ..inp };
    let mes::Stopped { ok, lines, .. } = mes::pre_stop(&mut owner, &inp2).ok_or("MES not found running")?;
    kassert!(ok && lines.iter().filter(|l| l.contains("REMOVE_QUEUE") && l.contains("answered")).count() == 2 && owner.hw().hw().mes.mapped().is_empty(), "pre stop: {lines:?}");
    kassert!(halted(owner.hw().hw()), "pre stop left MES running");
    let gl2 = gfx_mes_order(d, &mut owner, &mut bo, &hub)?;
    let r2 = mes::enable(&mut owner, &mut bo, &inp2, &hub, Some(&gl2));
    kassert!(r2.passed() && r2.reused_bo && r2.layout == r1.layout, "again: {:?}", r2.error);
    // The stop: queues unmapped, pipes dequeued, MES halted; GRBM_GFX_CNTL back at 0.
    let d0 = owner.hw().hw().mes.dequeues.len();
    let mes::Stopped { ok, lines, .. } = mes::stop(&mut owner, &a, r2.layout.map(|l| l.bo_mc), true);
    kassert!(ok && halted(owner.hw().hw()) && owner.hw().hw().mes.mapped().is_empty(), "stop: {lines:?}");
    kassert!(owner.hw().hw().mes.dequeues.len() >= d0 + 2, "stop: dequeues {:?}", owner.hw().hw().mes.dequeues);
    // A stop of a halted MES writes only the dequeue side and the halt again (no frame).
    let f0 = owner.hw().hw().mes.frames.len();
    let mes::Stopped { ok, .. } = mes::stop(&mut owner, &a, r2.layout.map(|l| l.bo_mc), true);
    kassert!(ok && owner.hw().hw().mes.frames.len() == f0, "stop of a halted MES sent frames");
    // The Quiesce halt while a run holds the PSP lock: raw registers only, no wait, nothing when not armed.
    let gl3 = gfx_mes_order(d, &mut owner, &mut bo, &hub)?;
    let r3 = mes::enable(&mut owner, &mut bo, &MesInputs { bo: r1.bo, ..inp }, &hub, Some(&gl3));
    kassert!(r3.passed(), "third: {:?}", r3.error);
    let t0 = owner.hw().hw().now;
    let n0 = owner.log().totals().writes;
    let (none, ok) = crate::psplive::with_live_held(|_| (crate::psplive::quiesce_halt_mes(None, owner.hw()), crate::psplive::quiesce_halt_mes(Some(&a), owner.hw())));
    kassert!(none.is_none() && ok == Some(true), "quiesce halt: {none:?} {ok:?}");
    kassert!(owner.hw().hw().now == t0 && owner.log().totals().writes == n0, "quiesce halt waited or went through the owner's log");
    kassert!(halted(owner.hw().hw()), "quiesce halt left MES running");
    kassert!(owner.hw().hw().mes.hqd(mes11::KIQ_PIPE, M::CpHqdActive) == 1, "the KIQ's HQD is no longer active after a raw halt");
    // The next run meets the stale HQDs (a Reload after the Quiesce halt): dequeued first, then a whole run.
    let gl4 = gfx_mes_order(d, &mut owner, &mut bo, &hub)?;
    let r4 = mes::enable(&mut owner, &mut bo, &MesInputs { bo: r1.bo, ..inp }, &hub, Some(&gl4));
    kassert!(r4.passed() && r4.stale.iter().any(|x| x.0 == mes11::KIQ_PIPE) && r4.stale.iter().any(|x| x.0 == mes11::SCHED_PIPE), "stale HQDs: {:?} {:?}", r4.error, r4.stale);
    kassert!(mes::render(&r4).iter().any(|l| l.contains("HQD found active")), "no stale line");
    // A stop on a wedged MES: one REMOVE_QUEUE waits its API timeout, the second is not sent, both stranded.
    let fr0 = owner.hw().hw().mes.frames.iter().filter(|&&op| op == mesapi::op::REMOVE_QUEUE).count();
    owner.hw().hw().mes.hostile = MesHostile::UnmapHangs;
    let t0 = owner.hw().hw().now;
    let st = mes::stop(&mut owner, &a, r4.layout.map(|l| l.bo_mc), true);
    let el = owner.hw().hw().now.saturating_sub(t0);
    let removes = owner.hw().hw().mes.frames.iter().filter(|&&op| op == mesapi::op::REMOVE_QUEUE).count() - fr0;
    owner.hw().hw().mes.hostile = MesHostile::None;
    kassert!(!st.ok && st.stranded == ["compute ring", "GFX ring 0"] && removes == 1 && halted(owner.hw().hw()), "wedged stop: {:?} removes {removes}", st.lines);
    kassert!(st.mes_halted, "stranded queues do not unsay the halt: {:?}", st.lines);
    kassert!(el < 2 * mes11::API_TIMEOUT_US && st.lines.iter().any(|l| l.contains("not attempted")) && st.lines.iter().any(|l| l.contains("STRANDED")), "wedged stop waited {el} us: {:?}", st.lines);
    // Linux's plan from what the first run found: the capture's 29 writes plus the last select.
    let seq = mes::linux_sequence(&r1, &a);
    kassert!(seq.len() == 30 && seq.first() == Some(&(a.byte(M::CpMesCntl).unwrap_or(0), 0x4003_0000)) && seq[28].0 == a.byte(M::CpHqdActive).unwrap_or(0), "linux sequence {} {:x?}", seq.len(), seq.first());
    Ok(())
}

/// The registers' and packets' values against the capture and Linux's headers.
pub(crate) fn check_values() -> ktest::TestResult {
    kassert!(mesmqd::mes_pq_control(mes::KIQ_RING_BYTES) == 0xd830_8011, "KIQ PQ_CONTROL {:#x}", mesmqd::mes_pq_control(mes::KIQ_RING_BYTES));
    kassert!(mesmqd::hqd_doorbell(Some(mes11::KIQ_DOORBELL_DW)) == 0x4000_0060 && mesmqd::persistent_state() == 0x0be0_5501 && mesmqd::eop_control() == 0x8, "doorbell/persistent/eop");
    kassert!(mes11::grbm_select(3, 0, 0, 0) == 0xc && mes11::grbm_select(3, 1, 0, 0) == 0xd && mes11::grbm_select(1, 0, 1, 0) == 0x104, "grbm selects");
    kassert!(mes11::rlc_cp_schedulers(0x3038, 3, 1, 0) == 0x30e8 && mes11::cntl_reset(0x4000_0000) == 0x4003_0000 && mes11::CNTL_RUN == 0x0c00_0000, "rlc/cntl");
    kassert!(mes11::prgrm_cntr(mesfake::UC_START) == (0xfc00_1400, 0x3fff_ffff), "program counter");
    kassert!(mes11::cntl_halt(mes11::CNTL_RUN) == 0x4003_0010 && !mes11::cntl_running(0x4000_0000) && mes11::cntl_running(0x0c00_0000), "halt");
    kassert!(mesmqd::compute_pq_control(mes::COMPUTE_RING_BYTES) == 0xd030_890a && mesmqd::gfx_hqd_cntl(gfx11::RING_DW * 4) == 0x00a0_080a, "compute/gfx control");
    // The frames: header, status offsets, map fields.
    let f = Frame::set_hw_resources(&mesapi::HwRes { masks: HwMasks::LINUX_780M, agg_doorbells: [0; 5], sch_ctx: 0x1_2340, query_fence: 0x5_6780, gc_base: [1, 2, 3, 4, 5], mmhub_base: [0; 5], osssys_base: [0; 5] });
    kassert!(f.0[0] == 0x0004_0001 && f.status_at() == Some(50) && f.0[22] == 0x1_2340 && f.0[24] == 0x5_6780 && f.0[26] == 1, "SET_HW_RESOURCES {:x?}", &f.0[..30]);
    let mut q = Frame::query_status();
    q.set_status(0x1_0000_0008, 7);
    kassert!(q.0[0] == 0x0004_00b1 && q.0[2] == 8 && q.0[3] == 1 && q.0[4] == 7, "QUERY {:x?}", &q.0[..6]);
    let m = Frame::map_legacy(mesapi::qtype::GFX, 0, 0, gfx11::DOORBELL_DW, 0xAB00, 0xCD00);
    kassert!(m.status_at() == Some(37) && m.0[18] == gfx11::DOORBELL_DW && m.0[19] == 0xAB00 && m.0[21] == 0xCD00 && m.0[36] == 1 << 13, "ADD_QUEUE");
    let k = mesapi::map_queues(2, 5, 0, 0, mes11::SCHED_DOORBELL_DW, 0x1000, 0x2000);
    kassert!(k[0] == 0xC005_A200 && k[1] == (2 << 18) | (5 << 26) | (1 << 29) && k[2] == 22 << 2, "MAP_QUEUES {k:x?}");
    Ok(())
}

fn starts() -> ktest::TestResult {
    check_starts(&crate::gpucore::fakechip::discovery_780m())
}
// INT-W28 post-integration made the fake chains here QemuOnly (52-121 KiB of stack in a release build, over MCP's
// 64 KiB request thread). GX-7m2 boxed the chains' outcomes and owners and gave every engine stage its own frame:
// at most 33 KiB with `ktest_stack=48` (29.10., release, QEMU; plan/notes/gx-GX-7m2.md): Safe again.
ktest::ktest!("gx.mes.starts", Safe, starts);

fn failures() -> ktest::TestResult {
    check_failures(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.mes.failures", Safe, failures);

fn again() -> ktest::TestResult {
    check_again(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.mes.again", Safe, again);

/// GX-9a: a halt that did not take is no proof, with no queue stranded; the stop says so and a full reset reads it.
pub(crate) fn check_halt_proof(d: &Discovery) -> ktest::TestResult {
    let (mut owner, mut bo, hub, gl) = after_cp(d)?;
    let a = addrs(d)?;
    let inp = MesInputs { queues: true, ..inputs(d, &mut owner)? };
    let r = mes::enable(&mut owner, &mut bo, &inp, &hub, Some(&gl));
    kassert!(r.passed(), "start: {:?}", r.error);
    owner.hw().hw().mes.hostile = MesHostile::HaltIgnored;
    let st = mes::stop(&mut owner, &a, r.layout.map(|l| l.bo_mc), true);
    owner.hw().hw().mes.hostile = MesHostile::None;
    kassert!(!st.mes_halted && !st.ok && st.stranded.is_empty() && !halted(owner.hw().hw()), "{:?}", st.lines);
    let st = mes::stop(&mut owner, &a, r.layout.map(|l| l.bo_mc), false);
    kassert!(st.mes_halted && st.ok && halted(owner.hw().hw()), "{:?}", st.lines);
    Ok(())
}

fn halt_proof() -> ktest::TestResult {
    check_halt_proof(&crate::gpucore::fakechip::discovery_780m())
}
ktest::ktest!("gx.mes.halt_proof", Safe, halt_proof);

fn values() -> ktest::TestResult {
    check_values()
}
ktest::ktest!("gx.mes.values", Safe, values);
