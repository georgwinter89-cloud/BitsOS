//! GX-7k: the MQDs (memory queue descriptors) of GC 11 as Linux 7.1 fills them: the MES pipes' own queues
//! (mes_v11_0_mqd_init, 7.1 mes_v11_0.c:1110), a compute ring (gfx_v11_0_compute_mqd_init, 7.1 gfx_v11_0.c:4291)
//! and a GFX ring (gfx_v11_0_gfx_mqd_init, :4118). Dword offsets are those of `struct v11_compute_mqd` and
//! `struct v11_gfx_mqd` (Linux 7.1 include/v11_structs.h, MIT, AMD; computed with offsetof, 512 dwords each).
//! The firmware reads them from memory (KIQ MAP_QUEUES, MES ADD_QUEUE) or the driver copies the fields into the
//! HQD registers (`queue_init_register`).

use super::mes11::{EOP_BYTES, MQD_DW, dflt, f, order_base_2};
use alloc::vec;
use alloc::vec::Vec;

/// `struct v11_compute_mqd` offsets (dwords).
pub mod c {
    pub const HEADER: usize = 0;
    pub const PIPELINESTAT_ENABLE: usize = 11;
    pub const STATIC_THREAD_MGMT_SE0: usize = 23;
    pub const STATIC_THREAD_MGMT_SE1: usize = 24;
    pub const STATIC_THREAD_MGMT_SE2: usize = 26;
    pub const STATIC_THREAD_MGMT_SE3: usize = 27;
    pub const MISC_RESERVED: usize = 32;
    pub const MQD_BASE_LO: usize = 128;
    pub const MQD_BASE_HI: usize = 129;
    pub const HQD_ACTIVE: usize = 130;
    pub const HQD_VMID: usize = 131;
    pub const PERSISTENT_STATE: usize = 132;
    pub const PIPE_PRIORITY: usize = 133;
    pub const QUEUE_PRIORITY: usize = 134;
    pub const QUANTUM: usize = 135;
    pub const PQ_BASE_LO: usize = 136;
    pub const PQ_BASE_HI: usize = 137;
    pub const PQ_RPTR: usize = 138;
    pub const RPTR_REPORT_LO: usize = 139;
    pub const RPTR_REPORT_HI: usize = 140;
    pub const WPTR_POLL_LO: usize = 141;
    pub const WPTR_POLL_HI: usize = 142;
    pub const DOORBELL_CONTROL: usize = 143;
    pub const PQ_CONTROL: usize = 145;
    pub const IB_CONTROL: usize = 149;
    pub const IQ_TIMER: usize = 150;
    pub const DEQUEUE_REQUEST: usize = 152;
    pub const MQD_CONTROL: usize = 162;
    pub const EOP_BASE_LO: usize = 165;
    pub const EOP_BASE_HI: usize = 166;
    pub const EOP_CONTROL: usize = 167;
    pub const PQ_WPTR_LO: usize = 182;
    pub const PQ_WPTR_HI: usize = 183;
}

/// `struct v11_gfx_mqd` offsets (dwords).
pub mod g {
    pub const HQD_ACTIVE: usize = 130;
    pub const HQD_VMID: usize = 131;
    pub const MQD_BASE: usize = 128;
    pub const MQD_BASE_HI: usize = 129;
    pub const QUEUE_PRIORITY: usize = 134;
    pub const QUANTUM: usize = 135;
    pub const HQD_BASE: usize = 136;
    pub const HQD_BASE_HI: usize = 137;
    pub const HQD_RPTR: usize = 138;
    pub const RPTR_ADDR: usize = 139;
    pub const RPTR_ADDR_HI: usize = 140;
    pub const WPTR_POLL_LO: usize = 141;
    pub const WPTR_POLL_HI: usize = 142;
    pub const DOORBELL_CONTROL: usize = 143;
    pub const HQD_CNTL: usize = 145;
    pub const HQD_WPTR: usize = 149;
    pub const HQD_WPTR_HI: usize = 150;
    pub const MQD_CONTROL: usize = 162;
}

/// What an MQD describes (amdgpu_mqd_prop's fields this driver uses; MC addresses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prop {
    pub mqd: u64,
    pub ring: u64,
    pub ring_bytes: u64,
    pub rptr: u64,
    pub wptr: u64,
    pub eop: u64,
    /// The doorbell's dword index (`None`: no doorbell).
    pub doorbell_dw: Option<u32>,
    /// CP_HQD_ACTIVE in the MQD (Linux: only the KIQ's, amdgpu_ring_to_mqd_prop).
    pub active: bool,
}

fn lo(x: u64) -> u32 {
    x as u32
}
fn hi(x: u64) -> u32 {
    (x >> 32) as u32
}

/// The doorbell control of a compute HQD: OFFSET (the dword index), EN; SOURCE and HIT 0.
pub fn hqd_doorbell(db: Option<u32>) -> u32 {
    match db {
        Some(i) => ((i << 2) & f::DB_OFFSET_MASK) | f::DB_EN,
        None => 0,
    }
}

/// CP_HQD_PQ_CONTROL of the MES pipes' queues (mes_v11_0_mqd_init, 7.1): QUEUE_SIZE, RPTR_BLOCK_SIZE as Linux
/// computes it (`(order_base_2(4096 / 4) - 1) << 8` given to REG_SET_FIELD, which masks it to 0: the capture's
/// 0xd8308011), UNORD_DISPATCH, PRIV_STATE, KMD_QUEUE, NO_UPDATE_RPTR.
pub fn mes_pq_control(ring_bytes: u64) -> u32 {
    let qs = order_base_2(ring_bytes / 4).saturating_sub(1);
    let block = (order_base_2(4096 / 4) - 1) << 8;
    let mut t = (dflt::CP_HQD_PQ_CONTROL & !f::PQ_QUEUE_SIZE_MASK) | (qs & f::PQ_QUEUE_SIZE_MASK);
    t = (t & !f::PQ_RPTR_BLOCK_SIZE_MASK) | ((block << f::PQ_RPTR_BLOCK_SIZE_SHIFT) & f::PQ_RPTR_BLOCK_SIZE_MASK);
    (t & !f::PQ_TUNNEL_DISPATCH) | f::PQ_UNORD_DISPATCH | f::PQ_PRIV_STATE | f::PQ_KMD_QUEUE | f::PQ_NO_UPDATE_RPTR
}

/// CP_HQD_PQ_CONTROL of a kernel compute ring (gfx_v11_0_compute_mqd_init, 7.1): QUEUE_SIZE, RPTR_BLOCK_SIZE 9
/// (a 4 KiB block), UNORD_DISPATCH, no tunneling, PRIV_STATE and KMD_QUEUE (a kernel queue).
pub fn compute_pq_control(ring_bytes: u64) -> u32 {
    let qs = order_base_2(ring_bytes / 4).saturating_sub(1);
    let block = order_base_2(4096 / 4) - 1;
    let mut t = (dflt::CP_HQD_PQ_CONTROL & !f::PQ_QUEUE_SIZE_MASK) | (qs & f::PQ_QUEUE_SIZE_MASK);
    t = (t & !f::PQ_RPTR_BLOCK_SIZE_MASK) | ((block << f::PQ_RPTR_BLOCK_SIZE_SHIFT) & f::PQ_RPTR_BLOCK_SIZE_MASK);
    (t & !f::PQ_TUNNEL_DISPATCH) | f::PQ_UNORD_DISPATCH | f::PQ_PRIV_STATE | f::PQ_KMD_QUEUE
}

/// CP_HQD_EOP_CONTROL: EOP_SIZE = order_base_2(EOP bytes / 4) - 1 (the register value is 2^(EOP_SIZE+1) dwords).
pub fn eop_control() -> u32 {
    (dflt::CP_HQD_EOP_CONTROL & !f::EOP_SIZE_MASK) | (order_base_2(EOP_BYTES / 4) - 1)
}

/// CP_HQD_PERSISTENT_STATE with PRELOAD_SIZE 0x55.
pub fn persistent_state() -> u32 {
    (dflt::CP_HQD_PERSISTENT_STATE & !f::PRELOAD_SIZE_MASK) | (0x55 << f::PRELOAD_SIZE_SHIFT)
}

/// The compute MQD fields both builders share.
fn compute_common(m: &mut [u32], p: &Prop) {
    m[c::HEADER] = 0xC031_0800;
    m[c::PIPELINESTAT_ENABLE] = 1;
    for i in [c::STATIC_THREAD_MGMT_SE0, c::STATIC_THREAD_MGMT_SE1, c::STATIC_THREAD_MGMT_SE2, c::STATIC_THREAD_MGMT_SE3] {
        m[i] = u32::MAX;
    }
    m[c::MISC_RESERVED] = 7;
    let eop = p.eop >> 8;
    m[c::EOP_BASE_LO] = lo(eop);
    m[c::EOP_BASE_HI] = hi(eop);
    m[c::EOP_CONTROL] = eop_control();
    m[c::PQ_RPTR] = 0;
    m[c::PQ_WPTR_LO] = 0;
    m[c::PQ_WPTR_HI] = 0;
    m[c::MQD_BASE_LO] = lo(p.mqd) & !3;
    m[c::MQD_BASE_HI] = hi(p.mqd);
    m[c::MQD_CONTROL] = dflt::CP_MQD_CONTROL & !f::MQD_VMID_MASK;
    let pq = p.ring >> 8;
    m[c::PQ_BASE_LO] = lo(pq);
    m[c::PQ_BASE_HI] = hi(pq);
    m[c::RPTR_REPORT_LO] = lo(p.rptr) & !3;
    m[c::RPTR_REPORT_HI] = hi(p.rptr) & f::RPTR_ADDR_HI_MASK;
    m[c::DOORBELL_CONTROL] = hqd_doorbell(p.doorbell_dw);
    m[c::HQD_VMID] = 0;
    m[c::HQD_ACTIVE] = u32::from(p.active);
    m[c::PERSISTENT_STATE] = persistent_state();
}

/// mes_v11_0_mqd_init (7.1): the MES scheduler's and the KIQ's queue (active, its own doorbell).
pub fn mes_mqd(p: &Prop) -> Vec<u32> {
    let mut m = vec![0u32; MQD_DW];
    compute_common(&mut m, p);
    m[c::WPTR_POLL_LO] = lo(p.wptr) & !7;
    m[c::WPTR_POLL_HI] = hi(p.wptr) & f::RPTR_ADDR_HI_MASK;
    m[c::PQ_CONTROL] = mes_pq_control(p.ring_bytes);
    m[c::IB_CONTROL] = dflt::CP_HQD_IB_CONTROL;
    m[c::IQ_TIMER] = dflt::CP_HQD_IQ_TIMER;
    m[c::QUANTUM] = dflt::CP_HQD_QUANTUM;
    m
}

/// gfx_v11_0_compute_mqd_init (7.1) for a kernel compute ring of normal priority (pipe and queue priority 0).
pub fn compute_mqd(p: &Prop) -> Vec<u32> {
    let mut m = vec![0u32; MQD_DW];
    compute_common(&mut m, p);
    m[c::DEQUEUE_REQUEST] = 0;
    m[c::WPTR_POLL_LO] = lo(p.wptr) & !3;
    m[c::WPTR_POLL_HI] = hi(p.wptr) & f::RPTR_ADDR_HI_MASK;
    m[c::PQ_CONTROL] = compute_pq_control(p.ring_bytes);
    m[c::IB_CONTROL] = (dflt::CP_HQD_IB_CONTROL & !f::IB_MIN_AVAIL_MASK) | (3 << f::IB_MIN_AVAIL_SHIFT);
    m[c::PIPE_PRIORITY] = 0;
    m[c::QUEUE_PRIORITY] = 0;
    m
}

/// CP_GFX_HQD_CNTL (gfx_mqd_init): RB_BUFSZ = order_base_2(bytes / 4) - 1, RB_BLKSZ = RB_BUFSZ - 2; a kernel
/// queue (RB_NON_PRIV clear).
pub fn gfx_hqd_cntl(ring_bytes: u64) -> u32 {
    let b = order_base_2(ring_bytes / 4).saturating_sub(1);
    let mut t = (dflt::CP_GFX_HQD_CNTL & !f::GFX_RB_BUFSZ_MASK) | (b & f::GFX_RB_BUFSZ_MASK);
    t = (t & !f::GFX_RB_BLKSZ_MASK) | ((b.saturating_sub(2) << f::GFX_RB_BLKSZ_SHIFT) & f::GFX_RB_BLKSZ_MASK);
    t
}

/// gfx_v11_0_gfx_mqd_init (7.1) for GFX ring 0 (a kernel queue of normal priority, doorbell on, active; no
/// shadow, GDS backup or CSA: those are user-queue fields).
pub fn gfx_mqd(p: &Prop) -> Vec<u32> {
    let mut m = vec![0u32; MQD_DW];
    m[g::HQD_WPTR] = 0;
    m[g::HQD_WPTR_HI] = 0;
    m[g::MQD_BASE] = lo(p.mqd) & !3;
    m[g::MQD_BASE_HI] = hi(p.mqd);
    m[g::MQD_CONTROL] = (dflt::CP_GFX_MQD_CONTROL & !(f::GFX_MQD_VMID_MASK | f::GFX_MQD_CACHE_POLICY_MASK)) | f::GFX_MQD_PRIV_STATE;
    m[g::HQD_VMID] = 0;
    m[g::QUEUE_PRIORITY] = 0;
    m[g::QUANTUM] = dflt::CP_GFX_HQD_QUANTUM | f::GFX_QUANTUM_EN;
    let base = p.ring >> 8;
    m[g::HQD_BASE] = lo(base);
    m[g::HQD_BASE_HI] = hi(base);
    m[g::RPTR_ADDR] = lo(p.rptr) & !3;
    m[g::RPTR_ADDR_HI] = hi(p.rptr) & f::RPTR_ADDR_HI_MASK;
    m[g::WPTR_POLL_LO] = lo(p.wptr) & !3;
    m[g::WPTR_POLL_HI] = hi(p.wptr) & f::RPTR_ADDR_HI_MASK;
    m[g::HQD_CNTL] = gfx_hqd_cntl(p.ring_bytes);
    m[g::DOORBELL_CONTROL] = match p.doorbell_dw {
        Some(i) => ((i << 2) & f::DB_OFFSET_MASK) | f::DB_EN,
        None => 0,
    };
    m[g::HQD_RPTR] = 0;
    m[g::HQD_ACTIVE] = 1;
    m
}

/// An MQD as the bytes the firmware reads.
pub fn bytes(m: &[u32]) -> Vec<u8> {
    m.iter().flat_map(|d| d.to_le_bytes()).collect()
}
