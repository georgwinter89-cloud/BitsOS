//! GX-7m3: a queue that stalled at its intended VM fault, brought back.
//!
//! Why it stalls. The 780M's run of 30.10. (plan/hw/gx-780m-minipc-2026-10-30/c1/g2-vm.txt): GFX ring 0 dropped its
//! faulting write and went on, the compute ring stopped at the same packet, with the fault recorded (CPC, write,
//! VMID 1, PERMISSION_FAULTS 0x5, the right page). Both run on the same hub setup, as Linux's: no retry
//! (amdgpu_gmc_noretry_set, amdgpu_gmc.c:648-661; gfxhub_v3_0.c:321-322), the fault defaults on in every context. The
//! difference is the client: Linux's "NO halt CP when page fault" is CP_DEBUG.CPG_UTCL1_ERROR_HALT_DISABLE
//! (gfxhub_v3_0.c:415-418), and gc_11_0_0_sh_mask.h has no such bit for CPC, while it has CPC_UTCL1_ERROR's
//! ERROR_DETECTED_HALT next to CPG_UTCL1_ERROR's (:15344-15347) and per-queue UTCL1 error bits in CP_HQD_ERROR
//! (:17703-17714). So a compute queue halts at a VM fault by design and Linux has nothing to program against it; what
//! Linux relies on is taking the queue away. A process queue: "process queues [are] evicted on gpuvm fault"
//! (amdgpu_drv.c:801-806, kfd_int_process_v11.c:272-275), by MES's REMOVE_QUEUE (kfd_device_queue_manager.c:1121-1122).
//! A kernel queue: 6.6 the job's timeout and a GPU reset (mode 2 is refused on 13.0.4, GX-D9); 7.1 per queue,
//! gfx_v11_0_reset_kcq (gfx_v7.1.c:7052-7081): mes_v11_0_reset_queue_mmio (mes11.c:436-455: CP_HQD_DEQUEUE_REQUEST 2,
//! SPI_COMPUTE_QUEUE_RESET 1, CP_HQD_ACTIVE polled), the MQD restored, the ring cleared at wptr 0, map_legacy_queue.
//! Pipe reset is off there ("The CPFW hasn't support pipe reset yet", gfx_v11_pipe_reset_support :6823), so it is not
//! tried here either. The chip also answered REMOVE_QUEUE of the stalled ring in 201 us (c1/g3-stop.txt).
//!
//! So the rung proves what Linux relies on. The fault is reported and decoded, then:
//! - the kernel compute ring: MES REMOVE_QUEUE (the unmap the brief asks for; MES then holds no record of it), the HQD
//!   read and, only if still active, reset as 7.1 does; its MQD and ring rewritten, wptr 0; ADD_QUEUE map_legacy;
//! - the MES process queue: REMOVE_QUEUE, MQD and pages rewritten, the same ADD_QUEUE (MES may pick another VMID);
//! - SDMA 0, if it stalls (the chip decides: nothing in Linux says it must): SDMA0_QUEUE_RESET_REQ's queue 0 bit, as
//!   mes11.c:456-480 does for a hung SDMA queue, then the stage's own restart.
//!
//! Then a ring test on the fresh queue, and the queue's rung 4 runs on it. GFX ring 0 never takes this path: the CP
//! program says it goes on, so a stall there stays a failure. Linux's RLC safe mode around the reset acts only with
//! clock gating on (amdgpu_gfx_rlc_enter_safe_mode), which this stage never turns on. The reads named `state` are
//! report only: they tell the desk what the chip held at the stall, and nothing depends on their values.
//!
//! `cpc_release` is the one step Linux does not have (no Linux source writes CPC_UTCL1_ERROR): if the sticky halt bit
//! is still set with the queue gone, it is written 0 before the map, so one round on the chip shows both the bit and
//! whether the fresh queue runs. The bit was already set, so the write cannot start a halt; the register is GC's, no
//! display field moves (GX-D9). Decision and reason: plan/notes/gx-GX-7m3.md.

use super::bringup::Bringup;
use super::mes::{self, Layout, MesAddrs, MesError, PmQueue, Regs, Rung, hx32};
use super::psp::Hw;
use super::sdma;
use super::vmproc;
use super::vmring::{self, Ctx, Q, Queue, VmInputs, VmReport};
use super::watch::Watch;
use crate::ipmod::gmc11::{Fault, R};
use crate::ipmod::mes11::{self, M, f};
use crate::ipmod::mesapi::Frame;
use crate::ipmod::mesmqd;
use crate::ipmod::sdma6;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::ring::RingError;

fn rd<H: Hw>(owner: &mut Bringup<Watch<H>>, b: Option<u32>) -> String {
    hx32(b.and_then(|b| owner.reg_read(b)))
}

/// What CPC and the queue's HQD hold (report only). `hqd`: the kernel ring's HQD (the process queue's is MES's choice).
fn cpc_state<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, hqd: bool) -> String {
    let mut s = format!(
        "CPC_UTCL1_ERROR {}, CPC_UTCL1_STATUS {}, CP_CPC_STATUS {}, CP_CPC_STALLED_STAT1 {}",
        rd(owner, a.byte(M::CpcUtcl1Error)),
        rd(owner, a.byte(M::CpcUtcl1Status)),
        rd(owner, a.byte(M::CpCpcStatus)),
        rd(owner, a.byte(M::CpCpcStalledStat1))
    );
    if hqd && mes::select(owner, a, mes11::COMPUTE_ME, mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE).is_ok() {
        s.push_str(&format!(
            ", the HQD: CP_HQD_ACTIVE {}, CP_HQD_ERROR {}, CP_HQD_PQ_RPTR {}, CP_HQD_DEQUEUE_REQUEST {}",
            rd(owner, a.byte(M::CpHqdActive)),
            rd(owner, a.byte(M::CpHqdError)),
            rd(owner, a.byte(M::CpHqdPqRptr)),
            rd(owner, a.byte(M::CpHqdDequeueRequest))
        ));
        let _ = mes::select(owner, a, 0, 0, 0);
    }
    s
}

/// What the queue's engine holds at or after the stall (report only).
#[inline(never)]
pub(super) fn state<H: Hw>(owner: &mut Bringup<Watch<H>>, which: Q, inp: &VmInputs) -> String {
    match (which, inp.addrs.hqd, inp.sdma) {
        (Q::Sdma, _, Some(s)) => format!(
            "SDMA0_STATUS_REG {}, SDMA0_STATUS4_REG {}, SDMA0_UTCL1_WR_XNACK0 {}, SDMA0_UTCL1_RD_XNACK0 {}",
            rd(owner, s.addrs.byte(sdma6::R::Status)),
            rd(owner, s.addrs.byte(sdma6::R::Status4)),
            rd(owner, s.addrs.byte(sdma6::R::Utcl1WrXnack0)),
            rd(owner, s.addrs.byte(sdma6::R::Utcl1RdXnack0))
        ),
        (Q::Compute, Some(a), _) => cpc_state(owner, &a, true),
        (Q::Proc, Some(a), _) => cpc_state(owner, &a, false),
        _ => String::from("no registers known"),
    }
}

/// The selected HQD: `(active as found, active after)`; reset as mes_v11_0_reset_queue_mmio (7.1) only if active.
fn hqd_reset(o: &mut dyn Regs, a: &MesAddrs, us: u64) -> Result<(u32, u32), MesError> {
    let found = mes::rd(o, a, M::CpHqdActive)?;
    let mut now = found;
    if found & f::HQD_ACTIVE != 0 {
        mes::wr(o, a, M::CpHqdDequeueRequest, f::DEQUEUE_RESET)?;
        mes::wr(o, a, M::SpiComputeQueueReset, f::SPI_QUEUE_RESET)?;
        for _ in 0..us {
            now = mes::rd(o, a, M::CpHqdActive)?;
            if now & f::HQD_ACTIVE == 0 {
                break;
            }
            o.delay(1);
        }
    }
    Ok((found, now))
}

/// CPC's sticky halt bit with the queue gone (see the module text): written 0 if it reads set, the line says what it
/// read before and after. No answer is not a failure: the ring test after it decides.
#[inline(never)]
pub(super) fn cpc_release<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, d: &mut String) {
    let Ok(v) = mes::rd(owner, a, M::CpcUtcl1Error) else {
        d.push_str("CPC_UTCL1_ERROR did not answer; ");
        return;
    };
    if v & f::CPC_ERROR_HALT == 0 {
        d.push_str(&format!("CPC_UTCL1_ERROR {v:#x}; "));
        return;
    }
    let written = mes::wr(owner, a, M::CpcUtcl1Error, v & !f::CPC_ERROR_HALT).is_ok();
    let after = hx32(mes::rd(owner, a, M::CpcUtcl1Error).ok());
    klog::event!(Warn, "gx", "vm.cpc_halt_bit", found = klog::Hex(u64::from(v)), written = written; "CPC_UTCL1_ERROR's halt bit set after the queue went: written 0");
    d.push_str(&format!("CPC_UTCL1_ERROR {v:#x} (halt bit set), written 0{}: reads {after}; ", if written { "" } else { " REFUSED" }));
}

/// `mes::COMPUTE_*` for a later stop and until=vm; a slot that cannot be written is said, a stop then sends its REMOVE_QUEUE.
fn note_compute<H: Hw>(owner: &mut Bringup<Watch<H>>, l: &Layout, state: u64, d: &mut String) {
    if mes::mem(owner, l.compute_state, &state.to_le_bytes()).is_err() {
        klog::event!(Error, "gx", "vm.recover_note", state = state; "the compute ring's state slot was not written");
        d.push_str("the compute ring's state slot NOT written; ");
    }
}

/// MES answered REMOVE_QUEUE, so it holds no record of the queue, and the HQD is active or its state unknown: no firmware
/// owns it, and no stage may start the CP or its firmware again in this boot.
fn stuck<H: Hw>(owner: &mut Bringup<Watch<H>>, l: &Layout, r: &mut VmReport, d: &mut String, why: &'static str) -> &'static str {
    r.stranded = true;
    note_compute(owner, l, mes::COMPUTE_STUCK, d);
    klog::event!(Error, "gx", "vm.recover_stranded", why = why; "the compute ring's HQD is left with no firmware owning it");
    d.push_str("STRANDED: no firmware owns the HQD; ");
    why
}

/// The kernel compute ring: unmapped, its HQD off, its memory rewritten, mapped again (the module text). A failure
/// after the unmap leaves that on record (`mes::COMPUTE_UNMAPPED`, `COMPUTE_STUCK`).
#[inline(never)]
fn compute<H: Hw>(owner: &mut Bringup<Watch<H>>, inp: &VmInputs, r: &mut VmReport, d: &mut String) -> Result<Queue, &'static str> {
    let l = inp.mes;
    let a = inp.addrs.hqd.ok_or("no MES registers known")?;
    let (_, qtype, pipe, queue, db) = mes::LEGACY[0];
    let mut ring = vmproc::sched_ring(owner, &l);
    let mut dbs = Vec::new();
    let res = ring.submit(owner, &l, Frame::unmap_legacy(qtype, pipe, queue, db), inp.api_us, &mut dbs);
    r.doorbells.append(&mut dbs);
    match res {
        Ok((us, _)) => {
            d.push_str(&format!("MES REMOVE_QUEUE (unmap_legacy_queue) answered in {us} us; "));
            note_compute(owner, &l, mes::COMPUTE_UNMAPPED, d);
        }
        Err(e) => {
            r.stranded = true;
            klog::event!(Error, "gx", "vm.recover_unmap", error = format!("{e:?}").as_str(); "MES did not unmap the stalled compute ring");
            d.push_str(&format!("MES REMOVE_QUEUE FAILED {e:?}: STRANDED; "));
            return Err("MES did not unmap the queue");
        }
    }
    if mes::select(owner, &a, mes11::COMPUTE_ME, pipe, queue).is_err() {
        return Err(stuck(owner, &l, r, d, "GRBM_GFX_CNTL not written"));
    }
    let res = hqd_reset(owner, &a, inp.timeout_us);
    if mes::select(owner, &a, 0, 0, 0).is_err() {
        return Err(stuck(owner, &l, r, d, "GRBM_GFX_CNTL not written back"));
    }
    let Ok((found, now)) = res else {
        return Err(stuck(owner, &l, r, d, "the HQD registers did not answer"));
    };
    d.push_str(&format!("CP_HQD_ACTIVE after the unmap {found:#x}{}; ", if found & f::HQD_ACTIVE != 0 { format!(", reset (DEQUEUE_REQUEST 2, SPI_COMPUTE_QUEUE_RESET 1): now {now:#x}") } else { String::new() }));
    klog::event!(Info, "gx", "vm.recover_hqd", found = klog::Hex(u64::from(found)), now = klog::Hex(u64::from(now)));
    if now & f::HQD_ACTIVE != 0 {
        return Err(stuck(owner, &l, r, d, "the HQD stays active after the reset"));
    }
    cpc_release(owner, &a, d);
    let cp = l.compute_prop();
    let mq = PmQueue::new(l.compute_ring, mes::COMPUTE_RING_BYTES, Some(l.comp_rptr), l.comp_wptr, mes11::COMPUTE_DOORBELL_DW).map_err(|_| "no ring")?;
    mes::zero(owner, l.compute_ring, mes::COMPUTE_RING_BYTES).map_err(|_| "the ring's memory")?;
    mes::mem(owner, cp.mqd, &mesmqd::bytes(&mesmqd::compute_mqd(&cp))).map_err(|_| "the MQD's memory")?;
    mq.reset(owner).map_err(|_| "the pointer slots' memory")?;
    let mut ring = vmproc::sched_ring(owner, &l);
    let mut dbs = Vec::new();
    let res = ring.submit(owner, &l, Frame::map_legacy(qtype, pipe, queue, db, cp.mqd, cp.wptr), inp.api_us, &mut dbs);
    r.doorbells.append(&mut dbs);
    match res {
        Ok((us, _)) => {
            d.push_str(&format!("MQD and ring rewritten, wptr 0; MES ADD_QUEUE (map_legacy) answered in {us} us; "));
            note_compute(owner, &l, 0, d);
        }
        Err(e) => {
            r.stranded = true;
            note_compute(owner, &l, mes::COMPUTE_STUCK, d);
            klog::event!(Error, "gx", "vm.recover_map", error = format!("{e:?}").as_str(); "MES did not map the compute ring again");
            d.push_str(&format!("MES ADD_QUEUE FAILED {e:?}: STRANDED; "));
            return Err("MES did not map the queue again");
        }
    }
    Ok(Queue::Pm4 { q: mq, gfx: false })
}

/// Rewrites what `compute`, `process` or `sdma_queue` left behind, then a ring test on the fresh queue. The rung is
/// `which.recover_rung()`; it fails (`done` true) when a step did, and times out when the fresh queue does not run.
#[inline(never)]
pub(super) fn recover<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut Queue, which: Q, c: &Ctx, seq: u64, r: &mut VmReport) -> Rung {
    let name = which.recover_rung();
    let mut d = String::new();
    let t0 = owner.now_us();
    let fresh = match which {
        Q::Compute => compute(owner, c.inp, r, &mut d),
        Q::Proc => match c.proc {
            Some(s) => vmproc::again(owner, c.inp, &s, r, &mut d),
            None => Err("the process queue's frame is unknown"),
        },
        Q::Sdma => match c.inp.sdma {
            Some(s) => sdma::restart_stalled(owner, &s.addrs, &s.layout, s.burst_nop, c.inp.timeout_us)
                .map_err(|e| {
                    d.push_str(&format!("{e:?}; "));
                    "SDMA 0 did not restart"
                })
                .and_then(|t| {
                    d.push_str(&t);
                    d.push_str("; ");
                    let mut found = Vec::new();
                    vmring::sdma_queue(owner, &s, &mut found).map_err(|_| "SDMA 0 is not idle after its restart")
                }),
            None => Err("no SDMA queue known"),
        },
        Q::Gfx => Err("GFX ring 0 is not recovered: it must go on"),
    };
    let fail = |d: String, why: &str, us: u64| {
        klog::event!(Error, "gx", "vm.recover_failed", rung = name, why = why);
        Rung { name, pass: false, done: true, wptr: 0, rptr: None, fence: None, polls: 0, us, detail: format!("{d}{why}") }
    };
    let new = match fresh {
        Ok(n) => n,
        Err(why) => {
            let us = owner.now_us().saturating_sub(t0);
            return fail(d, why, us);
        }
    };
    *q = new;
    // A fault status the first clear left (the stalled client kept it set) goes now that the client is gone.
    let left = super::gmc::read_fault(owner, &c.a.hub);
    if left.status.is_some_and(|s| s != 0 && s != u32::MAX) {
        d.push_str(&format!("GFXHUB still held {} after the unmap: cleared; ", left.text()));
        let _ = vmring::clear_fault(owner, c.a, c.us);
    }
    if mes::mem(owner, c.fence, &[0; 8]).is_err() {
        let us = owner.now_us().saturating_sub(t0);
        return fail(d, "the fence slot's memory", us);
    }
    owner.store_barrier();
    let fence_gpu = c.fence_gpu;
    let w = q.submit(owner, 16, |g| g.emit_fence(fence_gpu, seq, false, false).map_err(|_: RingError| RingError::State), &mut r.doorbells);
    let w = match w {
        Ok(w) => w,
        Err(e) => {
            let us = owner.now_us().saturating_sub(t0);
            return fail(d, &format!("the ring test not submitted: {e:?}"), us);
        }
    };
    let fence = c.fence;
    let mut x = q.wait(owner, c.us, name, w, &mut |hw| mes::rd32(hw, fence) == Some(seq as u32));
    x.fence = mes::rd32(owner, fence).map(u64::from);
    let st = super::gmc::read_fault(owner, &c.a.hub);
    if x.pass {
        x.pass = st.status == Some(0);
    }
    let vmid = st.status.map_or(0, |s| Fault::decode(s).vmid);
    if which == Q::Proc {
        vmproc::refind(owner, c, r);
    }
    let state = state(owner, which, c.inp);
    x.detail = format!("{d}ring test: fence {seq} read {}; GFXHUB fault status {}{}; now {state}", super::mes::hx(x.fence), st.text(), if st.status.is_some_and(|s| s != 0) { format!(" (VMID {vmid})") } else { String::new() });
    x
}

/// GFXHUB's fault registers as this module's callers read them.
pub(super) fn hub_status<H: Hw>(owner: &mut Bringup<Watch<H>>, c: &Ctx) -> Option<u32> {
    c.a.hub.byte(R::FaultStatus).and_then(|b| owner.reg_read(b))
}
