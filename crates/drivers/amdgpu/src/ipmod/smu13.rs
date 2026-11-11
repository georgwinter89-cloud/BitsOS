//! SMU 13.0.4 (MP1): Linux's `smu_v13_0` block with the 13.0.4 table (linux-6.6 pm/swsmu/smu13/smu_v13_0_4_ppt.c).
//! The APU's SMU firmware lives in the BIOS: there is no file to load (Linux's microcode init returns early for
//! APUs). The driver talks to it through a mailbox: message C2PMSG_66, argument C2PMSG_82, response C2PMSG_90
//! (smu_v13_0_4_ppt.c:1127-1134). Power-up of GFX through the IMU on APUs goes through it (GX-7).
//!
//! GX-7e: the message ids and answer codes (the real client `gpucore::smu` and the fake `smufake` share them),
//! the driver mailbox's addresses from the discovery table, and the SMN window FIRMWARE_FLAGS is read through.

use super::{Firmware, Module, Role, State, v};
use crate::gpucore::disc::Discovery;
use crate::gpucore::hwid;

pub static MODULE: Module = Module {
    name: "smu13_0_4",
    role: Role::Smu,
    versions: &[v(13, 0, 4)],
    linux: "smu_v13_0",
    linux_also: &[
        v(13, 0, 0),
        v(13, 0, 1),
        v(13, 0, 2),
        v(13, 0, 3),
        v(13, 0, 5),
        v(13, 0, 6),
        v(13, 0, 7),
        v(13, 0, 8),
        v(13, 0, 10),
        v(13, 0, 11),
    ],
    firmware: Firmware::None("APU: the SMU firmware is part of the BIOS"),
    state: State::Present("gpucore::smu, gpucore::gfxup (GX-7e: the driver mailbox, EnableGfxImu, mode 2), dpm (GX-9b)"),
};

/// (segment index, dword) in MP1's bases (mp_13_0_4_offset.h).
pub const MSG: (usize, u32) = (1, 0x282);
pub const ARG: (usize, u32) = (1, 0x292);
pub const RESP: (usize, u32) = (1, 0x29a);

/// GX-7b: the GFXOFF state without a message: SMUIO_GFX_MISC_CNTL bits 2:1 PWR_GFXOFF_STATUS, 0 in GFXOFF,
/// 1 leaving, 2 not in GFXOFF (GC powered), 3 entering (segment 0 of SMUIO). Linux defines and reads it for the
/// SMU 13.0.1/13.0.3 APUs (yellow_carp_ppt.c:45-48, :616-626); 6.6's 13.0.4 table has no reader, so on the
/// 780M it is unverified until the Mini-PC answers (gx-GX-7b.md).
pub const SMUIO_GFX_MISC_CNTL: (usize, u32) = (0, 0xc5);
/// The DCN's own mailbox to PMFW (C2PMSG_67/83/91 of MP1): read-only here, owned by the display (dmub).
pub const DCN_RESP: (usize, u32) = (1, 0x29b);

/// PPSMC message ids of PMFW 13.0.4 (smu_v13_0_4_ppsmc.h:54-94; the 13.0.4 table, smu_v13_0_4_ppt.c:86-121).
pub mod msg {
    pub const TEST: u32 = 0x01;
    pub const GET_PMFW_VERSION: u32 = 0x02;
    pub const GET_DRIVER_IF_VERSION: u32 = 0x03;
    pub const POWER_DOWN_VCN: u32 = 0x06;
    pub const POWER_UP_VCN: u32 = 0x07;
    pub const PREPARE_MP1_FOR_UNLOAD: u32 = 0x0C;
    pub const SET_DRIVER_DRAM_ADDR_HIGH: u32 = 0x0D;
    pub const SET_DRIVER_DRAM_ADDR_LOW: u32 = 0x0E;
    pub const TRANSFER_TABLE_SMU2DRAM: u32 = 0x0F;
    pub const TRANSFER_TABLE_DRAM2SMU: u32 = 0x10;
    pub const GFX_DEVICE_DRIVER_RESET: u32 = 0x11;
    pub const GET_ENABLED_SMU_FEATURES: u32 = 0x12;
    pub const ENABLE_GFX_IMU: u32 = 0x16;
    pub const ALLOW_GFXOFF: u32 = 0x19;
    pub const DISALLOW_GFXOFF: u32 = 0x1A;
    pub const SET_SOFT_MAX_GFXCLK: u32 = 0x1B;
    pub const SET_HARD_MIN_GFXCLK: u32 = 0x1C;
    pub const POWER_DOWN_JPEG: u32 = 0x21;
    pub const POWER_UP_JPEG: u32 = 0x22;
    pub const ALLOW_ZSTATES: u32 = 0x25;
    /// One past the highest id the 13.0.4 table knows (PPSMC_Message_Count).
    pub const COUNT: u32 = 0x26;
}

/// Answer codes in C2PMSG_90 (smu_cmn.c:81-87).
pub mod resp {
    pub const NONE: u32 = 0;
    pub const OK: u32 = 1;
    pub const FAILED: u32 = 0xFF;
    pub const UNKNOWN: u32 = 0xFE;
    pub const PREREQ: u32 = 0xFD;
    pub const BUSY: u32 = 0xFC;
}

/// FEATURE_GFX_DPM_BIT of PMFW 13.0.4's enabled mask (smu_v13_0_4_pmfw.h:55): PMFW runs GFXCLK by load.
pub const FEATURE_GFX_DPM_BIT: u32 = 20;

/// SMU13_0_4_DRIVER_IF_VERSION (smu13_driver_if_v13_0_4.h:30): the interface the message table is for.
pub const DRIVER_IF_VERSION: u32 = 8;
/// GfxDeviceDriverReset's argument for a mode-2 (GC only) reset (`SMU_RESET_MODE_2`, amdgpu_smu.h:414;
/// smu_v13_0_4_ppt.c:1079-1083). Mode 1 is the PSP's whole-chip reset and never sent (it kills the picture).
pub const RESET_MODE_2: u32 = 2;
/// MP1_Public | smnMP1_V13_0_4_FIRMWARE_FLAGS (smu_v13_0.h:33-38, smu_v13_0.c:241), read through the SMN window.
pub const SMN_FIRMWARE_FLAGS: u32 = 0x03b1_0028;
/// FIRMWARE_FLAGS bit 0 INTERRUPTS_ENABLED: PMFW runs (smu_v13_0_check_fw_status, smu_v13_0.c:232-251).
pub const FIRMWARE_FLAGS_UP: u32 = 0x1;
/// NBIO 7.7's PCIE_INDEX2/PCIE_DATA2 (nbio_7_7_0_offset.h:3370-3373, segment 0 of NBIF; nbio_v7_7.c:202-210):
/// the SMN window `amdgpu_device_indirect_rreg` uses.
pub const PCIE_INDEX2: (usize, u32) = (0, 0xe);
pub const PCIE_DATA2: (usize, u32) = (0, 0xf);

/// The driver's mailbox, the display's (never written) and the SMN window, as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addrs {
    pub msg: u32,
    pub arg: u32,
    pub resp: u32,
    /// The display's C2PMSG_67 and 91 (never written or read here; tests check they are not writable).
    pub dcn_msg: u32,
    pub dcn_resp: u32,
    pub smn_index: u32,
    pub smn_data: u32,
}

impl Addrs {
    /// From MP1 instance 0's segment 1 and NBIF's segment 0; `None` when one is missing or an address overflows.
    pub fn from_discovery(d: &Discovery) -> Option<Addrs> {
        let mp1 = |at: (usize, u32)| d.reg(hwid::MP1, 0, at.0, at.1)?.checked_mul(4);
        let nbio = |at: (usize, u32)| d.reg(hwid::NBIF, 0, at.0, at.1)?.checked_mul(4);
        Some(Addrs {
            msg: mp1(MSG)?,
            arg: mp1(ARG)?,
            resp: mp1(RESP)?,
            dcn_msg: mp1((1, 0x283))?,
            dcn_resp: mp1(DCN_RESP)?,
            smn_index: nbio(PCIE_INDEX2)?,
            smn_data: nbio(PCIE_DATA2)?,
        })
    }

    /// The driver mailbox's three registers: the only SMU registers the live owner writes (the display's
    /// 67/83/91 never; the SMN index only with FIRMWARE_FLAGS' address, `smn_index_ok`).
    pub fn writable(&self) -> [u32; 3] {
        [self.msg, self.arg, self.resp]
    }

    /// The one value the SMN index may be written with (a read of FIRMWARE_FLAGS, nothing else).
    pub fn smn_index_ok(&self, byte: u32, value: u32) -> bool {
        byte == self.smn_index && value == SMN_FIRMWARE_FLAGS
    }

    /// Every register the client reads (the display's mailbox is not among them).
    pub fn readable(&self) -> [u32; 5] {
        [self.msg, self.arg, self.resp, self.smn_index, self.smn_data]
    }
}

/// A message's name (the 13.0.4 table; `?` for one BitsOS has no name for).
pub fn msg_name(id: u32) -> &'static str {
    match id {
        msg::TEST => "TestMessage",
        msg::GET_PMFW_VERSION => "GetPmfwVersion",
        msg::GET_DRIVER_IF_VERSION => "GetDriverIfVersion",
        msg::POWER_DOWN_VCN => "PowerDownVcn",
        msg::POWER_UP_VCN => "PowerUpVcn",
        msg::PREPARE_MP1_FOR_UNLOAD => "PrepareMp1ForUnload",
        msg::SET_DRIVER_DRAM_ADDR_HIGH => "SetDriverDramAddrHigh",
        msg::SET_DRIVER_DRAM_ADDR_LOW => "SetDriverDramAddrLow",
        msg::TRANSFER_TABLE_SMU2DRAM => "TransferTableSmu2Dram",
        msg::TRANSFER_TABLE_DRAM2SMU => "TransferTableDram2Smu",
        msg::GFX_DEVICE_DRIVER_RESET => "GfxDeviceDriverReset",
        msg::GET_ENABLED_SMU_FEATURES => "GetEnabledSmuFeatures",
        msg::ENABLE_GFX_IMU => "EnableGfxImu",
        msg::ALLOW_GFXOFF => "AllowGfxOff",
        msg::DISALLOW_GFXOFF => "DisallowGfxOff",
        msg::SET_SOFT_MAX_GFXCLK => "SetSoftMaxGfxClk",
        msg::SET_HARD_MIN_GFXCLK => "SetHardMinGfxClk",
        msg::POWER_DOWN_JPEG => "PowerDownJpeg",
        msg::POWER_UP_JPEG => "PowerUpJpeg",
        msg::ALLOW_ZSTATES => "AllowZstates",
        _ => "?",
    }
}

/// An answer code's meaning.
pub fn resp_name(code: u32) -> &'static str {
    match code {
        resp::NONE => "none (busy)",
        resp::OK => "OK",
        resp::FAILED => "failed",
        resp::UNKNOWN => "unknown message",
        resp::PREREQ => "bad prerequisites",
        resp::BUSY => "busy with another message",
        _ => "undefined",
    }
}
