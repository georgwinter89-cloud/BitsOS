//! Wave 44, item ME: MES 12's API frames as Linux builds them (A:../include/mes_v12_api_def.h, the only format
//! source; `#pragma pack(push, 8)` at :27, so a 64-bit field sits on an even dword, unlike MES 11's pack(4) frames of
//! the 780M). Every frame is `API_FRAME_SIZE_IN_DWORDS` (64) dwords, zeroed (`memset`) and filled with what
//! mes_v12_0.c sets. The dword offsets below are the C compiler's for that header, held against it in
//! `tests/mes12_offsets.rs`. The driver builds frames with these functions and the model parses them with the
//! same offsets.

use alloc::vec::Vec;

pub const FRAME_DW: usize = 64;
pub const FRAME_BYTES: u32 = FRAME_DW as u32 * 4;
/// `MES_API_TYPE_SCHEDULER`.
pub const TYPE_SCHEDULER: u32 = 1;
/// `MES_API_VERSION` of the header (:29). Linux never compares it with what the firmware reports.
pub const HEADER_API_VERSION: u32 = 0x14;
/// `MES_SCH_API_MAX`: Linux refuses to submit an opcode from here on (A:mes_v12_0.c:173-174).
pub const OPCODE_MAX: u32 = 0xFF;

/// `enum MES_SCH_API_OPCODE` (:50-74).
pub mod op {
    pub const SET_HW_RSRC: u32 = 0;
    pub const SET_SCHEDULING_CONFIG: u32 = 1;
    pub const ADD_QUEUE: u32 = 2;
    pub const REMOVE_QUEUE: u32 = 3;
    pub const PERFORM_YIELD: u32 = 4;
    pub const SET_GANG_PRIORITY_LEVEL: u32 = 5;
    pub const SUSPEND: u32 = 6;
    pub const RESUME: u32 = 7;
    pub const RESET: u32 = 8;
    pub const SET_LOG_BUFFER: u32 = 9;
    pub const CHANGE_GANG_PRORITY: u32 = 10;
    pub const QUERY_SCHEDULER_STATUS: u32 = 11;
    pub const SET_DEBUG_VMID: u32 = 13;
    pub const MISC: u32 = 14;
    pub const UPDATE_ROOT_PAGE_TABLE: u32 = 15;
    pub const AMD_LOG: u32 = 16;
    pub const SET_SE_MODE: u32 = 17;
    pub const SET_GANG_SUBMIT: u32 = 18;
    pub const SET_HW_RSRC_1: u32 = 19;
    pub const INV_TLBS: u32 = 20;
}

/// Linux's names of the opcodes (A:mes_v12_0.c:96-118), for the stage's lines.
pub fn op_name(opcode: u32) -> &'static str {
    const NAMES: [&str; 21] = [
        "SET_HW_RSRC", "SET_SCHEDULING_CONFIG", "ADD_QUEUE", "REMOVE_QUEUE", "PERFORM_YIELD", "SET_GANG_PRIORITY_LEVEL", "SUSPEND", "RESUME", "RESET", "SET_LOG_BUFFER", "CHANGE_GANG_PRORITY",
        "QUERY_SCHEDULER_STATUS", "unused", "SET_DEBUG_VMID", "MISC", "UPDATE_ROOT_PAGE_TABLE", "AMD_LOG", "SET_SE_MODE", "SET_GANG_SUBMIT", "SET_HW_RSRC_1", "INVALIDATE_TLBS",
    ];
    NAMES.get(opcode as usize).copied().unwrap_or("?")
}

/// `enum MESAPI_MISC_OPCODE` (:744-758).
pub mod misc {
    pub const WRITE_REG: u32 = 0;
    pub const INV_GART: u32 = 1;
    pub const QUERY_STATUS: u32 = 2;
    pub const READ_REG: u32 = 3;
    pub const WAIT_REG_MEM: u32 = 4;
    pub const SET_SHADER_DEBUGGER: u32 = 5;
    pub const CHANGE_CONFIG: u32 = 9;
    /// `enum WRM_OPERATION`: WAIT_REG_MEM, WR_WAIT_WR_REG.
    pub const WRM_WAIT_REG_MEM: u32 = 0;
    pub const WRM_WR_WAIT_WR_REG: u32 = 1;
    /// `MESAPI_MISC__CHANGE_CONFIG_OPTION_LIMIT_SINGLE_PROCESS`.
    pub const CHANGE_CONFIG_LIMIT_SINGLE_PROCESS: u32 = 0;
}

/// Linux's names of the misc opcodes (A:mes_v12_0.c:120-129).
pub fn misc_name(opcode: u32) -> &'static str {
    const NAMES: [&str; 8] = ["WRITE_REG", "INV_GART", "QUERY_STATUS", "READ_REG", "WAIT_REG_MEM", "SET_SHADER_DEBUGGER", "NOTIFY_WORK_ON_UNMAPPED_QUEUE", "NOTIFY_TO_UNMAP_PROCESSES"];
    match opcode {
        misc::CHANGE_CONFIG => "CHANGE_CONFIG",
        _ => NAMES.get(opcode as usize).copied().unwrap_or("?"),
    }
}

/// `enum MES_QUEUE_TYPE` (:81-88).
pub mod qtype {
    pub const GFX: u32 = 0;
    pub const COMPUTE: u32 = 1;
    pub const SDMA: u32 = 2;
    pub const SCHQ: u32 = 3;
}

/// Dword offsets of the fields Linux sets (pack 8; the C compiler's, `tests/mes12_offsets.rs`).
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
    pub const HW1_STATUS: usize = 2;
    pub const HW1_FLAGS: usize = 8;
    pub const HW1_KIQ_UNMAP_TIMEOUT: usize = 13;
    pub const HW1_CLEANER_FENCE: usize = 16;
    pub const ADD_PROCESS_ID: usize = 1;
    pub const ADD_GANG_CTX: usize = 14;
    pub const ADD_DOORBELL: usize = 18;
    pub const ADD_MQD: usize = 20;
    pub const ADD_WPTR: usize = 22;
    pub const ADD_QUEUE_TYPE: usize = 28;
    pub const ADD_VM_CONTEXT_CNTL: usize = 36;
    pub const ADD_FLAGS: usize = 37;
    pub const ADD_STATUS: usize = 38;
    pub const ADD_PIPE: usize = 50;
    pub const ADD_QUEUE: usize = 51;
    pub const REMOVE_DOORBELL: usize = 1;
    pub const REMOVE_GANG_CTX: usize = 2;
    pub const REMOVE_FLAGS: usize = 4;
    pub const REMOVE_STATUS: usize = 6;
    pub const REMOVE_PIPE: usize = 10;
    pub const REMOVE_QUEUE: usize = 11;
    pub const REMOVE_TF_ADDR: usize = 12;
    pub const REMOVE_TF_DATA: usize = 14;
    pub const REMOVE_QUEUE_TYPE: usize = 15;
    pub const QUERY_SUBOPCODE: usize = 1;
    pub const QUERY_STATUS: usize = 2;
    pub const MISC_OPCODE: usize = 1;
    pub const MISC_STATUS: usize = 2;
    /// The union of the misc ops starts here: WRITE_REG reg_offset, READ_REG reg_offset, WAIT_REG_MEM op,
    /// CHANGE_CONFIG opcode.
    pub const MISC_DATA: usize = 6;
    pub const MISC_READ_BUFFER: usize = 8;
    pub const MISC_WRM_REFERENCE: usize = 7;
    pub const MISC_WRM_MASK: usize = 8;
    pub const MISC_WRM_REG1: usize = 9;
    pub const MISC_WRM_REG2: usize = 10;
    pub const MISC_WRITE_VALUE: usize = 7;
    pub const MISC_CHANGE_CONFIG_OPTION: usize = 7;
}

/// SET_HW_RESOURCES' flag bits (the union after `api_status`, :282-312).
pub mod hwflag {
    pub const DISABLE_RESET: u32 = 1 << 0;
    pub const USE_DIFFERENT_VMID_COMPUTE: u32 = 1 << 1;
    pub const DISABLE_MES_LOG: u32 = 1 << 2;
    pub const ENABLE_LEVEL_PROCESS_QUANTUM_CHECK: u32 = 1 << 6;
    pub const ENABLE_MES_EVENT_INT_LOGGING: u32 = 1 << 9;
    pub const ENABLE_REG_ACTIVE_POLL: u32 = 1 << 10;
    pub const LIMIT_SINGLE_PROCESS: u32 = 1 << 18;
    pub const UNMAPPED_DOORBELL_HANDLING_SHIFT: u32 = 19;
    /// What `mes_v12_0_set_hw_resources` sets on this card (A:mes_v12_0.c:991-1015): no event log
    /// (`amdgpu_mes_log_enable` 0), no isolation (`enforce_isolation` off), unmapped doorbell handling 2
    /// ("basic+").
    pub const LINUX: u32 = DISABLE_RESET | DISABLE_MES_LOG | USE_DIFFERENT_VMID_COMPUTE | ENABLE_REG_ACTIVE_POLL | ENABLE_LEVEL_PROCESS_QUANTUM_CHECK | (2 << UNMAPPED_DOORBELL_HANDLING_SHIFT);
}

/// ADD_QUEUE's `map_legacy_kq` (bit 13 of the flags dword); REMOVE_QUEUE's `preempt_legacy_gfx_queue` (bit 2),
/// `unmap_legacy_queue` (bit 3), `remove_queue_after_reset` (bit 4).
pub const ADD_MAP_LEGACY_KQ: u32 = 1 << 13;
pub const REMOVE_PREEMPT_LEGACY_GFX: u32 = 1 << 2;
pub const REMOVE_UNMAP_LEGACY: u32 = 1 << 3;
pub const REMOVE_AFTER_RESET: u32 = 1 << 4;
/// SET_HW_RESOURCES_1's `mes_kiq_unmap_timeout` (A:mes_v12_0.c:928: 0xa, in 100 ms).
pub const KIQ_UNMAP_TIMEOUT: u32 = 0xA;

/// The header dword: type, opcode, size in dwords (header included).
pub const fn header(opcode: u32) -> u32 {
    TYPE_SCHEDULER | (opcode << 4) | ((FRAME_DW as u32) << 12)
}

/// (type, opcode, dwsize) of a header.
pub const fn header_fields(h: u32) -> (u32, u32, u32) {
    (h & 0xF, (h >> 4) & 0xFF, (h >> 12) & 0xFF)
}

/// Where a frame's `MES_API_STATUS` lies (`offsetof(.., api_status)`, the last argument of every submit).
pub const fn status_at(opcode: u32) -> Option<usize> {
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

pub fn put64(d: &mut [u32], at: usize, v: u64) {
    d[at] = v as u32;
    d[at + 1] = (v >> 32) as u32;
}

pub fn get64(d: &[u32], at: usize) -> u64 {
    u64::from(d.get(at).copied().unwrap_or(0)) | u64::from(d.get(at + 1).copied().unwrap_or(0)) << 32
}

/// A zeroed frame with its header.
pub fn frame(opcode: u32) -> Vec<u32> {
    let mut d = alloc::vec![0u32; FRAME_DW];
    d[0] = header(opcode);
    d
}

/// The completion fence of a frame: `api_completion_fence_addr` and `_value` (A:mes_v12_0.c:197-199).
pub fn set_status(d: &mut [u32], status: usize, addr: u64, value: u64) {
    put64(d, status, addr);
    put64(d, status + 2, value);
}

/// What SET_HW_RESOURCES names on one pipe. The KIQ pipe gets no masks and no doorbells (A:mes_v12_0.c:953-976).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwRes {
    /// `pipe == AMDGPU_MES_SCHED_PIPE`: the masks, the GDS size and the aggregated doorbells go out.
    pub sched: bool,
    pub sch_ctx: u64,
    pub query_fence: u64,
    /// `adev->reg_offset[GC|MMHUB|OSSSYS][0][0..5]`: the discovery table's base segments of instance 0.
    pub gc_base: [u32; 5],
    pub mmhub_base: [u32; 5],
    pub osssys_base: [u32; 5],
    pub oversubscription_timer: u32,
}

/// `mes_v12_0_set_hw_resources` (:936-1019).
pub fn set_hw_resources(r: &HwRes) -> Vec<u32> {
    use crate::mes12 as m;
    let mut d = frame(op::SET_HW_RSRC);
    if r.sched {
        (d[at::HW_VMID_MMHUB], d[at::HW_VMID_GFXHUB], d[at::HW_GDS_SIZE], d[at::HW_PAGING_VMID]) = (m::VMID_MASK_MMHUB, m::VMID_MASK_GFXHUB, 0, 0);
        d[at::HW_COMPUTE_HQD..at::HW_COMPUTE_HQD + 8].copy_from_slice(&m::COMPUTE_HQD_MASK);
        d[at::HW_GFX_HQD..at::HW_GFX_HQD + 2].copy_from_slice(&m::GFX_HQD_MASK);
        d[at::HW_SDMA_HQD..at::HW_SDMA_HQD + 2].copy_from_slice(&m::SDMA_HQD_MASK);
        d[at::HW_AGG_DOORBELLS..at::HW_AGG_DOORBELLS + 5].copy_from_slice(&m::AGGREGATED_DOORBELLS);
    }
    put64(&mut d, at::HW_SCH_CTX, r.sch_ctx);
    put64(&mut d, at::HW_QUERY_FENCE, r.query_fence);
    d[at::HW_GC_BASE..at::HW_GC_BASE + 5].copy_from_slice(&r.gc_base);
    d[at::HW_MMHUB_BASE..at::HW_MMHUB_BASE + 5].copy_from_slice(&r.mmhub_base);
    d[at::HW_OSSSYS_BASE..at::HW_OSSSYS_BASE + 5].copy_from_slice(&r.osssys_base);
    d[at::HW_FLAGS] = hwflag::LINUX;
    d[at::HW_OVERSUB_TIMER] = r.oversubscription_timer;
    d
}

/// `mes_v12_0_set_hw_resources_1` (:918-934): the KIQ's unmap timeout and where the cleaner shader's fence goes.
pub fn set_hw_resources_1(cleaner_fence: u64) -> Vec<u32> {
    let mut d = frame(op::SET_HW_RSRC_1);
    d[at::HW1_KIQ_UNMAP_TIMEOUT] = KIQ_UNMAP_TIMEOUT;
    put64(&mut d, at::HW1_CLEANER_FENCE, cleaner_fence);
    d
}

/// `mes_v12_0_query_sched_status` (:827-839), also the frame behind every command (:205-213).
pub fn query_status() -> Vec<u32> {
    frame(op::QUERY_SCHEDULER_STATUS)
}

/// A kernel queue as `amdgpu_mes_map_legacy_queue` and `amdgpu_mes_unmap_legacy_queue` hand it to MES
/// (A:amdgpu_mes.c:372-430).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Legacy {
    /// `convert_to_mes_queue_type` (A:mes_v12_0.c:274-287).
    pub queue_type: u32,
    pub pipe: u32,
    pub queue: u32,
    pub doorbell: u32,
    pub mqd: u64,
    pub wptr: u64,
}

/// `mes_v12_0_map_legacy_queue` (:694-728): ADD_QUEUE with `map_legacy_kq`; every process and gang field 0.
pub fn add_legacy(q: &Legacy) -> Vec<u32> {
    let mut d = frame(op::ADD_QUEUE);
    (d[at::ADD_PIPE], d[at::ADD_QUEUE], d[at::ADD_DOORBELL]) = (q.pipe, q.queue, q.doorbell);
    put64(&mut d, at::ADD_MQD, q.mqd);
    put64(&mut d, at::ADD_WPTR, q.wptr);
    d[at::ADD_QUEUE_TYPE] = q.queue_type;
    d[at::ADD_FLAGS] = ADD_MAP_LEGACY_KQ;
    d
}

/// `enum amdgpu_unmap_queues_action` (A:amdgpu_ring.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmap {
    PreemptQueues,
    ResetQueues,
    DisableProcessQueues,
    PreemptQueuesNoUnmap,
}

/// `mes_v12_0_unmap_legacy_queue` (:730-783): REMOVE_QUEUE of a kernel queue. `version`: the scheduler's
/// firmware version (`remove_queue_after_reset` only from 0x5a on); `trail`: the trailing fence's address and
/// value for a preemption without unmap.
pub fn remove_legacy(q: &Legacy, action: Unmap, version: u32, trail: (u64, u64)) -> Vec<u32> {
    let mut d = frame(op::REMOVE_QUEUE);
    d[at::REMOVE_DOORBELL] = q.doorbell;
    put64(&mut d, at::REMOVE_GANG_CTX, 0);
    (d[at::REMOVE_PIPE], d[at::REMOVE_QUEUE]) = (q.pipe, q.queue);
    if action == Unmap::PreemptQueuesNoUnmap {
        d[at::REMOVE_FLAGS] = REMOVE_PREEMPT_LEGACY_GFX;
        put64(&mut d, at::REMOVE_TF_ADDR, trail.0);
        d[at::REMOVE_TF_DATA] = trail.1 as u32;
    } else {
        d[at::REMOVE_FLAGS] = REMOVE_UNMAP_LEGACY;
        d[at::REMOVE_QUEUE_TYPE] = q.queue_type;
        if action == Unmap::ResetQueues && version >= crate::mes12::REMOVE_AFTER_RESET_FROM {
            d[at::REMOVE_FLAGS] |= REMOVE_AFTER_RESET;
        }
    }
    d
}

/// `mes_v12_0_misc_op` (:841-916), the ops Linux uses with MES running: a register write and wait, a read, a
/// write, and CHANGE_CONFIG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Misc {
    /// MES_MISC_OP_WRM_REG_WR_WAIT: `reg0` and `reg1` as dword offsets.
    WriteWait { reg0: u32, reg1: u32, reference: u32, mask: u32 },
    ReadReg { reg: u32, buffer: u64 },
    WriteReg { reg: u32, value: u32 },
    ChangeConfig { limit_single_process: bool },
}

pub fn misc_frame(m: Misc) -> Vec<u32> {
    let mut d = frame(op::MISC);
    match m {
        Misc::WriteWait { reg0, reg1, reference, mask } => {
            d[at::MISC_OPCODE] = misc::WAIT_REG_MEM;
            (d[at::MISC_DATA], d[at::MISC_WRM_REFERENCE], d[at::MISC_WRM_MASK]) = (misc::WRM_WR_WAIT_WR_REG, reference, mask);
            (d[at::MISC_WRM_REG1], d[at::MISC_WRM_REG2]) = (reg0, reg1);
        }
        Misc::ReadReg { reg, buffer } => {
            (d[at::MISC_OPCODE], d[at::MISC_DATA]) = (misc::READ_REG, reg);
            put64(&mut d, at::MISC_READ_BUFFER, buffer);
        }
        Misc::WriteReg { reg, value } => {
            (d[at::MISC_OPCODE], d[at::MISC_DATA], d[at::MISC_WRITE_VALUE]) = (misc::WRITE_REG, reg, value);
        }
        Misc::ChangeConfig { limit_single_process } => {
            (d[at::MISC_OPCODE], d[at::MISC_DATA]) = (misc::CHANGE_CONFIG, misc::CHANGE_CONFIG_LIMIT_SINGLE_PROCESS);
            d[at::MISC_CHANGE_CONFIG_OPTION] = u32::from(limit_single_process);
        }
    }
    d
}

/// A failed command's status as the header documents it (:118-151): the lower 32 bits 0, the upper the error
/// code with bit 31 set.
pub const fn error_status(api_err: u32, opcode: u32, misc_op: u32, category: u32) -> u64 {
    ((api_err | opcode << 8 | misc_op << 16 | category << 24 | 1 << 31) as u64) << 32
}

/// `MES_ERROR_API`, `MES_ERROR_SCHEDULING`, `MES_ERROR_UNKNOWN` (:131-135).
pub const ERROR_API: u32 = 1;
pub const ERROR_SCHEDULING: u32 = 2;
pub const ERROR_UNKNOWN: u32 = 3;

/// The upper dword of a failed status, in words (opcode, misc opcode, category, the API's code).
pub fn describe_error(upper: u32) -> alloc::string::String {
    let cat = match (upper >> 24) & 0x7F {
        ERROR_API => "API",
        ERROR_SCHEDULING => "scheduling",
        ERROR_UNKNOWN => "unknown",
        _ => "no category",
    };
    alloc::format!("{cat} error {:#x} of {} (misc {:#x}){}", upper & 0xFF, op_name((upper >> 8) & 0xFF), (upper >> 16) & 0xFF, if upper >> 31 == 1 { "" } else { ", without the error bit" })
}
