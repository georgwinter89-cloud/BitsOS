//! VCN 4.0.2 (video decode, GX-8v): Linux `vcn_v4_0` (linux-7.3.1 amdgpu/vcn_v4_0.c). Its discovery revision carries
//! the encode/decode-off bits 7:6 (`IpEntry::vcn_config`); the VCN table's fuse bits say which codecs are off.
//! `jpeg_v4_0` runs on VCN's firmware and has no file of its own; BitsOS does not start its ring (JPEG rings stay 0).
//!
//! The registers the stage touches, as (segment, dword) of vcn_4_0_0_offset.h (`R`, all in segment 1, the 780M's
//! base 0x7E00), the field values of the DPG start (vcn_v4_0.c:996-1130, `mc_resume_dpg_mode` :509-616) and the
//! doorbell (soc21.c:508, AMDGPU_NAVI10_DOORBELL64_VCN0_1 0x188; vcn_v4_0.c:225 `(0x188 << 1) + 2`; NBIO's range
//! nbio_v7_7.c:87-105). The run is `gpucore::vcn`.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;

pub static MODULE: Module = Module {
    name: "vcn4_0_2",
    role: Role::Vcn,
    versions: &[v(4, 0, 2)],
    linux: "vcn_v4_0 + jpeg_v4_0",
    linux_also: &[v(4, 0, 0), v(4, 0, 4)],
    firmware: Firmware::Files { prefix: "vcn", needs: &[Need::one("")] },
    state: State::Present("gpucore::vcn (GX-8v: at every boot; PowerUpVcn, the DPG start through the PSP, the unified ring in the carve-out, MMHUB's contexts 1-7 for programs' jobs)"),
};

/// One VCN 0 register (Linux's `regUVD_*` / `regVCN_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum R {
    PowerStatus,
    Status,
    VcpuCntl,
    MastintEn,
    LmiCtrl,
    LmiCtrl2,
    MpcCntl,
    MpcSetMuxa0,
    MpcSetMuxb0,
    MpcSetMux,
    CacheBarLo,
    CacheBarHi,
    CacheOffset0,
    CacheSize0,
    Cache1BarLo,
    Cache1BarHi,
    CacheOffset1,
    CacheSize1,
    Cache2BarLo,
    Cache2BarHi,
    CacheOffset2,
    CacheSize2,
    Nc0BarLo,
    Nc0BarHi,
    NcOffset0,
    NcSize0,
    AddrConfig,
    RbBaseLo,
    RbBaseHi,
    RbSize,
    RbRptr,
    RbWptr,
    RbEnable,
    Rb1DbCtrl,
}

impl R {
    pub const ALL: [R; 34] = [
        R::PowerStatus,
        R::Status,
        R::VcpuCntl,
        R::MastintEn,
        R::LmiCtrl,
        R::LmiCtrl2,
        R::MpcCntl,
        R::MpcSetMuxa0,
        R::MpcSetMuxb0,
        R::MpcSetMux,
        R::CacheBarLo,
        R::CacheBarHi,
        R::CacheOffset0,
        R::CacheSize0,
        R::Cache1BarLo,
        R::Cache1BarHi,
        R::CacheOffset1,
        R::CacheSize1,
        R::Cache2BarLo,
        R::Cache2BarHi,
        R::CacheOffset2,
        R::CacheSize2,
        R::Nc0BarLo,
        R::Nc0BarHi,
        R::NcOffset0,
        R::NcSize0,
        R::AddrConfig,
        R::RbBaseLo,
        R::RbBaseHi,
        R::RbSize,
        R::RbRptr,
        R::RbWptr,
        R::RbEnable,
        R::Rb1DbCtrl,
    ];

    /// The dword in segment 1 (vcn_4_0_0_offset.h, host/gen/dcn314/tables/vcn_4_0_0.tsv) and Linux's name.
    pub fn at(self) -> (u32, &'static str) {
        match self {
            R::PowerStatus => (0x0002, "UVD_POWER_STATUS"),
            R::Status => (0x0143, "UVD_STATUS"),
            R::VcpuCntl => (0x0196, "UVD_VCPU_CNTL"),
            R::MastintEn => (0x00e1, "UVD_MASTINT_EN"),
            R::LmiCtrl => (0x02b0, "UVD_LMI_CTRL"),
            R::LmiCtrl2 => (0x02ae, "UVD_LMI_CTRL2"),
            R::MpcCntl => (0x01d4, "UVD_MPC_CNTL"),
            R::MpcSetMuxa0 => (0x01d6, "UVD_MPC_SET_MUXA0"),
            R::MpcSetMuxb0 => (0x01d8, "UVD_MPC_SET_MUXB0"),
            R::MpcSetMux => (0x01da, "UVD_MPC_SET_MUX"),
            R::CacheBarLo => (0x0244, "UVD_LMI_VCPU_CACHE_64BIT_BAR_LOW"),
            R::CacheBarHi => (0x0245, "UVD_LMI_VCPU_CACHE_64BIT_BAR_HIGH"),
            R::CacheOffset0 => (0x0180, "UVD_VCPU_CACHE_OFFSET0"),
            R::CacheSize0 => (0x0181, "UVD_VCPU_CACHE_SIZE0"),
            R::Cache1BarLo => (0x0270, "UVD_LMI_VCPU_CACHE1_64BIT_BAR_LOW"),
            R::Cache1BarHi => (0x0271, "UVD_LMI_VCPU_CACHE1_64BIT_BAR_HIGH"),
            R::CacheOffset1 => (0x0182, "UVD_VCPU_CACHE_OFFSET1"),
            R::CacheSize1 => (0x0183, "UVD_VCPU_CACHE_SIZE1"),
            R::Cache2BarLo => (0x0274, "UVD_LMI_VCPU_CACHE2_64BIT_BAR_LOW"),
            R::Cache2BarHi => (0x0275, "UVD_LMI_VCPU_CACHE2_64BIT_BAR_HIGH"),
            R::CacheOffset2 => (0x0184, "UVD_VCPU_CACHE_OFFSET2"),
            R::CacheSize2 => (0x0185, "UVD_VCPU_CACHE_SIZE2"),
            R::Nc0BarLo => (0x0240, "UVD_LMI_VCPU_NC0_64BIT_BAR_LOW"),
            R::Nc0BarHi => (0x0241, "UVD_LMI_VCPU_NC0_64BIT_BAR_HIGH"),
            R::NcOffset0 => (0x0192, "UVD_VCPU_NONCACHE_OFFSET0"),
            R::NcSize0 => (0x0193, "UVD_VCPU_NONCACHE_SIZE0"),
            R::AddrConfig => (0x0042, "UVD_GFX10_ADDR_CONFIG"),
            R::RbBaseLo => (0x00ea, "UVD_RB_BASE_LO"),
            R::RbBaseHi => (0x00eb, "UVD_RB_BASE_HI"),
            R::RbSize => (0x00ec, "UVD_RB_SIZE"),
            R::RbRptr => (0x00ac, "UVD_RB_RPTR"),
            R::RbWptr => (0x00ad, "UVD_RB_WPTR"),
            R::RbEnable => (0x0085, "VCN_RB_ENABLE"),
            R::Rb1DbCtrl => (0x0072, "VCN_RB1_DB_CTRL"),
        }
    }

    pub fn name(self) -> &'static str {
        self.at().1
    }
}

/// The CPU writes of the DPG start and stop (vcn_v4_0.c:1003-1129, :1563-1593); everything else goes into the SRAM
/// list the PSP loads. The capture of 27.10. shows exactly these (UVD_POWER_STATUS, the ring's five, the doorbell).
pub const CPU_WRITES: [R; 8] = [R::PowerStatus, R::RbBaseLo, R::RbBaseHi, R::RbSize, R::RbRptr, R::RbWptr, R::RbEnable, R::Rb1DbCtrl];

/// Field masks (vcn_4_0_0_sh_mask.h).
pub mod f {
    pub const POWER_STATUS_MASK: u32 = 0x3;
    pub const PG_MODE: u32 = 1 << 2;
    pub const PG_EN: u32 = 1 << 8;
    pub const VCPU_CLK_EN: u32 = 1 << 9;
    pub const VCPU_BLK_RST: u32 = 1 << 28;
    pub const VCPU_PRB_TIMEOUT_SHIFT: u32 = 20;
    pub const MASTINT_VCPU_EN: u32 = 1 << 1;
    pub const RB1_EN: u32 = 1 << 2;
    pub const DB_EN: u32 = 1 << 30;
    pub const DB_OFFSET_SHIFT: u32 = 2;
    /// UVD_LMI_CTRL as vcn_v4_0_start_dpg_mode writes it.
    pub const LMI_CTRL: u32 = 0x100 | 0x200 | 0x4000 | 0x1000 | 0x2000 | 0x20_0000 | 8 | 0x10_0000;
    /// UVD_LMI_CTRL2: RE_OFLD_MIF_WR_REQ_NUM 0x1f.
    pub const LMI_CTRL2: u32 = 0x1f << 17;
    pub const MPC_CNTL: u32 = 0x2 << 3;
    pub const MPC_MUXA0: u32 = 1 << 6 | 2 << 12 | 3 << 18 | 4 << 24;
    pub const MPC_MUXB0: u32 = MPC_MUXA0;
    pub const MPC_MUX: u32 = 1 << 3 | 2 << 6;
}

/// VCN 0's unified ring in 64-bit doorbell units, and as the dword index the ring rings (vcn_v4_0.c:225).
pub const DOORBELL_INDEX64: u32 = 0x188;
pub const DOORBELL_DW: u32 = (DOORBELL_INDEX64 << 1) + 2;
/// The range NBIO routes to VCN 0: OFFSET `0x188 << 1`, SIZE 8 (vcn_v4_0.c:337, nbio_v7_7.c:96-97).
pub const DOORBELL_RANGE_START_DW: u32 = DOORBELL_INDEX64 << 1;
pub const DOORBELL_RANGE_SIZE: u32 = 8;
/// GDC0_BIF_VCN0_DOORBELL_RANGE in NBIO 7.7's segment 3 (nbio_7_7_0_offset.h:4004), reached through RSMU.
pub const VCN0_DOORBELL_RANGE: (usize, u32) = (3, 0x4f_0af3);

/// GDC0_BIF_VCN0_DOORBELL_RANGE with OFFSET (bits 11:2) and SIZE (20:16).
pub fn doorbell_range(v: u32, start_dw: u32, size: u32) -> u32 {
    (v & !0x001f_0ffc) | ((start_dw << 2) & 0xffc) | ((size << 16) & 0x001f_0000)
}

/// VCN_RB1_DB_CTRL: the doorbell's dword index, enabled (the capture: 0x40000c48).
pub fn rb1_db_ctrl(index_dw: u32) -> u32 {
    index_dw << f::DB_OFFSET_SHIFT | f::DB_EN
}

/// Linux's ring: `amdgpu_ring_init(.., 512, ..)` doubled by sched_hw_submission and rounded: 1024 dwords, UVD_RB_SIZE
/// 0x400 in the capture.
pub const RING_DW: u64 = 1024;
/// Linux's waits (adev->usec_timeout) for the ring test and the register polls.
pub const LINUX_TIMEOUT_US: u64 = 100_000;
/// `GFX_FW_TYPE_VCN0_RAM` (psp_gfx_if.h:260): the SRAM list's LOAD_IP_FW type.
pub const FW_TYPE_VCN0_RAM: u32 = 49;
/// Interrupts (soc15_ih_clientid.h:49, irqsrcs_vcn_4_0.h:27): client VCN, the unified ring's trap.
pub const IH_CLIENT_VCN: u32 = 0x10;
pub const IH_SRC_ENC_GENERAL_PURPOSE: u32 = 119;
/// UVD_STATUS reads this in DPG mode on the 780M (the capture, right after the start): never data.
pub const DPG_STATUS_READ: u32 = 0xdead_beef;
