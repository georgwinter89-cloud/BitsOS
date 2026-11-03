//! MES 11 (the micro engine scheduler inside GC 11): Linux `mes_v11_0` (linux-6.6 amdgpu/mes_v11_0.c; the 27.10.
//! capture ran Linux 7.1, whose mes_v11_0.c is followed where the two differ). Two pipes of ME 3: the scheduler
//! pipe 0 runs `_mes_2` (falls back to `_mes`), the KIQ pipe 1 runs `_mes1` (amdgpu_mes.c:1398-1418); both are
//! loaded by the PSP and started here. GX-7k: the registers (`M`, (segment, dword) of gc_11_0_0_offset.h in GC's
//! bases), their fields and Linux's values; the MQDs (`mesmqd`), the MES API frames and KIQ packets (`mesapi`).
//! The run is `gpucore::mes`.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub static MODULE: Module = Module {
    name: "mes11",
    role: Role::Mes,
    versions: &[v(11, 0, 1)],
    linux: "mes_v11_0",
    linux_also: &[v(11, 0, 0), v(11, 0, 2), v(11, 0, 3), v(11, 0, 4)],
    firmware: Firmware::Files {
        prefix: "gc",
        needs: &[Need { suffix: "_mes_2", fallback: Some("_mes") }, Need::one("_mes1")],
    },
    state: State::Present("gpucore::mes (GX-7k: both MES pipes started, the KIQ on pipe 1, the scheduler queue mapped through it, SET_HW_RESOURCES, the self test, GFX ring 0 and one compute ring mapped through MES with ring, IB and fence tests)"),
};

/// One register of the MES start, the KIQ and the HQDs (instance 0). The CP_HQD_* and CP_MQD_* registers are
/// banked: GRBM_GFX_CNTL selects which (ME, pipe, queue) they show.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum M {
    CpMesCntl,
    CpMesPrgrmCntrStart,
    CpMesPrgrmCntrStartHi,
    CpMesGp3Lo,
    RlcCpSchedulers,
    CpMqdBaseAddr,
    CpMqdBaseAddrHi,
    CpHqdActive,
    CpHqdVmid,
    CpHqdPersistentState,
    CpHqdPqBase,
    CpHqdPqBaseHi,
    CpHqdPqRptr,
    CpHqdPqRptrReportAddr,
    CpHqdPqRptrReportAddrHi,
    CpHqdPqWptrPollAddr,
    CpHqdPqWptrPollAddrHi,
    CpHqdPqDoorbellControl,
    CpHqdPqControl,
    CpHqdDequeueRequest,
    CpMqdControl,
    CpHqdEopBaseAddr,
    CpHqdEopBaseAddrHi,
    CpHqdEopControl,
    CpHqdPqWptrLo,
    CpHqdPqWptrHi,
    /// GX-7m3: a stalled compute queue's reset (mes_v11_0_reset_queue_mmio, 7.1) and what the CPU reads about the
    /// stall (report only; Linux dumps the same in gfx_v11_ip_dump, 7.1).
    SpiComputeQueueReset,
    CpHqdError,
    CpcUtcl1Error,
    CpcUtcl1Status,
    CpCpcStatus,
    CpCpcStalledStat1,
}

impl M {
    pub const ALL: [M; 32] = [
        M::CpMesCntl,
        M::CpMesPrgrmCntrStart,
        M::CpMesPrgrmCntrStartHi,
        M::CpMesGp3Lo,
        M::RlcCpSchedulers,
        M::CpMqdBaseAddr,
        M::CpMqdBaseAddrHi,
        M::CpHqdActive,
        M::CpHqdVmid,
        M::CpHqdPersistentState,
        M::CpHqdPqBase,
        M::CpHqdPqBaseHi,
        M::CpHqdPqRptr,
        M::CpHqdPqRptrReportAddr,
        M::CpHqdPqRptrReportAddrHi,
        M::CpHqdPqWptrPollAddr,
        M::CpHqdPqWptrPollAddrHi,
        M::CpHqdPqDoorbellControl,
        M::CpHqdPqControl,
        M::CpHqdDequeueRequest,
        M::CpMqdControl,
        M::CpHqdEopBaseAddr,
        M::CpHqdEopBaseAddrHi,
        M::CpHqdEopControl,
        M::CpHqdPqWptrLo,
        M::CpHqdPqWptrHi,
        M::SpiComputeQueueReset,
        M::CpHqdError,
        M::CpcUtcl1Error,
        M::CpcUtcl1Status,
        M::CpCpcStatus,
        M::CpCpcStalledStat1,
    ];

    /// (segment, dword) in GC's bases (gc_11_0_0_offset.h).
    pub fn at(self) -> (usize, u32) {
        match self {
            M::CpMesCntl => (1, 0x2807),
            M::CpMesPrgrmCntrStart => (1, 0x2800),
            M::CpMesPrgrmCntrStartHi => (1, 0x289d),
            M::CpMesGp3Lo => (1, 0x2849),
            M::RlcCpSchedulers => (1, 0x098a),
            M::CpMqdBaseAddr => (0, 0x1fa9),
            M::CpMqdBaseAddrHi => (0, 0x1faa),
            M::CpHqdActive => (0, 0x1fab),
            M::CpHqdVmid => (0, 0x1fac),
            M::CpHqdPersistentState => (0, 0x1fad),
            M::CpHqdPqBase => (0, 0x1fb1),
            M::CpHqdPqBaseHi => (0, 0x1fb2),
            M::CpHqdPqRptr => (0, 0x1fb3),
            M::CpHqdPqRptrReportAddr => (0, 0x1fb4),
            M::CpHqdPqRptrReportAddrHi => (0, 0x1fb5),
            M::CpHqdPqWptrPollAddr => (0, 0x1fb6),
            M::CpHqdPqWptrPollAddrHi => (0, 0x1fb7),
            M::CpHqdPqDoorbellControl => (0, 0x1fb8),
            M::CpHqdPqControl => (0, 0x1fba),
            M::CpHqdDequeueRequest => (0, 0x1fc1),
            M::CpMqdControl => (0, 0x1fcb),
            M::CpHqdEopBaseAddr => (0, 0x1fce),
            M::CpHqdEopBaseAddrHi => (0, 0x1fcf),
            M::CpHqdEopControl => (0, 0x1fd0),
            M::CpHqdPqWptrLo => (0, 0x1fdf),
            M::CpHqdPqWptrHi => (0, 0x1fe0),
            M::SpiComputeQueueReset => (0, 0x1f73),
            M::CpHqdError => (0, 0x1fdc),
            M::CpcUtcl1Error => (0, 0x1dff),
            M::CpcUtcl1Status => (0, 0x1f55),
            M::CpCpcStatus => (0, 0x0e24),
            M::CpCpcStalledStat1 => (0, 0x0e26),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            M::CpMesCntl => "CP_MES_CNTL",
            M::CpMesPrgrmCntrStart => "CP_MES_PRGRM_CNTR_START",
            M::CpMesPrgrmCntrStartHi => "CP_MES_PRGRM_CNTR_START_HI",
            M::CpMesGp3Lo => "CP_MES_GP3_LO",
            M::RlcCpSchedulers => "RLC_CP_SCHEDULERS",
            M::CpMqdBaseAddr => "CP_MQD_BASE_ADDR",
            M::CpMqdBaseAddrHi => "CP_MQD_BASE_ADDR_HI",
            M::CpHqdActive => "CP_HQD_ACTIVE",
            M::CpHqdVmid => "CP_HQD_VMID",
            M::CpHqdPersistentState => "CP_HQD_PERSISTENT_STATE",
            M::CpHqdPqBase => "CP_HQD_PQ_BASE",
            M::CpHqdPqBaseHi => "CP_HQD_PQ_BASE_HI",
            M::CpHqdPqRptr => "CP_HQD_PQ_RPTR",
            M::CpHqdPqRptrReportAddr => "CP_HQD_PQ_RPTR_REPORT_ADDR",
            M::CpHqdPqRptrReportAddrHi => "CP_HQD_PQ_RPTR_REPORT_ADDR_HI",
            M::CpHqdPqWptrPollAddr => "CP_HQD_PQ_WPTR_POLL_ADDR",
            M::CpHqdPqWptrPollAddrHi => "CP_HQD_PQ_WPTR_POLL_ADDR_HI",
            M::CpHqdPqDoorbellControl => "CP_HQD_PQ_DOORBELL_CONTROL",
            M::CpHqdPqControl => "CP_HQD_PQ_CONTROL",
            M::CpHqdDequeueRequest => "CP_HQD_DEQUEUE_REQUEST",
            M::CpMqdControl => "CP_MQD_CONTROL",
            M::CpHqdEopBaseAddr => "CP_HQD_EOP_BASE_ADDR",
            M::CpHqdEopBaseAddrHi => "CP_HQD_EOP_BASE_ADDR_HI",
            M::CpHqdEopControl => "CP_HQD_EOP_CONTROL",
            M::CpHqdPqWptrLo => "CP_HQD_PQ_WPTR_LO",
            M::CpHqdPqWptrHi => "CP_HQD_PQ_WPTR_HI",
            M::SpiComputeQueueReset => "SPI_COMPUTE_QUEUE_RESET",
            M::CpHqdError => "CP_HQD_ERROR",
            M::CpcUtcl1Error => "CPC_UTCL1_ERROR",
            M::CpcUtcl1Status => "CPC_UTCL1_STATUS",
            M::CpCpcStatus => "CP_CPC_STATUS",
            M::CpCpcStalledStat1 => "CP_CPC_STALLED_STAT1",
        }
    }

    /// Only read (the firmware's version; GX-7m3: the stall's report).
    pub fn read_only(self) -> bool {
        matches!(self, M::CpMesGp3Lo | M::CpHqdError | M::CpcUtcl1Status | M::CpCpcStatus | M::CpCpcStalledStat1)
    }

    /// Banked by GRBM_GFX_CNTL (the HQD/MQD registers).
    pub fn banked(self) -> bool {
        !matches!(self, M::CpMesCntl | M::RlcCpSchedulers | M::CpcUtcl1Error | M::CpcUtcl1Status | M::CpCpcStatus | M::CpCpcStalledStat1)
    }
}

/// Register fields (gc_11_0_0_sh_mask.h).
pub mod f {
    pub const MES_INVALIDATE_ICACHE: u32 = 1 << 4;
    pub const MES_PIPE0_RESET: u32 = 1 << 16;
    pub const MES_PIPE1_RESET: u32 = 1 << 17;
    pub const MES_PIPE0_ACTIVE: u32 = 1 << 26;
    pub const MES_PIPE1_ACTIVE: u32 = 1 << 27;
    pub const MES_HALT: u32 = 1 << 30;
    pub const GRBM_MEID_SHIFT: u32 = 2;
    pub const GRBM_VMID_SHIFT: u32 = 4;
    pub const GRBM_QUEUEID_SHIFT: u32 = 8;
    pub const HQD_ACTIVE: u32 = 1;
    pub const DEQUEUE_REQ: u32 = 1;
    /// CP_HQD_DEQUEUE_REQUEST 2: the reset dequeue (mes_v11_0_reset_queue_mmio, 7.1); 1 drains, which a stalled
    /// queue never finishes.
    pub const DEQUEUE_RESET: u32 = 2;
    pub const SPI_QUEUE_RESET: u32 = 1;
    /// CPC_UTCL1_ERROR.ERROR_DETECTED_HALT (gc_11_0_0_sh_mask.h:15347).
    pub const CPC_ERROR_HALT: u32 = 1;
    pub const PQ_QUEUE_SIZE_MASK: u32 = 0x3f;
    pub const PQ_RPTR_BLOCK_SIZE_SHIFT: u32 = 8;
    pub const PQ_RPTR_BLOCK_SIZE_MASK: u32 = 0x3f00;
    pub const PQ_NO_UPDATE_RPTR: u32 = 1 << 27;
    pub const PQ_UNORD_DISPATCH: u32 = 1 << 28;
    pub const PQ_TUNNEL_DISPATCH: u32 = 1 << 29;
    pub const PQ_PRIV_STATE: u32 = 1 << 30;
    pub const PQ_KMD_QUEUE: u32 = 1 << 31;
    pub const DB_OFFSET_MASK: u32 = 0x0fff_fffc;
    pub const DB_EN: u32 = 1 << 30;
    pub const DB_HIT: u32 = 1 << 31;
    pub const EOP_SIZE_MASK: u32 = 0x3f;
    pub const PRELOAD_SIZE_SHIFT: u32 = 8;
    pub const PRELOAD_SIZE_MASK: u32 = 0x3ff00;
    pub const MQD_VMID_MASK: u32 = 0xf;
    pub const IB_MIN_AVAIL_SHIFT: u32 = 20;
    pub const IB_MIN_AVAIL_MASK: u32 = 0x30_0000;
    pub const RPTR_ADDR_HI_MASK: u32 = 0xffff;
    /// The GFX HQD's fields (the MES-mapped GFX queue's MQD).
    pub const GFX_RB_BUFSZ_MASK: u32 = 0x3f;
    pub const GFX_RB_BLKSZ_SHIFT: u32 = 8;
    pub const GFX_RB_BLKSZ_MASK: u32 = 0x3f00;
    pub const GFX_MQD_VMID_MASK: u32 = 0xf;
    pub const GFX_MQD_PRIV_STATE: u32 = 1 << 8;
    pub const GFX_MQD_CACHE_POLICY_MASK: u32 = 0x0300_0000;
    pub const GFX_QUANTUM_EN: u32 = 1;
}

/// Register defaults Linux starts from (gc_11_0_0_default.h).
pub mod dflt {
    pub const CP_HQD_PQ_CONTROL: u32 = 0x0030_8509;
    pub const CP_HQD_EOP_CONTROL: u32 = 0x0000_0006;
    pub const CP_HQD_PERSISTENT_STATE: u32 = 0x0be0_5501;
    pub const CP_HQD_IB_CONTROL: u32 = 0x0030_0000;
    pub const CP_HQD_IQ_TIMER: u32 = 0;
    pub const CP_HQD_QUANTUM: u32 = 0;
    pub const CP_MQD_CONTROL: u32 = 0x0000_0100;
    pub const CP_GFX_MQD_CONTROL: u32 = 0x0000_0100;
    pub const CP_GFX_HQD_QUANTUM: u32 = 0x0000_0a01;
    pub const CP_GFX_HQD_CNTL: u32 = 0x00a0_0000;
}

/// MES is ME 3; its pipes (AMDGPU_MES_SCHED_PIPE, AMDGPU_MES_KIQ_PIPE).
pub const MES_ME: u32 = 3;
pub const SCHED_PIPE: u32 = 0;
pub const KIQ_PIPE: u32 = 1;
pub const PIPES: [u32; 2] = [SCHED_PIPE, KIQ_PIPE];

/// The doorbells in 64-bit units (soc21.c:480-496, amdgpu_doorbell.h: AMDGPU_NAVI10_DOORBELL_MES_RING0/1,
/// _MEC_RING0) and as dword indexes (`<< 1`, mes_v11_0.c ring_init/kiq_ring_init; the capture's KIQ HQD reads
/// DOORBELL_OFFSET 0x18 = MES_RING1 << 1).
pub const DOORBELL_MES_RING0_64: u32 = 0x0b;
pub const DOORBELL_MES_RING1_64: u32 = 0x0c;
pub const DOORBELL_MEC_RING0_64: u32 = 0x03;
pub const SCHED_DOORBELL_DW: u32 = DOORBELL_MES_RING0_64 << 1;
pub const KIQ_DOORBELL_DW: u32 = DOORBELL_MES_RING1_64 << 1;
pub const COMPUTE_DOORBELL_DW: u32 = DOORBELL_MEC_RING0_64 << 1;

/// MES_EOP_SIZE (mes_v11_0.c:67) and GFX11_MEC_HPD_SIZE (gfx_v11_0.c:55): 2048 bytes each.
pub const EOP_BYTES: u64 = 2048;
/// sizeof(struct v11_compute_mqd) and sizeof(struct v11_gfx_mqd) (v11_structs.h): 512 dwords.
pub const MQD_DW: usize = 512;
/// The firmware version field of CP_MES_GP3_LO (AMDGPU_MES_VERSION_MASK) and the versions Linux 7.1 checks:
/// legacy queues mapped through MES from 0x47 on (kiq_hw_init :1599), SET_HW_RESOURCES_1 from 0x52 (:1680).
pub const VERSION_MASK: u32 = 0xfff;
pub const API_VERSION_SHIFT: u32 = 12;
pub const API_VERSION_MASK: u32 = 0xfff;
pub const LEGACY_MAP_MIN: u32 = 0x47;
pub const HW_RSRC_1_MIN: u32 = 0x52;
/// mes_v11_0_enable's udelay after the unhalt (Linux 7.1: 500 us; 6.6: 50 us).
pub const ENABLE_DELAY_US: u64 = 500;
/// submit_pkt_and_poll_completion's wait (2100 ms) and kiq_dequeue's (adev->usec_timeout).
pub const API_TIMEOUT_US: u64 = 2_100_000;
pub const DEQUEUE_TIMEOUT_US: u64 = 100_000;

/// GRBM_GFX_CNTL selecting (me, pipe, queue, vmid) (soc21_grbm_select).
pub const fn grbm_select(me: u32, pipe: u32, queue: u32, vmid: u32) -> u32 {
    (pipe & 3) | ((me & 3) << f::GRBM_MEID_SHIFT) | ((vmid & 0xf) << f::GRBM_VMID_SHIFT) | ((queue & 7) << f::GRBM_QUEUEID_SHIFT)
}

/// mes_v11_0_enable(true)'s first write: both pipes in reset, everything else as read.
pub fn cntl_reset(v: u32) -> u32 {
    v | f::MES_PIPE0_RESET | f::MES_PIPE1_RESET
}

/// The unhalt (written from 0): both pipes active, halt and resets cleared.
pub const CNTL_RUN: u32 = f::MES_PIPE0_ACTIVE | f::MES_PIPE1_ACTIVE;

/// mes_v11_0_enable(false): pipes inactive, icache invalidated, both pipes reset, halted.
pub fn cntl_halt(v: u32) -> u32 {
    (v & !(f::MES_PIPE0_ACTIVE | f::MES_PIPE1_ACTIVE)) | f::MES_INVALIDATE_ICACHE | f::MES_PIPE0_RESET | f::MES_PIPE1_RESET | f::MES_HALT
}

/// MES runs: halt clear and a pipe active.
pub fn cntl_running(v: u32) -> bool {
    v != u32::MAX && v & f::MES_HALT == 0 && v & (f::MES_PIPE0_ACTIVE | f::MES_PIPE1_ACTIVE) != 0
}

/// kiq_setting (7.1 mes_v11_0.c:1548): RLC_CP_SCHEDULERS' low byte names the KIQ (me << 5 | pipe << 3 | queue),
/// bit 7 set.
pub fn rlc_cp_schedulers(v: u32, me: u32, pipe: u32, queue: u32) -> u32 {
    (v & 0xffff_ff00) | (me << 5) | (pipe << 3) | queue | 0x80
}

/// order_base_2 (Linux's ceil(log2)).
pub fn order_base_2(x: u64) -> u32 {
    if x <= 1 { 0 } else { 64 - (x - 1).leading_zeros() }
}

/// The 64-bit MES microcode start (`mes_hdr->mes_uc_start_addr`) as CP_MES_PRGRM_CNTR_START/_HI take it (>> 2).
pub fn prgrm_cntr(uc_start: u64) -> (u32, u32) {
    let a = uc_start >> 2;
    (a as u32, (a >> 32) as u32)
}

/// SET_HW_RESOURCES's masks as Linux printed them on the 780M (the 27.10. capture's dmesg: "MES: vmid_mask_mmhub
/// 0x0000ff00, vmid_mask_gfxhub 0x0000ff00", "gfx_hqd_mask 0x00000002, compute_hqd_mask 0x0000000c,
/// sdma_hqd_mask 0x000000fc") and as amdgpu_mes_init (7.1 amdgpu_mes.c:104-170) spreads them over the pipes:
/// one GFX pipe, four MEC pipes, one SDMA instance. Wrong masks hang MES silently (roadmap wave 27).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwMasks {
    pub vmid_mmhub: u32,
    pub vmid_gfxhub: u32,
    pub gfx_hqd: [u32; 2],
    pub compute_hqd: [u32; 8],
    pub sdma_hqd: [u32; 2],
}

impl HwMasks {
    pub const LINUX_780M: HwMasks = HwMasks { vmid_mmhub: 0xff00, vmid_gfxhub: 0xff00, gfx_hqd: [0x2, 0], compute_hqd: [0xc, 0xc, 0xc, 0xc, 0, 0, 0, 0], sdma_hqd: [0xfc, 0] };
}

/// Linux's kernel queues on the masks' free slots: GFX ring 0 is ME 0 pipe 0 queue 0, compute ring 0 (comp_1.0.0)
/// MEC 1 pipe 0 queue 0 (the masks keep queue 0 of each for the kernel).
pub const GFX_PIPE: u32 = 0;
pub const GFX_QUEUE: u32 = 0;
pub const COMPUTE_ME: u32 = 1;
pub const COMPUTE_PIPE: u32 = 0;
pub const COMPUTE_QUEUE: u32 = 0;
