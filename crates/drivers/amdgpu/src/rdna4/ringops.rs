//! Wave 44, item RG: a kernel ring as Linux's driver writes it once MES mapped it: `amdgpu_ring_write` into the
//! ring's system pages at the write pointer (wrapping at the ring's end, `buf_mask`), then `amdgpu_ring_commit`
//! (amdgpu_ring.c:169-189): the frame padded to the rings' fetch size of 256 dwords with
//! `gfx_v12_ring_insert_nop` (gfx_v12_0.c:5154-5167), a memory barrier and `set_wptr` (`cpring`). The write
//! pointer is 64 bits in dwords and never wraps (`support_64bit_ptrs`). No HDP flush lies between the frame and
//! the doorbell: the rings are system pages, which the CP reads through the GART.
//!
//! The ring's frames for Linux's IB test and fences (`amdgpu_ib_schedule`, amdgpu_ib.c:124-335, with no job):
//! on the gfx ring the conditional execution on the ring's `cond_exe` slot (which holds 1), the HDP flush, the
//! IB, the fence with its interrupt; on a compute ring the same without the conditional execution.

use super::host::Card;
use super::mesring::Place;
use super::meskq::KernelQueue;
use super::sdmaq::{QStop, sys_write};
use alloc::vec::Vec;
use amdgpu_fmt::encode::EncodeError;
use amdgpu_fmt::{gfx12, pm4};
use rdna4_core::gc12ring::{self as rg, FRAME_DW};
use rdna4_core::mes12;

/// The ring's size in dwords.
pub const RING_DW: u64 = mes12::KQ_RING_BYTES as u64 / 4;

pub fn enc(e: EncodeError) -> QStop {
    QStop::Ring(e.into())
}

/// `words` padded to the next 256-dword boundary from `wptr`, as `amdgpu_ring_commit` pads.
pub fn padded(wptr: u64, words: &[u32]) -> Vec<u32> {
    let mut v = words.to_vec();
    let count = (FRAME_DW - (wptr + v.len() as u64) % FRAME_DW) % FRAME_DW;
    gfx12::insert_nop(&mut v, count as u32);
    v
}

/// One frame on the queue's ring from `wptr`: written, padded, committed. The new write pointer.
pub fn commit(card: &mut dyn Card, p: &Place, q: &KernelQueue, wptr: u64, words: &[u32]) -> Result<u64, QStop> {
    let v = padded(wptr, words);
    let mut at = wptr;
    for chunk in v.chunks(256) {
        // Frames are whole multiples of 256 dwords from a 256-dword boundary and the ring holds eight of them:
        // a chunk never crosses the ring's end.
        let off = (at % RING_DW) * 4;
        let bytes: Vec<u8> = chunk.iter().flat_map(|d| d.to_le_bytes()).collect();
        sys_write(card, q.ring_sys + off, &bytes)?;
        at += chunk.len() as u64;
    }
    if !super::cpring::set_wptr(card, p.slot_sys(q.slots.wptr), q.kq.doorbell, at) {
        return Err(QStop::Doorbell { index_dw: q.kq.doorbell });
    }
    Ok(at)
}

/// `gfx_v12_0_ring_test_ring`'s three dwords: SET_UCONFIG_REG of 0xDEADBEEF into SCRATCH_REG0 (a dword offset).
pub fn ring_test(scratch_dw: u32) -> Result<Vec<u32>, QStop> {
    let mut v = Vec::new();
    gfx12::ring_test(&mut v, scratch_dw).map_err(enc)?;
    Ok(v)
}

/// `amdgpu_ib_schedule`'s frame for one IB of `len_dw` at `ib_mc` without a job, ending in the ring's fence
/// `seq` (32 bits, with its interrupt: `amdgpu_fence_emit` passes `AMDGPU_FENCE_FLAG_INT`), or a fence alone
/// when `ib` is `None`. `hdp`: NBIF's HDP flush request and done dwords.
pub fn ib_frame(p: &Place, q: &KernelQueue, ib: Option<(u64, u32)>, seq: u32, hdp: (u32, u32)) -> Result<Vec<u32>, QStop> {
    let mut v = Vec::new();
    // gfx_v12_0_ring_emit_init_cond_exec (:4677-4692): the count dword is patched once the frame is known.
    let cond = if q.kq.gfx {
        pm4::cond_exec(&mut v, p.slot_mc(q.slots.cond_exe), 0).map_err(enc)?;
        Some(v.len() - 1)
    } else {
        None
    };
    let (mask, engine) = rg::hdp_flush_mask(q.kq.gfx, q.kq.pipe);
    gfx12::hdp_flush(&mut v, engine, hdp.1, hdp.0, mask).map_err(enc)?;
    if let Some((mc, len)) = ib {
        if q.kq.gfx { gfx12::ib_gfx(&mut v, mc, len, 0) } else { gfx12::ib_compute(&mut v, mc, len, 0) }.map_err(enc)?;
    }
    gfx12::fence(&mut v, p.slot_mc(q.slots.fence), u64::from(seq), false, true).map_err(enc)?;
    // amdgpu_ring_patch_cond_exec (amdgpu_ring.h:543-553): the dwords from the count to the frame's last.
    if let Some(at) = cond {
        v[at] = (v.len() - 1 - at) as u32;
    }
    Ok(v)
}

/// `amdgpu_ring_clear_ring`: the ring filled with the filler NOP (`ring->funcs->nop`).
pub fn clear(card: &mut dyn Card, q: &KernelQueue) -> Result<(), QStop> {
    let bytes: Vec<u8> = (0..RING_DW).flat_map(|_| pm4::PAD.to_le_bytes()).collect();
    sys_write(card, q.ring_sys, &bytes)
}
