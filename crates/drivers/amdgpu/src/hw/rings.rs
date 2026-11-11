//! GX-8b: a kernel queue as the job half drives it: its fence slot in the MES buffer (MC address, VMID 0, as until=vm
//! proved on the chip), the jobs on it, what the poll reads. Only the job thread touches these.

use super::chain::ChainOwner;
use super::frame::RingType;
use super::jobs::JobCfg;
use crate::gpucore::mes;
use crate::gpucore::vmring::{Queue, VmInputs, lay};
use crate::hw::vm::VmJob;
use crate::ipmod::{gfx11, mes11, mesapi};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::fence::{Half, read_split};
use amdgpu_fmt::ring::Kind;
use amdgpu_fmt::watch::{RingCfg, RingClass, RingId, Sample};
use drm::job::Submission;

/// A slot beside the fence, for a ring test that must not touch the fence (inside the 64 bytes `lay::FENCES` keeps per queue).
const TEST_SLOT: u64 = 0x10;
const TEST_VALUE: u32 = 0x4a0b_5e57;

/// A submission until the last of its jobs left the hardware: what it keeps alive (`Submission`'s contract).
pub(crate) struct Hold {
    pub sub: Submission,
}

/// A job whose frame is on a ring.
pub(crate) struct OnRing {
    pub seq: u64,
    pub hold: Arc<Hold>,
    /// Its index in the gang.
    pub member: usize,
    pub vm: Option<VmJob>,
}

/// A compute queue seen at a VM fault in its head job's VMID: it may have stopped there (CONTRACTS 6b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stall {
    pub at_us: u64,
    pub addr: u64,
    pub write: bool,
    pub vmid: u8,
    pub seq: u64,
    /// The queue's position when the count began.
    pub seen: Option<Sample>,
}

impl Stall {
    /// The queue stood still for `grace_us`: any move of its read pointer or fence starts the count again. A long IB shows
    /// no move either (the ring's pointer waits behind the IB's packet), so this tells a queue that went on from one that
    /// stopped, not a slow IB from a stopped one; a CPC fault is the CP's own access, which is why that is enough.
    pub fn stopped(&mut self, now_us: u64, s: Sample, grace_us: u64) -> bool {
        if self.seen != Some(s) {
            self.seen = Some(s);
            self.at_us = now_us;
            return false;
        }
        now_us.saturating_sub(self.at_us) >= grace_us
    }
}

pub(crate) struct HwRing {
    pub ty: RingType,
    pub id: RingId,
    pub queue: Queue,
    /// The fence slot's MC address.
    pub fence: u64,
    pub jobs: VecDeque<OnRing>,
    /// `ring->current_ctx`.
    pub current_ctx: Option<u64>,
    pub stall: Option<Stall>,
}

/// The fence slot of a queue in `inp`'s buffer.
pub fn fence_slot(inp: &VmInputs, ty: RingType) -> u64 {
    inp.carve.mc_base() + inp.mes_bo.offset + lay::FENCES + ty.index() as u64 * 0x40
}

/// The watch's configuration of a queue (gr-GR-3.md's table).
pub(crate) fn ring_cfg(ty: RingType, q: &Queue, cfg: &JobCfg) -> RingCfg {
    let class = match (ty, q.kind()) {
        (RingType::Gfx, _) => RingClass::GfxMesLegacy { queue_type: mesapi::qtype::GFX, pipe: mes11::GFX_PIPE, queue: mes11::GFX_QUEUE, doorbell_dw: gfx11::DOORBELL_DW },
        (RingType::Compute, _) => RingClass::ComputeMesLegacy { queue_type: mesapi::qtype::COMPUTE, pipe: mes11::COMPUTE_PIPE, queue: mes11::COMPUTE_QUEUE, doorbell_dw: mes11::COMPUTE_DOORBELL_DW },
        (RingType::Sdma, Kind::Sdma { burst_nop }) => RingClass::Sdma { burst_nop },
        (RingType::Sdma, Kind::Gfx) => RingClass::Sdma { burst_nop: false },
    };
    let mut c = RingCfg::for_780m(ty.name(), class, q.size_dw(), q.wptr(), q.size_dw() / q.kind().ring_align_dw());
    if let Some(t) = cfg.timeouts_us {
        c.timeout_us = t[ty.index()];
    }
    c
}

impl HwRing {
    /// A fresh timeline starts at 0: the slot too, and the one a ring test uses.
    pub fn clear_slots(hw: &mut dyn crate::gpucore::psp::Hw, fence: u64) -> bool {
        mes::mem(hw, fence, &[0; 8]).is_ok() && mes::mem(hw, fence + TEST_SLOT, &[0; 8]).is_ok()
    }

    /// The position the watch judges: the fence and the read pointer; `last`: the pointer when it does not answer.
    pub fn sample(&self, o: &mut dyn ChainOwner, last: u64) -> Option<Sample> {
        let hw = o.io();
        let fence = self.fence;
        let mut ok = true;
        let value = read_split(|h| {
            let at = match h {
                Half::Low => fence,
                Half::High => fence + 4,
            };
            mes::rd32(hw, at).unwrap_or_else(|| {
                ok = false;
                0
            })
        });
        if !ok {
            return None;
        }
        Some(Sample { rptr_hw: self.queue.rptr_hw(hw).unwrap_or(last), fence: value })
    }

    /// `amdgpu_fence_write` of `value`, then a fence on the rebuilt queue into the test slot, waited for at most `us`:
    /// the pointer it ends at, or why not.
    pub fn test(&mut self, o: &mut dyn ChainOwner, value: u64, us: u64) -> Result<u64, String> {
        let hw = o.io();
        let (fence, slot) = (self.fence, self.fence + TEST_SLOT);
        mes::mem(hw, fence, &value.to_le_bytes()).map_err(|e| alloc::format!("the fence slot: {e:?}"))?;
        mes::mem(hw, slot, &[0; 8]).map_err(|e| alloc::format!("the test slot: {e:?}"))?;
        hw.store_barrier();
        let mut bells = Vec::new();
        let w = self.queue.submit(hw, 16, |g| g.emit_fence(slot, u64::from(TEST_VALUE), false, false), &mut bells).map_err(|e| alloc::format!("not submitted: {e:?}"))?;
        let r = self.queue.wait(hw, us, "the job ring's test", w, &mut |h| mes::rd32(h, slot) == Some(TEST_VALUE));
        if r.pass { Ok(w) } else { Err(alloc::format!("no fence in {us} us: rptr {:?}", r.rptr)) }
    }
}
