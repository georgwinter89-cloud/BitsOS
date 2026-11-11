//! GFX 11.0.1 (gfx1103): Linux `gfx_v11_0` with `imu_v11_0` (linux-6.6 amdgpu/gfx_v11_0.c, imu_v11_0.c). With
//! PSP loading the microcode (APUs, `AMDGPU_FW_LOAD_PSP`), the files go to the PSP ring as LOAD_IP_FW
//! commands; the RLC backdoor path (imu_v11_0.c program_rlc_ram) is not this GPU's.
//!
//! GX-7j: the registers of the GFX tail and GFX ring 0 (`R`, as (segment, dword) of gc_11_0_0_offset.h in GC's
//! bases), their fields and Linux's values: the golden registers of GC 11.0.1 (gfx_v11_0.c:86-97 and :266, plus
//! TCP_CNTL bit 29 that Linux 7.1 adds on the 780M, the 27.10. capture), `constants_init` (:1694), the RLC
//! resume of the PSP path (:1973, init_csb :1757, enable_srm :1819), `cp_set_doorbell_range` (:3572) and the
//! direct `cp_gfx_resume` (:3230-3300) with `cp_gfx_start` (:3118) and `cp_gfx_enable` (:2630). The run is
//! `gpucore::gfxring`.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::fw::Need;
use alloc::vec::Vec;

pub static MODULE: Module = Module {
    name: "gfx11_0_1",
    role: Role::Gfx,
    versions: &[v(11, 0, 1)],
    linux: "gfx_v11_0",
    linux_also: &[v(11, 0, 0), v(11, 0, 2), v(11, 0, 3), v(11, 0, 4)],
    firmware: Firmware::Files {
        prefix: "gc",
        needs: &[Need::one("_pfp"), Need::one("_me"), Need::one("_mec"), Need::one("_rlc"), Need::one("_imu")],
    },
    state: State::Present("gpucore::gfxring (GX-7j: golden registers, constants, RLC resume, GFX ring 0 driven by the CP directly with MES idle, ring and IB tests, RELEASE_MEM fence)"),
};

/// GX-7b, read-only facts (gc_11_0_0_offset.h; (segment, dword) in GC's bases). All of them are GC registers:
/// read only while GC is powered (`gpucore::classify` checks SMUIO's GFXOFF status, then these flags first).
/// Bits 2:1: 0 GC off, 1 leaving GFXOFF, 2 on, 3 entering GFXOFF (gx-scout-bringup.md section 7).
pub const GFX_IMU_MSG_FLAGS: (usize, u32) = (1, 0x403f);
/// GC 11.0.1 reads its bootload status at this alias (gfx_v11_0.c:61-62, :2566-2570); bit 31 BOOTLOAD_COMPLETE.
pub const RLC_RLCS_BOOTLOAD_STATUS_11_0_1: (usize, u32) = (1, 0x4e7e);
/// Bit 0 RLC_ENABLE_F32.
pub const RLC_CNTL: (usize, u32) = (1, 0x4c00);
/// GX-9b: gfx_v11_cntl_pg (gfx_v11_0.c:4991-5021; gc_11_0_0_offset.h).
pub const RLC_SAFE_MODE: (usize, u32) = (1, 0x0980);
pub const RLC_PG_CNTL: (usize, u32) = (1, 0x4c43);
pub const RLC_PG_DELAY_3: (usize, u32) = (1, 0x4c78);
/// Zero when the CP is idle (gfx_v11_0.c:2563, :2640).
pub const CP_STAT: (usize, u32) = (0, 0xf40);
/// Bit 31 GUI_ACTIVE.
pub const GRBM_STATUS: (usize, u32) = (0, 0xda4);
/// Bits 28/26/24: ME/PFP/CE halted.
pub const CP_ME_CNTL: (usize, u32) = (1, 0x803);
/// Bit 30 MES_HALT (and the pipe resets).
pub const CP_MES_CNTL: (usize, u32) = (1, 0x2807);

/// One GC register of the GFX tail and ring 0 (instance 0). The GDS registers are per VMID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum R {
    // golden (the capture's order)
    CgttGsNggClkCtrl,
    CgttWdClkCtrl,
    CpfGcrCntl,
    Gl2cCtrl3,
    PaClEnhance,
    PaScEnhance3,
    PaScVrsSurfaceCntl1,
    TaCntlAux,
    TcpCntl2,
    TcpCntl,
    // constants_init
    GrbmCntl,
    GrbmGfxIndex,
    GrbmGfxCntl,
    ShMemConfig,
    ShMemBases,
    SpiGdbgPerVmidCntl,
    GdsBase(u8),
    GdsSize(u8),
    GdsGws(u8),
    GdsOa(u8),
    CcGcSaUnitDisable,
    GcUserSaUnitDisable,
    CcRbBackendDisable,
    GcUserRbBackendDisable,
    CcGcShaderArrayConfig,
    GcUserShaderArrayConfig,
    CgtsTccDisable,
    CgtsUserTccDisable,
    TaCntl2,
    // RLC resume (PSP path)
    RlcCsibAddrHi,
    RlcCsibAddrLo,
    RlcCsibLength,
    RlcSrmCntl,
    // CP
    CpRbDoorbellRangeLower,
    CpRbDoorbellRangeUpper,
    CpMecDoorbellRangeLower,
    CpMecDoorbellRangeUpper,
    CpMecCntl,
    CpMeCntl,
    CpMesCntl,
    CpStat,
    GrbmStatus,
    CpMaxContext,
    CpDeviceId,
    ScratchReg0,
    CpRbWptrDelay,
    CpRbVmid,
    CpRb0Cntl,
    CpRb0Wptr,
    CpRb0WptrHi,
    CpRb0Rptr,
    CpRbRptrWr,
    CpRb0RptrAddr,
    CpRb0RptrAddrHi,
    CpRbWptrPollAddrLo,
    CpRbWptrPollAddrHi,
    CpRb0Base,
    CpRb0BaseHi,
    CpRbActive,
    CpRbDoorbellControl,
    // GX-7j3: GFX pipe 0 queue 0's MQD pointer (what gfx_v11_0_gfx_mqd_init's MQD and MES set; the direct path
    // left it 0 and the CP read page 0 of VMID 0, the Mini-PC 29.10.).
    CpGfxMqdBaseAddr,
    CpGfxMqdBaseAddrHi,
    CpGfxMqdControl,
}

/// The VMIDs the GDS registers exist for (AMDGPU_NUM_VMID).
pub const NUM_VMID: u8 = 16;
/// GFXHUB's kernel and graphics VMIDs are 0..8; KFD's compute VMIDs start here (first_kfd_vmid).
pub const FIRST_KFD_VMID: u8 = 8;

impl R {
    /// Every register (the GDS ones for every VMID).
    pub fn all() -> Vec<R> {
        let mut v = alloc::vec![
            R::CgttGsNggClkCtrl,
            R::CgttWdClkCtrl,
            R::CpfGcrCntl,
            R::Gl2cCtrl3,
            R::PaClEnhance,
            R::PaScEnhance3,
            R::PaScVrsSurfaceCntl1,
            R::TaCntlAux,
            R::TcpCntl2,
            R::TcpCntl,
            R::GrbmCntl,
            R::GrbmGfxIndex,
            R::GrbmGfxCntl,
            R::ShMemConfig,
            R::ShMemBases,
            R::SpiGdbgPerVmidCntl,
            R::CcGcSaUnitDisable,
            R::GcUserSaUnitDisable,
            R::CcRbBackendDisable,
            R::GcUserRbBackendDisable,
            R::CcGcShaderArrayConfig,
            R::GcUserShaderArrayConfig,
            R::CgtsTccDisable,
            R::CgtsUserTccDisable,
            R::TaCntl2,
            R::RlcCsibAddrHi,
            R::RlcCsibAddrLo,
            R::RlcCsibLength,
            R::RlcSrmCntl,
            R::CpRbDoorbellRangeLower,
            R::CpRbDoorbellRangeUpper,
            R::CpMecDoorbellRangeLower,
            R::CpMecDoorbellRangeUpper,
            R::CpMecCntl,
            R::CpMeCntl,
            R::CpMesCntl,
            R::CpStat,
            R::GrbmStatus,
            R::CpMaxContext,
            R::CpDeviceId,
            R::ScratchReg0,
            R::CpRbWptrDelay,
            R::CpRbVmid,
            R::CpRb0Cntl,
            R::CpRb0Wptr,
            R::CpRb0WptrHi,
            R::CpRb0Rptr,
            R::CpRbRptrWr,
            R::CpRb0RptrAddr,
            R::CpRb0RptrAddrHi,
            R::CpRbWptrPollAddrLo,
            R::CpRbWptrPollAddrHi,
            R::CpRb0Base,
            R::CpRb0BaseHi,
            R::CpRbActive,
            R::CpRbDoorbellControl,
            R::CpGfxMqdBaseAddr,
            R::CpGfxMqdBaseAddrHi,
            R::CpGfxMqdControl,
        ];
        for i in 0..NUM_VMID {
            v.extend([R::GdsBase(i), R::GdsSize(i), R::GdsGws(i), R::GdsOa(i)]);
        }
        v
    }

    /// (segment, dword) in GC's bases (gc_11_0_0_offset.h; CGTT_WD_CLK_CTRL from gfx_v11_0.c:59, TCP_CNTL from
    /// gc_11_0_3_offset.h: neither is in gc_11_0_0's header).
    pub fn at(self) -> (usize, u32) {
        match self {
            R::CgttGsNggClkCtrl => (1, 0x5087),
            R::CgttWdClkCtrl => (1, 0x5086),
            R::CpfGcrCntl => (0, 0x1f53),
            R::Gl2cCtrl3 => (1, 0x2e0c),
            R::PaClEnhance => (0, 0x1025),
            R::PaScEnhance3 => (1, 0x0944),
            R::PaScVrsSurfaceCntl1 => (1, 0x0960),
            R::TaCntlAux => (0, 0x12e2),
            R::TcpCntl2 => (1, 0x19a3),
            R::TcpCntl => (1, 0x19a2),
            R::GrbmCntl => (0, 0x0da0),
            R::GrbmGfxIndex => (1, 0x2200),
            R::GrbmGfxCntl => (1, 0x0900),
            R::ShMemConfig => (1, 0x09e4),
            R::ShMemBases => (1, 0x09e3),
            R::SpiGdbgPerVmidCntl => (0, 0x1f72),
            R::GdsBase(i) => (0, 0x20a0 + 2 * u32::from(i)),
            R::GdsSize(i) => (0, 0x20a1 + 2 * u32::from(i)),
            R::GdsGws(i) => (0, 0x20c0 + u32::from(i)),
            R::GdsOa(i) => (0, 0x20d0 + u32::from(i)),
            R::CcGcSaUnitDisable => (0, 0x0fe9),
            R::GcUserSaUnitDisable => (1, 0x5b92),
            R::CcRbBackendDisable => (0, 0x13dd),
            R::GcUserRbBackendDisable => (1, 0x5b94),
            R::CcGcShaderArrayConfig => (0, 0x100f),
            R::GcUserShaderArrayConfig => (1, 0x5b90),
            R::CgtsTccDisable => (1, 0x5006),
            R::CgtsUserTccDisable => (1, 0x5b96),
            R::TaCntl2 => (0, 0x12e5),
            R::RlcCsibAddrHi => (1, 0x0988),
            R::RlcCsibAddrLo => (1, 0x0987),
            R::RlcCsibLength => (1, 0x0989),
            R::RlcSrmCntl => (1, 0x4c80),
            R::CpRbDoorbellRangeLower => (0, 0x1dfa),
            R::CpRbDoorbellRangeUpper => (0, 0x1dfb),
            R::CpMecDoorbellRangeLower => (0, 0x1dfc),
            R::CpMecDoorbellRangeUpper => (0, 0x1dfd),
            R::CpMecCntl => (1, 0x0802),
            R::CpMeCntl => CP_ME_CNTL,
            R::CpMesCntl => CP_MES_CNTL,
            R::CpStat => CP_STAT,
            R::GrbmStatus => GRBM_STATUS,
            R::CpMaxContext => (0, 0x1e4e),
            R::CpDeviceId => (0, 0x1deb),
            R::ScratchReg0 => (1, 0x2040),
            R::CpRbWptrDelay => (0, 0x0f61),
            R::CpRbVmid => (0, 0x1df1),
            R::CpRb0Cntl => (0, 0x1de1),
            R::CpRb0Wptr => (0, 0x1df4),
            R::CpRb0WptrHi => (0, 0x1df5),
            R::CpRb0Rptr => (0, 0x0f60),
            R::CpRbRptrWr => (0, 0x1de2),
            R::CpRb0RptrAddr => (0, 0x1de3),
            R::CpRb0RptrAddrHi => (0, 0x1de4),
            R::CpRbWptrPollAddrLo => (0, 0x1e8b),
            R::CpRbWptrPollAddrHi => (0, 0x1e8c),
            R::CpRb0Base => (0, 0x1de0),
            R::CpRb0BaseHi => (0, 0x1e51),
            R::CpRbActive => (0, 0x1f40),
            R::CpRbDoorbellControl => (0, 0x1e8d),
            R::CpGfxMqdBaseAddr => (0, 0x1e7e),
            R::CpGfxMqdBaseAddrHi => (0, 0x1e7f),
            R::CpGfxMqdControl => (0, 0x1e9a),
        }
    }

    /// Linux's name (GDS: with the VMID).
    pub fn name(self) -> &'static str {
        const GDS_BASE: [&str; 16] = [
            "GDS_VMID0_BASE", "GDS_VMID1_BASE", "GDS_VMID2_BASE", "GDS_VMID3_BASE", "GDS_VMID4_BASE", "GDS_VMID5_BASE", "GDS_VMID6_BASE", "GDS_VMID7_BASE",
            "GDS_VMID8_BASE", "GDS_VMID9_BASE", "GDS_VMID10_BASE", "GDS_VMID11_BASE", "GDS_VMID12_BASE", "GDS_VMID13_BASE", "GDS_VMID14_BASE", "GDS_VMID15_BASE",
        ];
        const GDS_SIZE: [&str; 16] = [
            "GDS_VMID0_SIZE", "GDS_VMID1_SIZE", "GDS_VMID2_SIZE", "GDS_VMID3_SIZE", "GDS_VMID4_SIZE", "GDS_VMID5_SIZE", "GDS_VMID6_SIZE", "GDS_VMID7_SIZE",
            "GDS_VMID8_SIZE", "GDS_VMID9_SIZE", "GDS_VMID10_SIZE", "GDS_VMID11_SIZE", "GDS_VMID12_SIZE", "GDS_VMID13_SIZE", "GDS_VMID14_SIZE", "GDS_VMID15_SIZE",
        ];
        const GDS_GWS: [&str; 16] = [
            "GDS_GWS_VMID0", "GDS_GWS_VMID1", "GDS_GWS_VMID2", "GDS_GWS_VMID3", "GDS_GWS_VMID4", "GDS_GWS_VMID5", "GDS_GWS_VMID6", "GDS_GWS_VMID7",
            "GDS_GWS_VMID8", "GDS_GWS_VMID9", "GDS_GWS_VMID10", "GDS_GWS_VMID11", "GDS_GWS_VMID12", "GDS_GWS_VMID13", "GDS_GWS_VMID14", "GDS_GWS_VMID15",
        ];
        const GDS_OA: [&str; 16] = [
            "GDS_OA_VMID0", "GDS_OA_VMID1", "GDS_OA_VMID2", "GDS_OA_VMID3", "GDS_OA_VMID4", "GDS_OA_VMID5", "GDS_OA_VMID6", "GDS_OA_VMID7",
            "GDS_OA_VMID8", "GDS_OA_VMID9", "GDS_OA_VMID10", "GDS_OA_VMID11", "GDS_OA_VMID12", "GDS_OA_VMID13", "GDS_OA_VMID14", "GDS_OA_VMID15",
        ];
        let gds = |t: &[&'static str; 16], i: u8| t.get(usize::from(i)).copied().unwrap_or("GDS_?");
        match self {
            R::CgttGsNggClkCtrl => "CGTT_GS_NGG_CLK_CTRL",
            R::CgttWdClkCtrl => "CGTT_WD_CLK_CTRL",
            R::CpfGcrCntl => "CPF_GCR_CNTL",
            R::Gl2cCtrl3 => "GL2C_CTRL3",
            R::PaClEnhance => "PA_CL_ENHANCE",
            R::PaScEnhance3 => "PA_SC_ENHANCE_3",
            R::PaScVrsSurfaceCntl1 => "PA_SC_VRS_SURFACE_CNTL_1",
            R::TaCntlAux => "TA_CNTL_AUX",
            R::TcpCntl2 => "TCP_CNTL2",
            R::TcpCntl => "TCP_CNTL",
            R::GrbmCntl => "GRBM_CNTL",
            R::GrbmGfxIndex => "GRBM_GFX_INDEX",
            R::GrbmGfxCntl => "GRBM_GFX_CNTL",
            R::ShMemConfig => "SH_MEM_CONFIG",
            R::ShMemBases => "SH_MEM_BASES",
            R::SpiGdbgPerVmidCntl => "SPI_GDBG_PER_VMID_CNTL",
            R::GdsBase(i) => gds(&GDS_BASE, i),
            R::GdsSize(i) => gds(&GDS_SIZE, i),
            R::GdsGws(i) => gds(&GDS_GWS, i),
            R::GdsOa(i) => gds(&GDS_OA, i),
            R::CcGcSaUnitDisable => "CC_GC_SA_UNIT_DISABLE",
            R::GcUserSaUnitDisable => "GC_USER_SA_UNIT_DISABLE",
            R::CcRbBackendDisable => "CC_RB_BACKEND_DISABLE",
            R::GcUserRbBackendDisable => "GC_USER_RB_BACKEND_DISABLE",
            R::CcGcShaderArrayConfig => "CC_GC_SHADER_ARRAY_CONFIG",
            R::GcUserShaderArrayConfig => "GC_USER_SHADER_ARRAY_CONFIG",
            R::CgtsTccDisable => "CGTS_TCC_DISABLE",
            R::CgtsUserTccDisable => "CGTS_USER_TCC_DISABLE",
            R::TaCntl2 => "TA_CNTL2",
            R::RlcCsibAddrHi => "RLC_CSIB_ADDR_HI",
            R::RlcCsibAddrLo => "RLC_CSIB_ADDR_LO",
            R::RlcCsibLength => "RLC_CSIB_LENGTH",
            R::RlcSrmCntl => "RLC_SRM_CNTL",
            R::CpRbDoorbellRangeLower => "CP_RB_DOORBELL_RANGE_LOWER",
            R::CpRbDoorbellRangeUpper => "CP_RB_DOORBELL_RANGE_UPPER",
            R::CpMecDoorbellRangeLower => "CP_MEC_DOORBELL_RANGE_LOWER",
            R::CpMecDoorbellRangeUpper => "CP_MEC_DOORBELL_RANGE_UPPER",
            R::CpMecCntl => "CP_MEC_CNTL",
            R::CpMeCntl => "CP_ME_CNTL",
            R::CpMesCntl => "CP_MES_CNTL",
            R::CpStat => "CP_STAT",
            R::GrbmStatus => "GRBM_STATUS",
            R::CpMaxContext => "CP_MAX_CONTEXT",
            R::CpDeviceId => "CP_DEVICE_ID",
            R::ScratchReg0 => "SCRATCH_REG0",
            R::CpRbWptrDelay => "CP_RB_WPTR_DELAY",
            R::CpRbVmid => "CP_RB_VMID",
            R::CpRb0Cntl => "CP_RB0_CNTL",
            R::CpRb0Wptr => "CP_RB0_WPTR",
            R::CpRb0WptrHi => "CP_RB0_WPTR_HI",
            R::CpRb0Rptr => "CP_RB0_RPTR",
            R::CpRbRptrWr => "CP_RB_RPTR_WR",
            R::CpRb0RptrAddr => "CP_RB0_RPTR_ADDR",
            R::CpRb0RptrAddrHi => "CP_RB0_RPTR_ADDR_HI",
            R::CpRbWptrPollAddrLo => "CP_RB_WPTR_POLL_ADDR_LO",
            R::CpRbWptrPollAddrHi => "CP_RB_WPTR_POLL_ADDR_HI",
            R::CpRb0Base => "CP_RB0_BASE",
            R::CpRb0BaseHi => "CP_RB0_BASE_HI",
            R::CpRbActive => "CP_RB_ACTIVE",
            R::CpRbDoorbellControl => "CP_RB_DOORBELL_CONTROL",
            R::CpGfxMqdBaseAddr => "CP_GFX_MQD_BASE_ADDR",
            R::CpGfxMqdBaseAddrHi => "CP_GFX_MQD_BASE_ADDR_HI",
            R::CpGfxMqdControl => "CP_GFX_MQD_CONTROL",
        }
    }
}

/// GX-7j3: CP_GFX_MQD_CONTROL as gfx_v11_0_gfx_mqd_init sets it from the value read: VMID 0, PRIV_STATE 1,
/// CACHE_POLICY 0 (gc_11_0_0_sh_mask.h: VMID 3:0, PRIV_STATE 8, CACHE_POLICY 25:24).
pub fn gfx_mqd_control(v: u32) -> u32 {
    (v & !(0xf | 0x0300_0000)) | 1 << 8
}

/// Field masks (gc_11_0_0_sh_mask.h).
pub mod f {
    pub const CE_HALT: u32 = 1 << 24;
    pub const PFP_HALT: u32 = 1 << 26;
    pub const ME_HALT: u32 = 1 << 28;
    /// What `cp_gfx_enable(false)` sets (PFP and ME).
    pub const GFX_HALTS: u32 = PFP_HALT | ME_HALT;
    /// The stop's and the Quiesce hook's halt: CE, PFP and ME (bits 24/26/28).
    pub const ALL_HALTS: u32 = CE_HALT | PFP_HALT | ME_HALT;
    pub const MES_HALT: u32 = 1 << 30;
    /// CP_MEC_CNTL MEC_ME1_HALT (bit 30): the cold value 0x40000000 is MEC ME1 halted, ME2 not.
    pub const MEC_ME1_HALT: u32 = 1 << 30;
    pub const RB_BUFSZ_MASK: u32 = 0x3f;
    pub const RB_BLKSZ_MASK: u32 = 0x3f00;
    pub const RB_NO_UPDATE: u32 = 1 << 27;
    pub const RB_RPTR_WR_ENA: u32 = 1 << 31;
    pub const RPTR_ADDR_HI_MASK: u32 = 0xffff;
    pub const WPTR_POLL_ADDR_HI_MASK: u32 = 0xffff;
    pub const RB_ACTIVE: u32 = 1;
    pub const DOORBELL_OFFSET_MASK: u32 = 0x0fff_fffc;
    pub const DOORBELL_EN: u32 = 1 << 30;
    pub const DOORBELL_HIT: u32 = 1 << 31;
    pub const DOORBELL_RANGE_MASK: u32 = 0xffc;
    pub const READ_TIMEOUT_MASK: u32 = 0xff;
    pub const SA_INDEX_SHIFT: u32 = 8;
    pub const SE_INDEX_SHIFT: u32 = 16;
    pub const SA_BROADCAST: u32 = 1 << 29;
    pub const INSTANCE_BROADCAST: u32 = 1 << 30;
    pub const SE_BROADCAST: u32 = 1 << 31;
    pub const PIPEID_MASK: u32 = 0x3;
    pub const VMID_SHIFT: u32 = 4;
    pub const TRAP_EN: u32 = 1 << 3;
    pub const SRM_ENABLE: u32 = 1;
    pub const SRM_AUTO_INCR_ADDR: u32 = 1 << 1;
    pub const CP_BUSY: u32 = 1 << 31;
    /// CC_GC_SA_UNIT_DISABLE / GC_USER_SA_UNIT_DISABLE: SA_DISABLE, bits 23:8.
    pub const SA_DISABLE_SHIFT: u32 = 8;
    pub const SA_DISABLE_MASK: u32 = 0x00ff_ff00;
}

/// One golden register: Linux's (and-mask, or-mask), applied as `soc15_program_register_sequence` does
/// (soc15.c:446-466): read, clear the and-mask, set the or-mask's bits inside it (an and-mask of all ones writes
/// the or-mask without a read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Golden {
    pub reg: R,
    pub and_mask: u32,
    pub or_mask: u32,
}

/// GC 11.0.1's golden settings (gfx_v11_0.c:86-97), with TCP_CNTL bit 29 as Linux 7.1 sets it on the 780M (the
/// capture read 0x039c0020 and wrote 0x239c0020 right after TCP_CNTL2: the 6.6 list ends before it).
pub const GOLDEN: [Golden; 10] = [
    Golden { reg: R::CgttGsNggClkCtrl, and_mask: 0x9fff_8fff, or_mask: 0x0000_0010 },
    Golden { reg: R::CgttWdClkCtrl, and_mask: 0xffff_8fff, or_mask: 0x0000_0010 },
    Golden { reg: R::CpfGcrCntl, and_mask: 0x0007_ffff, or_mask: 0x0000_c200 },
    Golden { reg: R::Gl2cCtrl3, and_mask: 0xffff_001b, or_mask: 0x00f0_1988 },
    Golden { reg: R::PaClEnhance, and_mask: 0xf0ff_ffff, or_mask: 0x0088_0007 },
    Golden { reg: R::PaScEnhance3, and_mask: 0xffff_fffd, or_mask: 0x0000_0008 },
    Golden { reg: R::PaScVrsSurfaceCntl1, and_mask: 0xfff8_91ff, or_mask: 0x5548_0100 },
    Golden { reg: R::TaCntlAux, and_mask: 0xf7f7_ffff, or_mask: 0x0103_0000 },
    Golden { reg: R::TcpCntl2, and_mask: 0xfcff_ffff, or_mask: 0x0000_000a },
    Golden { reg: R::TcpCntl, and_mask: 0x2000_0000, or_mask: 0x2000_0000 },
];

impl Golden {
    pub fn needs_read(&self) -> bool {
        self.and_mask != u32::MAX
    }
    pub fn apply(&self, found: u32) -> u32 {
        if self.needs_read() { (found & !self.and_mask) | (self.or_mask & self.and_mask) } else { self.or_mask }
    }
}

/// DEFAULT_SH_MEM_CONFIG (gfx_v11_0.c:99-102): 64-bit address mode (0), unaligned (3 << 2), INITIAL_INST_PREFETCH
/// 3 (<< 14).
pub const SH_MEM_CONFIG: u32 = (3 << 2) | (3 << 14);
/// SH_MEM_BASES of the graphics VMIDs 1..8 (:1716-1720): the private aperture's and the shared aperture's start
/// >> 48 (GMC 11: 0x1000 and 0x2000).
pub const SH_MEM_BASES_GFX: u32 = (0x2000 << 16) | 0x1000;
/// SH_MEM_BASES of KFD's VMIDs (:1632: LDS_APP_BASE 1 shared, SCRATCH_APP_BASE 2 private).
pub const SH_MEM_BASES_KFD: u32 = (1 << 16) | 2;
/// GRBM_GFX_INDEX with every broadcast bit (gfx_v11_0_select_se_sh(0xffffffff, 0xffffffff, 0xffffffff)).
pub const GFX_INDEX_BROADCAST: u32 = f::SE_BROADCAST | f::SA_BROADCAST | f::INSTANCE_BROADCAST;

/// GRBM_GFX_INDEX selecting one SA of one SE, every instance (select_se_sh(se, sa, 0xffffffff)).
pub fn gfx_index(se: u32, sa: u32) -> u32 {
    ((se & 0xff) << f::SE_INDEX_SHIFT) | ((sa & 0xff) << f::SA_INDEX_SHIFT) | f::INSTANCE_BROADCAST
}

/// GRBM_GFX_CNTL selecting `vmid` (soc21_grbm_select(adev, 0, 0, 0, vmid)).
pub fn grbm_vmid(vmid: u8) -> u32 {
    (u32::from(vmid) & 0xf) << f::VMID_SHIFT
}

/// The context count Linux programs (gfx config max_hw_contexts 8 on GFX 11: CP_MAX_CONTEXT = 8 - 1).
pub const MAX_HW_CONTEXTS: u32 = 8;

/// Linux's GFX ring 0: 2048 dwords (the capture's debugfs ring dump is 8 KiB).
pub const RING_DW: u64 = 2048;
/// Linux's waits (adev->usec_timeout).
pub const LINUX_TIMEOUT_US: u64 = 100_000;
/// `cp_gfx_resume`'s mdelay(1) between the first and the second CP_RB0_CNTL write.
pub const RESUME_DELAY_US: u64 = 1000;

/// CP_RB0_CNTL as `cp_gfx_resume` writes it: RB_BUFSZ = log2(bytes / 8), RB_BLKSZ = RB_BUFSZ - 2, nothing else
/// (written from 0: gfx_v11_0.c:3249-3252).
pub fn rb0_cntl(ring_dw: u64) -> u32 {
    let bufsz = (ring_dw * 4 / 8).trailing_zeros();
    (bufsz & f::RB_BUFSZ_MASK) | ((bufsz.saturating_sub(2) << 8) & f::RB_BLKSZ_MASK)
}

/// CP_ME_CNTL with PFP and ME unhalted (`cp_gfx_enable(true)`, CE as found) or halted (`false`).
pub fn me_cntl(v: u32, run: bool) -> u32 {
    if run { v & !f::GFX_HALTS } else { v | f::GFX_HALTS }
}

/// CP_MEC_CNTL with MEC ME1 unhalted (`cp_compute_enable(true)` with enable_mes_kiq, linux-6.6
/// gfx_v11_0.c:3358-3364: ME2 left as found) or halted again (`false`). Only ME1: Linux's disable also sets
/// MEC_ME2_HALT, but the 780M's cold value has ME2 clear and our unhalt never clears it, so setting ME1 alone
/// puts back exactly what was found (0x40000000).
pub fn mec_cntl(v: u32, run: bool) -> u32 {
    if run { v & !f::MEC_ME1_HALT } else { v | f::MEC_ME1_HALT }
}

/// `cp_compute_enable`'s udelay(50) after the CP_MEC_CNTL write (gfx_v11_0.c:3373; the capture: 62 us between the
/// MEC and the ME unhalt).
pub const MEC_DELAY_US: u64 = 50;

/// GFX ring 0's doorbell in 64-bit units (AMDGPU_NAVI10_DOORBELL_GFX_RING0, soc21.c:491, amdgpu_doorbell.h:203).
pub const DOORBELL_INDEX64: u32 = 0x8b;
/// The same as a dword index (gfx_v11_0.c:910 `<< 1`): BAR2 byte 0x458, in BAR2's first page.
pub const DOORBELL_DW: u32 = DOORBELL_INDEX64 << 1;
/// The other ends of `cp_set_doorbell_range` (soc21.c:480-496): KIQ 0x0, USERQUEUE_END 0x8a, GFX_USERQUEUE_END
/// 0xff (64-bit units).
pub const DOORBELL_KIQ64: u32 = 0x0;
pub const DOORBELL_USERQUEUE_END64: u32 = 0x8a;
pub const DOORBELL_GFX_USERQUEUE_END64: u32 = 0xff;

/// `cp_set_doorbell_range` (:3572): (RB lower, RB upper, MEC lower, MEC upper); `(index64 * 2) << 2`.
pub fn doorbell_ranges() -> [(R, u32); 4] {
    let r = |i64: u32| (i64 * 2) << 2;
    [
        (R::CpRbDoorbellRangeLower, r(DOORBELL_INDEX64)),
        (R::CpRbDoorbellRangeUpper, r(DOORBELL_GFX_USERQUEUE_END64)),
        (R::CpMecDoorbellRangeLower, r(DOORBELL_KIQ64)),
        (R::CpMecDoorbellRangeUpper, r(DOORBELL_USERQUEUE_END64)),
    ]
}

/// CP_RB_DOORBELL_CONTROL as `cp_gfx_set_doorbell` writes it (read-modify-write, :3209-3220): the doorbell OFFSET
/// (the dword index) and EN, or EN cleared only.
pub fn doorbell_control(v: u32, on: bool) -> u32 {
    if on { (v & !f::DOORBELL_OFFSET_MASK) | ((DOORBELL_DW << 2) & f::DOORBELL_OFFSET_MASK) | f::DOORBELL_EN } else { v & !f::DOORBELL_EN }
}

/// `cp_gfx_set_doorbell`'s range (:3222-3227): LOWER = the ring's dword index in the field (bits 11:2), UPPER =
/// the field's mask.
pub fn set_doorbell_range() -> [(R, u32); 2] {
    [(R::CpRbDoorbellRangeLower, (DOORBELL_DW << 2) & f::DOORBELL_RANGE_MASK), (R::CpRbDoorbellRangeUpper, f::DOORBELL_RANGE_MASK)]
}

/// Linux's ring test (gfx_v11_0.c:313-350): SCRATCH_REG0 poisoned 0xCAFEDEAD, written 0xDEADBEEF by
/// SET_UCONFIG_REG from the ring.
pub const POISON: u32 = 0xCAFE_DEAD;
pub const RING_TEST: u32 = 0xDEAD_BEEF;
