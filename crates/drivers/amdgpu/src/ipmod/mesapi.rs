//! GX-7k: the MES scheduler's API frames (Linux 7.1 include/mes_v11_api_def.h, MIT, AMD: `#pragma pack(4)`, so a
//! 64-bit field sits at any dword) and the KIQ's PM4 packets (nvd.h MAP_QUEUES/SET_RESOURCES, gfx_v11_0.c
//! gfx11_kiq_map_queues/_set_resources). Every frame is API_FRAME_SIZE_IN_DWORDS (64) dwords; its MES_API_STATUS
//! (fence address and value) sits at a per-opcode dword (`Frame::status_at`). Pure: the stage writes them, the
//! fake parses them with the same offsets.

use super::mes11::HwMasks;
use alloc::vec::Vec;
use amdgpu_fmt::pm4;

pub const FRAME_DW: usize = 64;
/// MES_API_TYPE_SCHEDULER.
pub const TYPE_SCHEDULER: u32 = 1;

/// MES_SCH_API_OPCODE.
pub mod op {
    pub const SET_HW_RSRC: u32 = 0;
    pub const ADD_QUEUE: u32 = 2;
    pub const REMOVE_QUEUE: u32 = 3;
    pub const QUERY_SCHEDULER_STATUS: u32 = 11;
    pub const MISC: u32 = 14;
    pub const SET_HW_RSRC_1: u32 = 19;
}

/// MESAPI_MISC_OPCODE.
pub mod misc {
    pub const WRITE_REG: u32 = 0;
    pub const READ_REG: u32 = 3;
}

/// MES_QUEUE_TYPE.
pub mod qtype {
    pub const GFX: u32 = 0;
    pub const COMPUTE: u32 = 1;
}

/// Dword offsets inside the frames (pack(4)).
pub mod at {
    pub const HW_VMID_MMHUB: usize = 1;
    pub const HW_VMID_GFXHUB: usize = 2;
    pub const HW_GDS_SIZE: usize = 3;
    pub const HW_PAGING_VMID: usize = 4;
    pub const HW_COMPUTE_HQD: usize = 5;
    pub const HW_GFX_HQD: usize = 13;
    pub const HW_SDMA_HQD: usize = 15;
    pub const HW_AGG_DOORBELLS: usize = 17;
    pub const HW_SCH_CTX: usize = 22;
    pub const HW_QUERY_FENCE: usize = 24;
    pub const HW_GC_BASE: usize = 26;
    pub const HW_MMHUB_BASE: usize = 34;
    pub const HW_OSSSYS_BASE: usize = 42;
    pub const HW_STATUS: usize = 50;
    pub const HW_FLAGS: usize = 54;
    pub const HW_OVERSUB_TIMER: usize = 55;
    pub const HW1_STATUS: usize = 1;
    pub const HW1_FLAGS: usize = 7;
    pub const HW1_CLEANER_FENCE: usize = 13;
    /// GX-7m: ADD_QUEUE's process and gang fields (mes_v11_api_def.h union MESAPI__ADD_QUEUE, pack 4).
    pub const ADD_PROCESS_ID: usize = 1;
    pub const ADD_PT_BASE: usize = 2;
    pub const ADD_VA_START: usize = 4;
    pub const ADD_VA_END: usize = 6;
    pub const ADD_PROCESS_QUANTUM: usize = 8;
    pub const ADD_PROCESS_CTX: usize = 10;
    pub const ADD_GANG_QUANTUM: usize = 12;
    pub const ADD_GANG_CTX: usize = 14;
    pub const ADD_INPROCESS_GANG_PRIORITY: usize = 16;
    pub const ADD_GANG_GLOBAL_PRIORITY: usize = 17;
    pub const ADD_TRAP_HANDLER: usize = 33;
    pub const ADD_VM_CONTEXT_CNTL: usize = 35;
    pub const ADD_DOORBELL: usize = 18;
    pub const ADD_MQD: usize = 19;
    pub const ADD_WPTR: usize = 21;
    pub const ADD_QUEUE_TYPE: usize = 27;
    pub const ADD_FLAGS: usize = 36;
    pub const ADD_STATUS: usize = 37;
    pub const ADD_PIPE: usize = 48;
    pub const ADD_QUEUE: usize = 49;
    pub const REMOVE_DOORBELL: usize = 1;
    /// GX-7m: REMOVE_QUEUE's gang context (a process queue's).
    pub const REMOVE_GANG_CTX: usize = 2;
    pub const REMOVE_FLAGS: usize = 4;
    pub const REMOVE_STATUS: usize = 5;
    pub const REMOVE_PIPE: usize = 9;
    pub const REMOVE_QUEUE: usize = 10;
    pub const REMOVE_QUEUE_TYPE: usize = 14;
    pub const QUERY_STATUS: usize = 2;
    pub const MISC_OPCODE: usize = 1;
    pub const MISC_STATUS: usize = 2;
    pub const MISC_DATA: usize = 6;
}

/// SET_HW_RESOURCES flag bits (the union after api_status).
pub mod hwflag {
    pub const DISABLE_RESET: u32 = 1 << 0;
    pub const USE_DIFFERENT_VMID_COMPUTE: u32 = 1 << 1;
    pub const DISABLE_MES_LOG: u32 = 1 << 2;
    pub const ENABLE_LEVEL_PROCESS_QUANTUM_CHECK: u32 = 1 << 6;
    pub const ENABLE_REG_ACTIVE_POLL: u32 = 1 << 10;
    /// What mes_v11_0_set_hw_resources (7.1 :674) sets without the event log and isolation options.
    pub const LINUX: u32 = DISABLE_RESET | DISABLE_MES_LOG | USE_DIFFERENT_VMID_COMPUTE | ENABLE_REG_ACTIVE_POLL | ENABLE_LEVEL_PROCESS_QUANTUM_CHECK;
}
/// ADD_QUEUE's map_legacy_kq (bit 13 of the flags dword); REMOVE_QUEUE's unmap_legacy_queue (bit 3).
pub const ADD_MAP_LEGACY_KQ: u32 = 1 << 13;
pub const REMOVE_UNMAP_LEGACY_QUEUE: u32 = 1 << 3;
/// SET_HW_RESOURCES_1's enable_mes_info_ctx.
pub const HW1_ENABLE_MES_INFO_CTX: u32 = 1;
/// SET_HW_RESOURCES's oversubscription_timer (Linux: 50).
pub const OVERSUBSCRIPTION_TIMER: u32 = 50;

/// One API frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame(pub [u32; FRAME_DW]);

/// The header dword: type, opcode, size in dwords (including the header).
pub const fn header(opcode: u32) -> u32 {
    TYPE_SCHEDULER | (opcode << 4) | ((FRAME_DW as u32) << 12)
}

/// (type, opcode, dwsize) of a header.
pub fn header_fields(h: u32) -> (u32, u32, u32) {
    (h & 0xf, (h >> 4) & 0xff, (h >> 12) & 0xff)
}

fn put64(d: &mut [u32; FRAME_DW], at: usize, v: u64) {
    d[at] = v as u32;
    d[at + 1] = (v >> 32) as u32;
}

/// A 64-bit field of a frame (the fake's parse).
pub fn get64(d: &[u32], at: usize) -> u64 {
    u64::from(d.get(at).copied().unwrap_or(0)) | u64::from(d.get(at + 1).copied().unwrap_or(0)) << 32
}

/// What SET_HW_RESOURCES names (MC addresses; the register bases are GC's, MMHUB's and OSSSYS's segment bases
/// from the discovery table, `adev->reg_offset[...][0][0..5]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwRes {
    pub masks: HwMasks,
    /// The aggregated doorbells per priority level (mes->aggregated_doorbells).
    pub agg_doorbells: [u32; 5],
    pub sch_ctx: u64,
    pub query_fence: u64,
    pub gc_base: [u32; 5],
    pub mmhub_base: [u32; 5],
    pub osssys_base: [u32; 5],
}

/// GX-7m: one process queue for ADD_QUEUE (amdgpu_mes_add_hw_queue's mes_add_queue_input, 6.6 amdgpu_mes.c:622-644).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessQueue {
    pub pasid: u32,
    /// The root table's address (vram_base_offset + pd_gpu_addr - vram_start: physical, no PDE bits).
    pub page_table_base: u64,
    pub va_start: u64,
    pub va_end: u64,
    pub process_quantum: u64,
    pub process_ctx: u64,
    pub gang_quantum: u64,
    pub gang_ctx: u64,
    pub inprocess_gang_priority: u32,
    pub gang_global_priority: u32,
    pub doorbell_dw: u32,
    pub mqd: u64,
    pub wptr: u64,
    pub queue_type: u32,
    pub vm_context_cntl: u32,
}

/// AMDGPU_MES_PRIORITY_LEVEL_NORMAL (amdgpu_mes.h:46; the self test's gang, amdgpu_mes.c:1233-1237).
pub const PRIORITY_NORMAL: u32 = 1;

impl Frame {
    fn new(opcode: u32) -> Frame {
        let mut d = [0u32; FRAME_DW];
        d[0] = header(opcode);
        Frame(d)
    }

    pub fn opcode(&self) -> u32 {
        header_fields(self.0[0]).1
    }

    /// Where MES_API_STATUS sits (by opcode; `None` for one this driver never sends).
    pub fn status_at(&self) -> Option<usize> {
        status_at(self.opcode())
    }

    /// The completion fence (submit_pkt_and_poll_completion: `api_status` gets the address and value).
    pub fn set_status(&mut self, addr: u64, value: u64) {
        if let Some(at) = self.status_at() {
            put64(&mut self.0, at, addr);
            put64(&mut self.0, at + 2, value);
        }
    }

    /// mes_v11_0_set_hw_resources (7.1 :674): masks, GDS size 0 (no GDS on GC 11's kernel path), paging VMID 0,
    /// the scheduler context and query-status fence, the register bases, Linux's flags, oversubscription 50.
    pub fn set_hw_resources(r: &HwRes) -> Frame {
        let mut f = Frame::new(op::SET_HW_RSRC);
        let d = &mut f.0;
        d[at::HW_VMID_MMHUB] = r.masks.vmid_mmhub;
        d[at::HW_VMID_GFXHUB] = r.masks.vmid_gfxhub;
        d[at::HW_GDS_SIZE] = 0;
        d[at::HW_PAGING_VMID] = 0;
        d[at::HW_COMPUTE_HQD..at::HW_COMPUTE_HQD + 8].copy_from_slice(&r.masks.compute_hqd);
        d[at::HW_GFX_HQD..at::HW_GFX_HQD + 2].copy_from_slice(&r.masks.gfx_hqd);
        d[at::HW_SDMA_HQD..at::HW_SDMA_HQD + 2].copy_from_slice(&r.masks.sdma_hqd);
        d[at::HW_AGG_DOORBELLS..at::HW_AGG_DOORBELLS + 5].copy_from_slice(&r.agg_doorbells);
        put64(d, at::HW_SCH_CTX, r.sch_ctx);
        put64(d, at::HW_QUERY_FENCE, r.query_fence);
        d[at::HW_GC_BASE..at::HW_GC_BASE + 5].copy_from_slice(&r.gc_base);
        d[at::HW_MMHUB_BASE..at::HW_MMHUB_BASE + 5].copy_from_slice(&r.mmhub_base);
        d[at::HW_OSSSYS_BASE..at::HW_OSSSYS_BASE + 5].copy_from_slice(&r.osssys_base);
        d[at::HW_FLAGS] = hwflag::LINUX;
        d[at::HW_OVERSUB_TIMER] = OVERSUBSCRIPTION_TIMER;
        f
    }

    /// mes_v11_0_set_hw_resources_1 (7.1 :738): enable_mes_info_ctx, the cleaner shader's fence page (no SR-IOV:
    /// no info context address).
    pub fn set_hw_resources_1(cleaner_fence: u64) -> Frame {
        let mut f = Frame::new(op::SET_HW_RSRC_1);
        f.0[at::HW1_FLAGS] = HW1_ENABLE_MES_INFO_CTX;
        put64(&mut f.0, at::HW1_CLEANER_FENCE, cleaner_fence);
        f
    }

    /// QUERY_SCHEDULER_STATUS (query_sched_status, and the fence frame after every submission).
    pub fn query_status() -> Frame {
        Frame::new(op::QUERY_SCHEDULER_STATUS)
    }

    /// MISC READ_REG: MES copies register `reg_dw` (dword index) to `buffer` (misc_op :600).
    pub fn misc_read_reg(reg_dw: u32, buffer: u64) -> Frame {
        let mut f = Frame::new(op::MISC);
        f.0[at::MISC_OPCODE] = misc::READ_REG;
        f.0[at::MISC_DATA] = reg_dw;
        put64(&mut f.0, at::MISC_DATA + 1, buffer);
        f
    }

    /// MISC WRITE_REG: MES writes `value` to register `reg_dw`.
    pub fn misc_write_reg(reg_dw: u32, value: u32) -> Frame {
        let mut f = Frame::new(op::MISC);
        f.0[at::MISC_OPCODE] = misc::WRITE_REG;
        f.0[at::MISC_DATA] = reg_dw;
        f.0[at::MISC_DATA + 1] = value;
        f
    }

    /// map_legacy_queue (7.1 :487): ADD_QUEUE with map_legacy_kq, the queue's pipe/queue, doorbell, MQD, wptr.
    pub fn map_legacy(queue_type: u32, pipe: u32, queue: u32, doorbell_dw: u32, mqd: u64, wptr: u64) -> Frame {
        let mut f = Frame::new(op::ADD_QUEUE);
        let d = &mut f.0;
        d[at::ADD_PIPE] = pipe;
        d[at::ADD_QUEUE] = queue;
        d[at::ADD_DOORBELL] = doorbell_dw;
        put64(d, at::ADD_MQD, mqd);
        put64(d, at::ADD_WPTR, wptr);
        d[at::ADD_QUEUE_TYPE] = queue_type;
        d[at::ADD_FLAGS] = ADD_MAP_LEGACY_KQ;
        f
    }

    /// GX-7m: mes_v11_0_add_hw_queue (6.6 mes_v11_0.c:162-215) for a process queue: the process (PASID, its page
    /// table, its VA range, its context page), its gang (context page, priorities), the queue (doorbell, MQD, the
    /// write pointer's MC address: scheduler API 2 and later take wptr_mc_addr), GFXHUB's VM_CONTEXT_CNTL. No
    /// map_legacy_kq: MES picks the VMID (from SET_HW_RESOURCES' mask) and the HQD.
    pub fn add_process_queue(p: &ProcessQueue) -> Frame {
        let mut f = Frame::new(op::ADD_QUEUE);
        let d = &mut f.0;
        d[at::ADD_PROCESS_ID] = p.pasid;
        put64(d, at::ADD_PT_BASE, p.page_table_base);
        put64(d, at::ADD_VA_START, p.va_start);
        put64(d, at::ADD_VA_END, p.va_end);
        put64(d, at::ADD_PROCESS_QUANTUM, p.process_quantum);
        put64(d, at::ADD_PROCESS_CTX, p.process_ctx);
        put64(d, at::ADD_GANG_QUANTUM, p.gang_quantum);
        put64(d, at::ADD_GANG_CTX, p.gang_ctx);
        d[at::ADD_INPROCESS_GANG_PRIORITY] = p.inprocess_gang_priority;
        d[at::ADD_GANG_GLOBAL_PRIORITY] = p.gang_global_priority;
        d[at::ADD_DOORBELL] = p.doorbell_dw;
        put64(d, at::ADD_MQD, p.mqd);
        put64(d, at::ADD_WPTR, p.wptr);
        d[at::ADD_QUEUE_TYPE] = p.queue_type;
        put64(d, at::ADD_TRAP_HANDLER, 0);
        d[at::ADD_VM_CONTEXT_CNTL] = p.vm_context_cntl;
        d[at::ADD_FLAGS] = 0;
        f
    }

    /// GX-7m: mes_v11_0_remove_hw_queue (6.6 :219-237) of a process queue: its doorbell and gang context.
    pub fn remove_process_queue(doorbell_dw: u32, gang_ctx: u64) -> Frame {
        let mut f = Frame::new(op::REMOVE_QUEUE);
        f.0[at::REMOVE_DOORBELL] = doorbell_dw;
        put64(&mut f.0, at::REMOVE_GANG_CTX, gang_ctx);
        f
    }

    /// unmap_legacy_queue (7.1 :512, action UNMAP): REMOVE_QUEUE with unmap_legacy_queue.
    pub fn unmap_legacy(queue_type: u32, pipe: u32, queue: u32, doorbell_dw: u32) -> Frame {
        let mut f = Frame::new(op::REMOVE_QUEUE);
        let d = &mut f.0;
        d[at::REMOVE_DOORBELL] = doorbell_dw;
        d[at::REMOVE_PIPE] = pipe;
        d[at::REMOVE_QUEUE] = queue;
        d[at::REMOVE_FLAGS] = REMOVE_UNMAP_LEGACY_QUEUE;
        d[at::REMOVE_QUEUE_TYPE] = queue_type;
        f
    }

    pub fn name(&self) -> &'static str {
        opcode_name(self.opcode())
    }
}

pub fn status_at(opcode: u32) -> Option<usize> {
    match opcode {
        op::SET_HW_RSRC => Some(at::HW_STATUS),
        op::SET_HW_RSRC_1 => Some(at::HW1_STATUS),
        op::ADD_QUEUE => Some(at::ADD_STATUS),
        op::REMOVE_QUEUE => Some(at::REMOVE_STATUS),
        op::QUERY_SCHEDULER_STATUS => Some(at::QUERY_STATUS),
        op::MISC => Some(at::MISC_STATUS),
        _ => None,
    }
}

pub fn opcode_name(opcode: u32) -> &'static str {
    match opcode {
        op::SET_HW_RSRC => "SET_HW_RESOURCES",
        op::SET_HW_RSRC_1 => "SET_HW_RESOURCES_1",
        op::ADD_QUEUE => "ADD_QUEUE",
        op::REMOVE_QUEUE => "REMOVE_QUEUE",
        op::QUERY_SCHEDULER_STATUS => "QUERY_SCHEDULER_STATUS",
        op::MISC => "MISC",
        _ => "?",
    }
}

/// gfx11_kiq_map_queues: MAP_QUEUES (Q_SEL 0, VMID 0, the queue, pipe, ME, normal queue type, all on one pipe,
/// ENGINE_SEL, one queue), the doorbell, the MQD and the wptr address. MES: ME 2, ENGINE_SEL 5.
pub fn map_queues(me: u32, eng_sel: u32, pipe: u32, queue: u32, doorbell_dw: u32, mqd: u64, wptr: u64) -> [u32; 7] {
    [
        pm4::packet3(pm4::op::MAP_QUEUES, 5),
        ((queue & 7) << 13) | ((pipe & 3) << 16) | ((me & 3) << 18) | ((eng_sel & 7) << 26) | (1 << 29),
        (doorbell_dw & 0x3ff_ffff) << 2,
        mqd as u32,
        (mqd >> 32) as u32,
        wptr as u32,
        (wptr >> 32) as u32,
    ]
}

/// The MES ring's MAP_QUEUES fields (gfx11_kiq_map_queues: AMDGPU_RING_TYPE_MES).
pub const MAP_ME_MES: u32 = 2;
pub const MAP_ENG_SEL_MES: u32 = 5;

/// gfx11_kiq_set_resources: SET_RESOURCES (VMID mask 0, unmap latency 0xa, queue type KIQ), the queue mask, the
/// cleaner shader's address >> 8 (none: 0), OAC mask 0, GDS heap 0.
pub fn set_resources(queue_mask: u64, cleaner_shader: u64) -> [u32; 8] {
    let s = cleaner_shader >> 8;
    [pm4::packet3(pm4::op::SET_RESOURCES, 6), 0xa << 16, queue_mask as u32, (queue_mask >> 32) as u32, s as u32, (s >> 32) as u32, 0, 0]
}

/// The KIQ's ring test packet (gfx_v11_0_ring_emit_wreg on a KIQ ring: WRITE_DATA to a register, ONE_ADDR).
pub fn kiq_wreg(out: &mut Vec<u32>, reg_dw: u32, value: u32) -> Result<(), amdgpu_fmt::encode::EncodeError> {
    pm4::write_data(out, pm4::dst_sel::REG, u64::from(reg_dw), pm4::WR_ONE_ADDR, &[value])
}
