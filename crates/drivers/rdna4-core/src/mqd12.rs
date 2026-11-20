//! Wave 44, item ME: the memory queue descriptors of GFX 12, `struct v12_compute_mqd` and `struct v12_gfx_mqd`
//! (A:../include/v12_structs.h, 512 dwords each), as Linux fills them: the MES pipes' (`mes_v12_0_mqd_init`,
//! A:mes_v12_0.c:1486-1593), the compute rings' (`gfx_v12_0_compute_mqd_init`, A:gfx_v12_0.c:3178-3318) and the gfx
//! ring's (`gfx_v12_0_gfx_mqd_init` :3003-3100). An MQD lives in system memory behind the GART; the CP (or MES,
//! for the queues it maps) reads it from there. The dword offsets are the header's (its comments say "offset: N"),
//! held against the header itself by the C compiler in `tests/mes12_offsets.rs`.

use crate::mes12::{self, default, f};
use crate::nbreg::Field;
use alloc::vec::Vec;

pub const MQD_DW: usize = 512;
pub const MQD_BYTES: u32 = MQD_DW as u32 * 4;
/// `mqd->header` of a compute MQD.
pub const HEADER: u32 = 0xC031_0800;

/// `struct v12_compute_mqd`'s fields Linux sets, as dword indices.
pub mod c {
    pub const HEADER: usize = 0;
    pub const COMPUTE_PIPELINESTAT_ENABLE: usize = 11;
    pub const STATIC_THREAD_MGMT_SE0: usize = 23;
    pub const STATIC_THREAD_MGMT_SE1: usize = 24;
    pub const STATIC_THREAD_MGMT_SE2: usize = 26;
    pub const STATIC_THREAD_MGMT_SE3: usize = 27;
    pub const COMPUTE_MISC_RESERVED: usize = 32;
    pub const MQD_BASE_ADDR_LO: usize = 128;
    pub const MQD_BASE_ADDR_HI: usize = 129;
    pub const HQD_ACTIVE: usize = 130;
    pub const HQD_VMID: usize = 131;
    pub const HQD_PERSISTENT_STATE: usize = 132;
    pub const HQD_PIPE_PRIORITY: usize = 133;
    pub const HQD_QUEUE_PRIORITY: usize = 134;
    pub const HQD_QUANTUM: usize = 135;
    pub const HQD_PQ_BASE_LO: usize = 136;
    pub const HQD_PQ_BASE_HI: usize = 137;
    pub const HQD_PQ_RPTR: usize = 138;
    pub const HQD_PQ_RPTR_REPORT_ADDR_LO: usize = 139;
    pub const HQD_PQ_RPTR_REPORT_ADDR_HI: usize = 140;
    pub const HQD_PQ_WPTR_POLL_ADDR_LO: usize = 141;
    pub const HQD_PQ_WPTR_POLL_ADDR_HI: usize = 142;
    pub const HQD_PQ_DOORBELL_CONTROL: usize = 143;
    pub const HQD_PQ_CONTROL: usize = 145;
    pub const HQD_IB_CONTROL: usize = 149;
    pub const HQD_IQ_TIMER: usize = 150;
    pub const HQD_DEQUEUE_REQUEST: usize = 152;
    pub const MQD_CONTROL: usize = 162;
    pub const HQD_EOP_BASE_ADDR_LO: usize = 165;
    pub const HQD_EOP_BASE_ADDR_HI: usize = 166;
    pub const HQD_EOP_CONTROL: usize = 167;
    pub const HQD_PQ_WPTR_LO: usize = 182;
    pub const HQD_PQ_WPTR_HI: usize = 183;
    pub const RESERVED_184: usize = 184;
    pub const FENCE_ADDRESS_LO: usize = 446;
    pub const FENCE_ADDRESS_HI: usize = 447;
}

/// `struct v12_gfx_mqd`'s fields Linux sets, as dword indices.
pub mod g {
    pub const SHADOW_BASE_LO: usize = 0;
    pub const SHADOW_BASE_HI: usize = 1;
    pub const FW_WORK_AREA_BASE_LO: usize = 4;
    pub const FW_WORK_AREA_BASE_HI: usize = 5;
    pub const MQD_BASE_ADDR: usize = 128;
    pub const MQD_BASE_ADDR_HI: usize = 129;
    pub const HQD_ACTIVE: usize = 130;
    pub const HQD_VMID: usize = 131;
    pub const HQD_QUEUE_PRIORITY: usize = 134;
    pub const HQD_QUANTUM: usize = 135;
    pub const HQD_BASE: usize = 136;
    pub const HQD_BASE_HI: usize = 137;
    pub const HQD_RPTR: usize = 138;
    pub const HQD_RPTR_ADDR: usize = 139;
    pub const HQD_RPTR_ADDR_HI: usize = 140;
    pub const RB_WPTR_POLL_ADDR_LO: usize = 141;
    pub const RB_WPTR_POLL_ADDR_HI: usize = 142;
    pub const RB_DOORBELL_CONTROL: usize = 143;
    pub const HQD_CNTL: usize = 145;
    pub const HQD_WPTR: usize = 149;
    pub const HQD_WPTR_HI: usize = 150;
    pub const MQD_CONTROL: usize = 162;
    pub const FENCE_ADDRESS_LO: usize = 510;
    pub const FENCE_ADDRESS_HI: usize = 511;
}

/// What a queue's MQD is filled from (Linux's `amdgpu_mqd_prop` / the ring's fields): GPU (GART) addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prop {
    pub mqd: u64,
    pub ring: u64,
    pub ring_bytes: u32,
    pub rptr: u64,
    pub wptr: u64,
    /// The EOP buffer (compute and MES; none for gfx).
    pub eop: u64,
    pub doorbell: u32,
    /// `amdgpu_gfx_is_high_priority_compute_queue` (A:amdgpu_gfx.c:195-206): the first compute ring when there is
    /// more than one. It gives the queue `AMDGPU_GFX_PIPE_PRIO_HIGH` and `AMDGPU_GFX_QUEUE_PRIORITY_MAXIMUM`
    /// (A:amdgpu_ring.c:849-853).
    pub high_priority: bool,
}

fn lo(v: u64) -> u32 {
    v as u32
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

/// `mes_v12_0_mqd_init` (A:mes_v12_0.c:1486-1593) into a zeroed MQD (`mes_v12_0_mqd_sw_init` clears it once,
/// :1806).
pub fn mes(p: &Prop) -> Vec<u32> {
    let mut m = alloc::vec![0u32; MQD_DW];
    m[c::HEADER] = HEADER;
    m[c::COMPUTE_PIPELINESTAT_ENABLE] = 1;
    for at in [c::STATIC_THREAD_MGMT_SE0, c::STATIC_THREAD_MGMT_SE1, c::STATIC_THREAD_MGMT_SE2, c::STATIC_THREAD_MGMT_SE3] {
        m[at] = 0xFFFF_FFFF;
    }
    m[c::COMPUTE_MISC_RESERVED] = 7;
    let eop = p.eop >> 8;
    (m[c::HQD_EOP_BASE_ADDR_LO], m[c::HQD_EOP_BASE_ADDR_HI]) = (lo(eop), hi(eop));
    m[c::HQD_EOP_CONTROL] = mes12::eop_control(mes12::EOP_BYTES);
    (m[c::HQD_PQ_RPTR], m[c::HQD_PQ_WPTR_LO], m[c::HQD_PQ_WPTR_HI]) = (0, 0, 0);
    (m[c::MQD_BASE_ADDR_LO], m[c::MQD_BASE_ADDR_HI]) = (lo(p.mqd) & 0xFFFF_FFFC, hi(p.mqd));
    m[c::MQD_CONTROL] = f::MQD_VMID.put(default::CP_MQD_CONTROL, 0);
    let base = p.ring >> 8;
    (m[c::HQD_PQ_BASE_LO], m[c::HQD_PQ_BASE_HI]) = (lo(base), hi(base));
    (m[c::HQD_PQ_RPTR_REPORT_ADDR_LO], m[c::HQD_PQ_RPTR_REPORT_ADDR_HI]) = (lo(p.rptr) & 0xFFFF_FFFC, hi(p.rptr) & 0xFFFF);
    (m[c::HQD_PQ_WPTR_POLL_ADDR_LO], m[c::HQD_PQ_WPTR_POLL_ADDR_HI]) = (lo(p.wptr) & 0xFFFF_FFF8, hi(p.wptr) & 0xFFFF);
    m[c::HQD_PQ_CONTROL] = mes12::mes_pq_control(p.ring_bytes);
    m[c::HQD_PQ_DOORBELL_CONTROL] = mes12::doorbell_enabled(p.doorbell);
    (m[c::HQD_VMID], m[c::HQD_ACTIVE]) = (0, 1);
    m[c::HQD_PERSISTENT_STATE] = mes12::persistent_state();
    m[c::HQD_IB_CONTROL] = default::CP_HQD_IB_CONTROL;
    m[c::HQD_IQ_TIMER] = default::CP_HQD_IQ_TIMER;
    m[c::HQD_QUANTUM] = default::CP_HQD_QUANTUM;
    // "Set CP_HQD_GFX_CONTROL.DB_UPDATED_MSG_EN[15] to enable unmapped doorbell handling" (:1588-1591).
    m[c::RESERVED_184] = 1 << 15;
    m
}

/// The register values `mes_v12_0_queue_init_register` takes from an MQD (:1595-1655).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HqdFromMqd {
    pub mqd_lo: u32,
    pub mqd_hi: u32,
    pub pq_base_lo: u32,
    pub pq_base_hi: u32,
    pub rptr_lo: u32,
    pub rptr_hi: u32,
    pub pq_control: u32,
    pub wptr_lo: u32,
    pub wptr_hi: u32,
    pub doorbell: u32,
    pub persistent: u32,
    pub active: u32,
}

impl HqdFromMqd {
    pub fn of(m: &[u32]) -> Option<HqdFromMqd> {
        let at = |i: usize| m.get(i).copied();
        Some(HqdFromMqd {
            mqd_lo: at(c::MQD_BASE_ADDR_LO)?,
            mqd_hi: at(c::MQD_BASE_ADDR_HI)?,
            pq_base_lo: at(c::HQD_PQ_BASE_LO)?,
            pq_base_hi: at(c::HQD_PQ_BASE_HI)?,
            rptr_lo: at(c::HQD_PQ_RPTR_REPORT_ADDR_LO)?,
            rptr_hi: at(c::HQD_PQ_RPTR_REPORT_ADDR_HI)?,
            pq_control: at(c::HQD_PQ_CONTROL)?,
            wptr_lo: at(c::HQD_PQ_WPTR_POLL_ADDR_LO)?,
            wptr_hi: at(c::HQD_PQ_WPTR_POLL_ADDR_HI)?,
            doorbell: at(c::HQD_PQ_DOORBELL_CONTROL)?,
            persistent: at(c::HQD_PERSISTENT_STATE)?,
            active: at(c::HQD_ACTIVE)?,
        })
    }
}

// The fields of the compute queue's registers that gfx_v12_0.c sets besides the MES ones (gc_12_0_0_sh_mask.h,
// held against the generated numbers in rdna4-regs).
pub const MIN_IB_AVAIL_SIZE: Field = Field::new(20, 0x0030_0000);
pub const QUANTUM_EN: Field = Field::new(0, 0x0000_0001);
pub const QUANTUM_SCALE: Field = Field::new(4, 0x0000_0010);
pub const QUANTUM_DURATION: Field = Field::new(8, 0x0000_3F00);
/// `AMDGPU_GFX_PIPE_PRIO_HIGH`, `AMDGPU_GFX_QUEUE_PRIORITY_MAXIMUM` (A:amdgpu_gfx.h).
pub const PIPE_PRIO_HIGH: u32 = 2;
pub const QUEUE_PRIORITY_MAXIMUM: u32 = 15;

/// `gfx_v12_0_compute_mqd_init` (A:gfx_v12_0.c:3178-3318) for a kernel queue, into the MQD
/// `gfx_v12_0_kcq_init_queue` cleared (:3477). `hqd_active` is 0: only the KIQ's is active (A:amdgpu_ring.c:847).
pub fn compute(p: &Prop) -> Vec<u32> {
    let mut m = alloc::vec![0u32; MQD_DW];
    m[c::HEADER] = HEADER;
    m[c::COMPUTE_PIPELINESTAT_ENABLE] = 1;
    for at in [c::STATIC_THREAD_MGMT_SE0, c::STATIC_THREAD_MGMT_SE1, c::STATIC_THREAD_MGMT_SE2, c::STATIC_THREAD_MGMT_SE3] {
        m[at] = 0xFFFF_FFFF;
    }
    m[c::COMPUTE_MISC_RESERVED] = 7;
    let eop = p.eop >> 8;
    (m[c::HQD_EOP_BASE_ADDR_LO], m[c::HQD_EOP_BASE_ADDR_HI]) = (lo(eop), hi(eop));
    m[c::HQD_EOP_CONTROL] = mes12::eop_control(mes12::MEC_HPD_BYTES);
    // Linux sets the doorbell control twice (:3205-3222 and :3270-3285); the second, the same value, stands.
    m[c::HQD_PQ_DOORBELL_CONTROL] = mes12::doorbell_enabled(p.doorbell);
    (m[c::HQD_DEQUEUE_REQUEST], m[c::HQD_PQ_RPTR], m[c::HQD_PQ_WPTR_LO], m[c::HQD_PQ_WPTR_HI]) = (0, 0, 0, 0);
    (m[c::MQD_BASE_ADDR_LO], m[c::MQD_BASE_ADDR_HI]) = (lo(p.mqd) & 0xFFFF_FFFC, hi(p.mqd));
    m[c::MQD_CONTROL] = f::MQD_VMID.put(default::CP_MQD_CONTROL, 0);
    let base = p.ring >> 8;
    (m[c::HQD_PQ_BASE_LO], m[c::HQD_PQ_BASE_HI]) = (lo(base), hi(base));
    // Here RPTR_BLOCK_SIZE gets the plain value (:3247-3248), unlike the MES's.
    let mut t = f::QUEUE_SIZE.put(default::CP_HQD_PQ_CONTROL, mes12::log2_less_one(p.ring_bytes / 4));
    t = f::RPTR_BLOCK_SIZE.put(t, mes12::log2_less_one(4096 / 4));
    t = f::TUNNEL_DISPATCH.put(f::UNORD_DISPATCH.put(t, 1), 0);
    m[c::HQD_PQ_CONTROL] = f::KMD_QUEUE.put(f::PRIV_STATE.put(t, 1), 1);
    (m[c::HQD_PQ_RPTR_REPORT_ADDR_LO], m[c::HQD_PQ_RPTR_REPORT_ADDR_HI]) = (lo(p.rptr) & 0xFFFF_FFFC, hi(p.rptr) & 0xFFFF);
    (m[c::HQD_PQ_WPTR_POLL_ADDR_LO], m[c::HQD_PQ_WPTR_POLL_ADDR_HI]) = (lo(p.wptr) & 0xFFFF_FFFC, hi(p.wptr) & 0xFFFF);
    m[c::HQD_VMID] = 0;
    m[c::HQD_PERSISTENT_STATE] = mes12::persistent_state();
    m[c::HQD_IB_CONTROL] = MIN_IB_AVAIL_SIZE.put(default::CP_HQD_IB_CONTROL, 3);
    if p.high_priority {
        (m[c::HQD_PIPE_PRIORITY], m[c::HQD_QUEUE_PRIORITY]) = (PIPE_PRIO_HIGH, QUEUE_PRIORITY_MAXIMUM);
    }
    m[c::HQD_QUANTUM] = QUANTUM_DURATION.put(QUANTUM_SCALE.put(QUANTUM_EN.put(0, 1), 1), 1);
    m[c::HQD_ACTIVE] = 0;
    m
}

// The gfx queue's registers: the defaults gfx_v12_0.c:55-61 defines itself (GFX 12's own, not GFX 11's), the
// fields gc_12_0_0_sh_mask.h's.
pub const GFX_MQD_CONTROL_DEFAULT: u32 = 0x0000_0100;
pub const GFX_HQD_QUANTUM_DEFAULT: u32 = 0x0000_0A01;
pub const GFX_HQD_CNTL_DEFAULT: u32 = 0x00F0_0000;
pub const GFX_PRIV_STATE: Field = Field::new(8, 0x0000_0100);
pub const GFX_CACHE_POLICY: Field = Field::new(24, 0x0300_0000);
pub const RB_BUFSZ: Field = Field::new(0, 0x0000_003F);
pub const RB_BLKSZ: Field = Field::new(8, 0x0000_3F00);
pub const GFX_KMD_QUEUE: Field = Field::new(29, 0x2000_0000);

/// `gfx_v12_0_gfx_mqd_init` (A:gfx_v12_0.c:3003-3100) for the kernel's gfx ring, into the MQD
/// `gfx_v12_0_kgq_init_queue` cleared (:3109). A kernel queue has no shadow, no work area and no user fence
/// (`amdgpu_ring_to_mqd_prop` sets none of them, A:amdgpu_ring.c:823-854): those fields stay 0.
pub fn gfx(p: &Prop) -> Vec<u32> {
    let mut m = alloc::vec![0u32; MQD_DW];
    (m[g::HQD_WPTR], m[g::HQD_WPTR_HI]) = (0, 0);
    (m[g::MQD_BASE_ADDR], m[g::MQD_BASE_ADDR_HI]) = (lo(p.mqd) & 0xFFFF_FFFC, hi(p.mqd));
    m[g::MQD_CONTROL] = GFX_CACHE_POLICY.put(GFX_PRIV_STATE.put(f::MQD_VMID.put(GFX_MQD_CONTROL_DEFAULT, 0), 1), 0);
    m[g::HQD_VMID] = 0;
    m[g::HQD_QUEUE_PRIORITY] = 0;
    m[g::HQD_QUANTUM] = QUANTUM_EN.put(GFX_HQD_QUANTUM_DEFAULT, 1);
    let base = p.ring >> 8;
    (m[g::HQD_BASE], m[g::HQD_BASE_HI]) = (lo(base), hi(base));
    (m[g::HQD_RPTR_ADDR], m[g::HQD_RPTR_ADDR_HI]) = (lo(p.rptr) & 0xFFFF_FFFC, hi(p.rptr) & 0xFFFF);
    (m[g::RB_WPTR_POLL_ADDR_LO], m[g::RB_WPTR_POLL_ADDR_HI]) = (lo(p.wptr) & 0xFFFF_FFFC, hi(p.wptr) & 0xFFFF);
    let bufsz = mes12::log2_less_one(p.ring_bytes / 4);
    m[g::HQD_CNTL] = GFX_KMD_QUEUE.put(RB_BLKSZ.put(RB_BUFSZ.put(GFX_HQD_CNTL_DEFAULT, bufsz), bufsz - 2), 1);
    m[g::RB_DOORBELL_CONTROL] = f::DOORBELL_EN.put(f::DOORBELL_OFFSET.put(0, p.doorbell), 1);
    m[g::HQD_RPTR] = 0;
    m[g::HQD_ACTIVE] = 1;
    m
}
