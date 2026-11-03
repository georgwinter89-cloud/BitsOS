//! GX-7k, `until=gfx_mes`: the kernel queues mapped through MES as Linux 7.1 does with legacy queue mapping
//! (firmware 0x47 and newer): gfx_v11_0_cp_resume (7.1 gfx_v11_0.c:4631) after amdgpu_mes_kiq_hw_init runs
//! kcq_resume (the compute MQD, gfx_v11_0_compute_mqd_init :4291; amdgpu_gfx_mes_enable_kcq: the KIQ's
//! SET_RESOURCES with its ring test, then MES ADD_QUEUE with map_legacy_kq for the compute ring), then
//! cp_async_gfx_ring_resume (the GFX MQD, gfx_v11_0_gfx_mqd_init :4118; amdgpu_gfx_enable_kgq: ADD_QUEUE for GFX
//! ring 0; cp_gfx_start: CP_MAX_CONTEXT, CP_DEVICE_ID, the clear-state preamble on the ring), then the ring tests
//! (GFX ring 0, then the compute ring) and, as amdgpu_ib_ring_tests later, the IB tests; then a RELEASE_MEM
//! end-of-pipe fence of a 64-bit value on each. One compute ring (comp_1.0.0: MEC 1 pipe 0 queue 0, doorbell
//! MEC_RING0) of Linux's eight; GFX ring 0's memory is the GFX stage's BO (`gfxring::Layout`), the compute ring's
//! this stage's. Every MES-mapped queue is committed through its doorbell (no MMIO write pointer).

use super::bringup::Bringup;
use super::gfxring;
use super::mes::{self, Layout, MesAddrs, MesError, MesInputs, MesReport, MesRing, PmQueue, Rung, api, dwords, kiq_test, mem, rd32, rd64, select, wrg, zero};
use super::psp::Hw;
use super::watch::Watch;
use crate::ipmod::gfx11::{self, R};
use crate::ipmod::mes11;
use crate::ipmod::mesapi::{self, Frame};
use crate::ipmod::mesmqd;
use alloc::format;
use alloc::vec::Vec;
use amdgpu_fmt::fence::Timeline;
use amdgpu_fmt::pm4::{self, RegSpace};
use amdgpu_fmt::ring::RingError;

/// The steps of `until=gfx_mes` after MES's own (also checks).
pub const STEPS: [&str; 3] = ["MES KIQ SET_RESOURCES and ring test", "MES map compute ring (ADD_QUEUE map_legacy_kq)", "MES map GFX ring 0 (ADD_QUEUE map_legacy_kq)"];
/// The rungs (also checks), in order.
pub const RUNGS: [&str; 6] = [
    "GFX (MES) ring test (SCRATCH_REG0)",
    "compute ring test (SCRATCH_REG0)",
    "GFX (MES) IB test (WRITE_DATA)",
    "GFX (MES) RELEASE_MEM 64-bit fence",
    "compute IB test (WRITE_DATA)",
    "compute RELEASE_MEM 64-bit fence",
];

fn ring_err(_: amdgpu_fmt::encode::EncodeError) -> MesError {
    MesError::Ring(RingError::State)
}

fn push(r: &mut MesReport, x: Rung) -> Result<(), MesError> {
    let (pass, done, name, rptr, want) = (x.pass, x.done, x.name, x.rptr, x.wptr);
    r.rungs.push(x);
    match (pass, done) {
        (true, _) => Ok(()),
        (false, true) => Err(MesError::Result { rung: name }),
        (false, false) => Err(MesError::Timeout { rung: name, rptr, want }),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, l: &Layout, g: &gfxring::Layout, kiq: &mut PmQueue, ring: &mut MesRing, inp: &MesInputs, r: &mut MesReport) -> Result<(), MesError> {
    let t = inp.timeout_us;
    // kcq_resume: the compute MQD (Linux selects the queue around amdgpu_ring_init_mqd), the ring's slots.
    let cp = l.compute_prop();
    select(owner, a, mes11::COMPUTE_ME, mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE)?;
    let m = mem(owner, l.compute_mqd, &mesmqd::bytes(&mesmqd::compute_mqd(&cp)));
    select(owner, a, 0, 0, 0)?;
    m?;
    let mut comp = PmQueue::new(cp.ring, cp.ring_bytes, Some(cp.rptr), cp.wptr, mes11::COMPUTE_DOORBELL_DW)?;
    comp.reset(owner)?;
    // amdgpu_gfx_mes_enable_kcq: the KIQ's SET_RESOURCES (every queue, no cleaner shader) and its ring test, then
    // the compute ring through MES.
    kiq_test(owner, a, kiq, t, r, STEPS[0], &mesapi::set_resources(u64::MAX, 0))?;
    let f = Frame::map_legacy(mesapi::qtype::COMPUTE, mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE, mes11::COMPUTE_DOORBELL_DW, cp.mqd, cp.wptr);
    api(owner, l, ring, inp.api_us, r, STEPS[1], f, format!("MEC 1 pipe {} queue {}, doorbell {:#x}, MQD MC {:#x}, ring MC {:#x} {} KiB", mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE, mes11::COMPUTE_DOORBELL_DW, cp.mqd, cp.ring, cp.ring_bytes >> 10))?;
    r.mapped.push(mes::LEGACY[0]);
    // cp_async_gfx_ring_resume: GFX ring 0 cleared, its MQD (the queue selected around it), then through MES.
    let gp = l.gfx_prop(g);
    select(owner, a, 0, mes11::GFX_PIPE, mes11::GFX_QUEUE)?;
    let m = mem(owner, l.gfx_mqd, &mesmqd::bytes(&mesmqd::gfx_mqd(&gp)));
    select(owner, a, 0, 0, 0)?;
    m?;
    zero(owner, gp.ring, gp.ring_bytes)?;
    let mut gq = PmQueue::new(gp.ring, gp.ring_bytes, Some(gp.rptr), gp.wptr, gfx11::DOORBELL_DW)?;
    gq.reset(owner)?;
    let f = Frame::map_legacy(mesapi::qtype::GFX, mes11::GFX_PIPE, mes11::GFX_QUEUE, gfx11::DOORBELL_DW, gp.mqd, gp.wptr);
    api(owner, l, ring, inp.api_us, r, STEPS[2], f, format!("ME 0 pipe {} queue {}, doorbell {:#x}, MQD MC {:#x}, ring MC {:#x} {} KiB", mes11::GFX_PIPE, mes11::GFX_QUEUE, gfx11::DOORBELL_DW, gp.mqd, gp.ring, gp.ring_bytes >> 10))?;
    r.mapped.push(mes::LEGACY[1]);
    // cp_gfx_start: the context count, the device id, the clear-state preamble.
    wrg(owner, a, R::CpMaxContext, gfx11::MAX_HW_CONTEXTS - 1)?;
    wrg(owner, a, R::CpDeviceId, 1)?;
    let csb = gfxring::csb();
    let pre = gq.submit(owner, csb.len() as u64, |x| x.write(&csb), &mut r.doorbells)?;
    // The ring tests: GFX ring 0, then the compute ring.
    ring_test(owner, a, &mut gq, t, RUNGS[0], r, Some(pre))?;
    ring_test(owner, a, &mut comp, t, RUNGS[1], r, None)?;
    // The IB tests and the 64-bit fences.
    let mut t32 = Timeline::new(0, 16);
    let mut t64g = Timeline::new(gfxring::FENCE64_START, 16);
    let mut t64c = Timeline::new(gfxring::FENCE64_START, 16);
    ib_test(owner, &mut gq, t, RUNGS[2], r, &mut t32, (g.ib, g.ib_target, g.fence_ib), false)?;
    fence64(owner, &mut gq, t, RUNGS[3], r, &mut t64g, g.fence64)?;
    ib_test(owner, &mut comp, t, RUNGS[4], r, &mut t32, (l.ib, l.comp_ib_target, l.comp_fence_ib), true)?;
    fence64(owner, &mut comp, t, RUNGS[5], r, &mut t64c, l.comp_fence64)?;
    Ok(())
}

/// Linux's ring test (gfx_v11_0_ring_test_ring): SCRATCH_REG0 poisoned 0xCAFEDEAD, SET_UCONFIG_REG of 0xDEADBEEF
/// from the ring, the rptr writeback at the write pointer.
#[allow(clippy::too_many_arguments)]
fn ring_test<H: Hw>(owner: &mut Bringup<Watch<H>>, a: &MesAddrs, q: &mut PmQueue, t: u64, name: &'static str, r: &mut MesReport, preamble: Option<u64>) -> Result<(), MesError> {
    let sb = a.gfx(R::ScratchReg0).ok_or(MesError::NoAddress)?;
    let sdw = sb / 4;
    wrg(owner, a, R::ScratchReg0, gfx11::POISON)?;
    let w = q.submit(owner, 3, |g| g.emit(|v| pm4::set_reg(v, RegSpace::Uconfig, sdw, &[gfx11::RING_TEST])), &mut r.doorbells)?;
    let mut x = q.wait(owner, t, name, w, &mut |hw| hw.reg_read(sb) == Some(gfx11::RING_TEST));
    let got = owner.reg_read(sb);
    x.detail = format!(
        "{}doorbell {:#x}; SCRATCH_REG0 (PM4 register {sdw:#x}) reads {}",
        preamble.map_or(alloc::string::String::new(), |p| format!("after the clear-state preamble (wptr {p}, {} dwords); ", gfxring::CSB_DW)),
        q.doorbell,
        mes::hx32(got)
    );
    push(r, x)
}

/// Linux's IB test (gfx_v11_0_ring_test_ib): an IB with WRITE_DATA (dst_sel 5, confirmed) of 0xDEADBEEF into a
/// carve-out dword, then a 32-bit fence. Compute IBs carry INDIRECT_BUFFER_VALID (gfx_v11_0.c emit_ib_compute).
#[allow(clippy::too_many_arguments)]
fn ib_test<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut PmQueue, t: u64, name: &'static str, r: &mut MesReport, t32: &mut Timeline, (ib_mc, target, fence): (u64, u64, u64), compute: bool) -> Result<(), MesError> {
    let mut ib = Vec::new();
    pm4::write_data(&mut ib, pm4::dst_sel::MEM, target, pm4::WR_CONFIRM, &[gfx11::RING_TEST]).map_err(ring_err)?;
    mem(owner, ib_mc, &dwords(&ib))?;
    mem(owner, target, &gfx11::POISON.to_le_bytes())?;
    mem(owner, fence, &[0; 8])?;
    owner.store_barrier();
    let seq = t32.emit().map_err(|_| MesError::Ring(RingError::State))?;
    let n = ib.len() as u32;
    let w = q.submit(
        owner,
        16,
        |g| {
            if compute {
                g.emit(|v| pm4::indirect_buffer(v, ib_mc, n, 0, false, pm4::IB_VALID))?;
            } else {
                g.emit_ib(0, ib_mc, n, 0, false)?;
            }
            g.emit_fence(fence, seq, false, false)
        },
        &mut r.doorbells,
    )?;
    let mut x = q.wait(owner, t, name, w, &mut |hw| rd32(hw, fence) == Some(seq as u32));
    x.fence = rd32(owner, fence).map(u64::from);
    let got = rd32(owner, target);
    if x.pass {
        t32.observe(seq);
        x.pass = got == Some(gfx11::RING_TEST);
    }
    x.detail = format!("IB at MC {ib_mc:#x} ({n} dwords), target MC {target:#x} reads {}, fence {seq} read {}", mes::hx32(got), mes::hx(x.fence));
    push(r, x)
}

/// M5 on a MES-mapped queue: RELEASE_MEM end-of-pipe of a 64-bit value that moves both halves.
fn fence64<H: Hw>(owner: &mut Bringup<Watch<H>>, q: &mut PmQueue, t: u64, name: &'static str, r: &mut MesReport, t64: &mut Timeline, at: u64) -> Result<(), MesError> {
    mem(owner, at, &gfxring::FENCE64_START.to_le_bytes())?;
    owner.store_barrier();
    let seq = t64.emit().map_err(|_| MesError::Ring(RingError::State))?;
    let w = q.submit(owner, 8, |g| g.emit_fence(at, seq, true, false), &mut r.doorbells)?;
    let mut x = q.wait(owner, t, name, w, &mut |hw| rd64(hw, at) == Some(seq));
    x.fence = rd64(owner, at);
    if x.pass {
        t64.observe(seq);
    }
    x.detail = format!("RELEASE_MEM (every cache written back and invalidated, DATA_SEL 2) to MC {at:#x}: {:#x} -> {seq:#x}, read {}", gfxring::FENCE64_START, mes::hx(x.fence));
    push(r, x)
}
