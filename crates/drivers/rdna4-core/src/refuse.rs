//! What must never reach this card's secure processor or SMU, by name, so a later register owner can gate on it. The
//! PSP can rewrite the card's SPI flash and a bad write there can leave a card dead (`plan/spec/gpu-rdna4.md` section 5); limit and voltage
//! messages wait until someone understands them (R5). Nothing here sends anything.
//!
//! The bootloader's two registers are refused here for every value; `bootseq::Gate` is the one thing that lets a
//! write to them through, and only Linux's nine loads of a cold start, each at its step.

use crate::regmap::{self, PSP_C2PMSG_35, PSP_C2PMSG_36, Reg};

/// A:`psp_v14_0.c`:54-56, the flash-writing mailbox commands, and the registers that path uses (:605-608, :651-665).
pub const SPI_UPDATE_COMMANDS: [(u32, &str); 3] =
    [(0x2, "SPI_UPDATE_ROM_IMAGE_ADDR_LO"), (0x3, "SPI_UPDATE_ROM_IMAGE_ADDR_HI"), (0x4, "SPI_UPDATE_FLASH_IMAGE")];

const fn psp(name: &'static str, offset: u32) -> Reg {
    Reg { name, hw_id: crate::ipfacts::hw::MP0, base_index: 0, offset }
}

/// Registers no driver of this card writes.
pub const NEVER_WRITTEN: [(Reg, &str); 3] = [
    (psp("MPASP_SMN_C2PMSG_115", 0xB3), "flash mailbox command"),
    (psp("MPASP_SMN_C2PMSG_116", 0xB4), "flash image address"),
    (psp("MPASP_SMN_C2PMSG_73", 0x89), "flash mailbox doorbell"),
];

/// The bootloader's mailbox: its command and the address of its buffer. Also the way a USB-PD image is loaded
/// (A:psp_v14_0.c:548-557, :586), which never happens here. Open only through `bootseq::Gate`.
pub const BOOTLOADER_MAILBOX: [(Reg, crate::bootseq::Mailbox); 2] =
    [(PSP_C2PMSG_35, crate::bootseq::Mailbox::Command), (PSP_C2PMSG_36, crate::bootseq::Mailbox::Address)];

/// Which of the bootloader's two registers `reg` is, if either.
pub fn bootloader_mailbox(reg: Reg) -> Option<crate::bootseq::Mailbox> {
    BOOTLOADER_MAILBOX.iter().find(|(r, _)| same(*r, reg)).map(|(_, m)| *m)
}

/// A:`amdgpu_psp.h`:96-108. Each one loads or replaces something below the secure OS. `bootseq::SEQUENCE` holds the
/// nine of them Linux sends this card, in its order; the other three are never sent.
pub const BOOTLOADER_COMMANDS: [(u32, &str); 12] = [
    (0x0001_0000, "LOAD_SYSDRV"),
    (0x0002_0000, "LOAD_SOSDRV"),
    (0x0008_0000, "LOAD_KEY_DATABASE"),
    (0x000B_0000, "LOAD_SOCDRV"),
    (0x000C_0000, "LOAD_DBGDRV, also LOAD_HADDRV"),
    (0x000D_0000, "LOAD_INTFDRV"),
    (0x000E_0000, "LOAD_RASDRV"),
    (0x000F_0000, "LOAD_IPKEYMGRDRV"),
    (0x0010_0000, "DRAM_LONG_TRAIN"),
    (0x0020_0000, "DRAM_SHORT_TRAIN"),
    (0x1000_0000, "LOAD_TOS_SPL_TABLE"),
    (0x2000_0000, "LOAD_SPDMDRV"),
];

/// A:`psp_gfx_if.h`:48, in the ring-control register: a reset of the whole card, and on this card the secure OS with
/// it.
pub const GFX_CTRL_MODE1_RESET: u32 = 0x0007_0000;

/// What may go to the SMU at any time: questions. P:38-148.
pub const SMU_QUESTIONS: [(u16, &str); 11] = [
    (0x01, "TestMessage"),
    (0x02, "GetSmuVersion"),
    (0x03, "GetDriverIfVersion"),
    (0x0C, "GetRunningSmuFeaturesLow"),
    (0x0D, "GetRunningSmuFeaturesHigh"),
    (0x1D, "GetMinDpmFreq"),
    (0x1E, "GetMaxDpmFreq"),
    (0x1F, "GetDpmFreqByIndex"),
    (0x27, "GetDcModeMaxDpmFreq"),
    (0x33, "GetPptLimit"),
    (0x54, "GetAllRunningSmuFeatures"),
];

/// The messages of Linux's start of the power controller that are no questions (the trace's section smu, each
/// reviewed in plan/spec/gpu-rdna4.md section 6). They pass only through `smugate::Gate`, lent to the stage that
/// starts the power controller: each at its step of `smuseq::Script`, with the argument Linux's source computes.
/// Without a gate they are refused like any message nobody reviewed.
pub const SMU_START: [(u16, &str); 13] = [
    (0x06, "EnableAllSmuFeatures"),
    (0x0E, "SetDriverDramAddrHigh"),
    (0x0F, "SetDriverDramAddrLow"),
    (0x10, "SetToolsDramAddrHigh"),
    (0x11, "SetToolsDramAddrLow"),
    (0x12, "TransferTableSmu2Dram"),
    (0x20, "OverridePcieParameters"),
    (0x2A, "PowerUpVcn"),
    (0x2B, "PowerDownVcn"),
    (0x2C, "PowerUpJpeg"),
    (0x2D, "PowerDownJpeg"),
    (0x36, "RunDcBtc"),
    (0x4D, "AllowIHHostInterrupt"),
];

/// Limits, voltage-related controls, resets and tables pushed into the SMU. Named so nobody adds them by accident; the
/// messages in no list are simply not reviewed yet, which `smu_verdict` treats as refused too. No gate opens one of
/// these (`smugate::Gate` asks this list first): a later wave that wants one changes this list with Georg's word.
pub const SMU_NEVER: [(u16, &str); 19] = [
    (0x07, "DisableAllSmuFeatures"),
    (0x13, "TransferTableDram2Smu"),
    (0x14, "UseDefaultPPTable"),
    (0x15, "EnterBaco"),
    (0x17, "ArmD3"),
    (0x19, "SetSoftMinByFreq"),
    (0x1A, "SetSoftMaxByFreq"),
    (0x1B, "SetHardMinByFreq"),
    (0x1C, "SetHardMaxByFreq"),
    (0x2E, "PrepareMp1ForUnload"),
    (0x32, "SetPptLimit"),
    (0x3A, "SetThrottlerMask"),
    (0x3C, "SetMGpuFanBoostLimitRpm"),
    (0x49, "TriggerVFFLR"),
    (0x4A, "SetNumBadMemoryPagesRetired"),
    (0x4B, "SetBadMemoryPagesRetiredFlagsPerChannel"),
    (0x4F, "Mode3Reset"),
    (0x53, "TransferTableDram2SmuWithAddr"),
    (0x58, "PreloadSwPstateForUclkOverDrive"),
];

/// P: `PPSMC_Message_Count`.
pub const SMU_MESSAGE_COUNT: u16 = 0x59;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed(&'static str),
    /// A message of Linux's start of the power controller: only through `smugate::Gate`.
    Start(&'static str),
    Never(&'static str),
    /// A real message nobody has reviewed yet.
    NotReviewed,
    /// Past the end of the table, not a message of this firmware's interface.
    Unknown,
}

pub fn smu_verdict(message: u16) -> Verdict {
    if let Some(&(_, name)) = SMU_QUESTIONS.iter().find(|(m, _)| *m == message) {
        return Verdict::Allowed(name);
    }
    if let Some(&(_, name)) = SMU_NEVER.iter().find(|(m, _)| *m == message) {
        return Verdict::Never(name);
    }
    if let Some(&(_, name)) = SMU_START.iter().find(|(m, _)| *m == message) {
        return Verdict::Start(name);
    }
    if message < SMU_MESSAGE_COUNT { Verdict::NotReviewed } else { Verdict::Unknown }
}

/// The register's reason for being on the list, if it is. Compares where the register is, not what it is called.
pub fn never_written(reg: Reg) -> Option<&'static str> {
    NEVER_WRITTEN
        .iter()
        .find(|(r, _)| r.hw_id == reg.hw_id && r.base_index == reg.base_index && r.offset == reg.offset)
        .map(|(_, why)| *why)
}

pub fn is_bootloader_command(value: u32) -> bool {
    BOOTLOADER_COMMANDS.iter().any(|(c, _)| *c == value)
}

pub fn is_spi_update(command: u32) -> bool {
    SPI_UPDATE_COMMANDS.iter().any(|(c, _)| *c == command)
}

/// A:psp_gfx_if.h:43-44: the only two values a bare-metal driver writes into the ring-control register C2PMSG_64.
pub const GFX_CTRL_INIT_GPCOM_RING: u32 = 0x0002_0000;
pub const GFX_CTRL_DESTROY_RINGS: u32 = 0x0003_0000;

fn same(a: Reg, b: Reg) -> bool {
    a.hw_id == b.hw_id && a.base_index == b.base_index && a.offset == b.offset
}

/// Why a write of `value` to `reg` is refused whatever a stage allows, or `None`. By construction: the flash
/// registers, the bootloader's mailbox (which only `bootseq::Gate` opens, `write_refused_at`), the SR-IOV ring, every
/// ring control but create and destroy (Mode1 reset among them), the SMU's debug mailbox, and every SMU message
/// that is not a reviewed question (the start's messages open only through `smugate::Gate`, `write_refused_with`;
/// the thermal handler's one metrics transfer only through `thmgate::Gate`, wave 44).
pub fn write_refused(reg: Reg, value: u32) -> Option<&'static str> {
    if let Some(why) = never_written(reg) {
        return Some(why);
    }
    if bootloader_mailbox(reg).is_some() {
        return Some("the bootloader's mailbox opens only for Linux's nine loads, step by step");
    }
    if [regmap::PSP_C2PMSG_101, regmap::PSP_C2PMSG_102, regmap::PSP_C2PMSG_103].iter().any(|r| same(*r, reg)) {
        return Some("the SR-IOV ring is a virtual function's");
    }
    if [regmap::SMU_DEBUG_ARGUMENT, regmap::SMU_DEBUG_ANSWER, regmap::SMU_DEBUG_MESSAGE].iter().any(|r| same(*r, reg)) {
        return Some("the SMU's debug mailbox (Mode1Reset goes there)");
    }
    if same(reg, regmap::PSP_C2PMSG_64) {
        return match value {
            GFX_CTRL_INIT_GPCOM_RING | GFX_CTRL_DESTROY_RINGS => None,
            GFX_CTRL_MODE1_RESET => Some("a Mode1 reset of the whole card"),
            _ => Some("not a ring control a bare-metal driver sends"),
        };
    }
    if same(reg, regmap::SMU_MESSAGE) {
        return match u16::try_from(value).map(smu_verdict) {
            Ok(Verdict::Allowed(_)) => None,
            Ok(Verdict::Never(_)) => Some("an SMU message on the never list"),
            Ok(Verdict::Start(_)) => Some("an SMU message of Linux's start: only the stage that starts the power controller sends it, at its step"),
            _ => Some("an SMU message nobody reviewed"),
        };
    }
    None
}

/// `write_refused` for a card whose bootloader state is known: the bootloader's two registers pass exactly when
/// `gate` says the write is Linux's next step; with no gate they stay shut. Everything else as `write_refused`.
pub fn write_refused_at(reg: Reg, value: u32, gate: Option<&crate::bootseq::Gate>) -> Option<&'static str> {
    match (bootloader_mailbox(reg), gate) {
        (Some(to), Some(g)) => g.refused(to, value),
        _ => write_refused(reg, value),
    }
}

/// `write_refused_at` for a card whose power controller is being started: the SMU's mailbox and the thermal
/// alert's registers pass exactly when `smu` says the write is Linux's next bus access; with no such gate the
/// mailbox takes questions only. Everything else as `write_refused_at`.
pub fn write_refused_with(reg: Reg, value: u32, boot: Option<&crate::bootseq::Gate>, smu: Option<&crate::smugate::Gate>) -> Option<&'static str> {
    use crate::smugate::Judged;
    match smu.map(|g| g.judge(reg, value)) {
        Some(Judged::Pass) => None,
        Some(Judged::Refused(why)) => Some(why),
        Some(Judged::NotMine) | None => write_refused_at(reg, value, boot),
    }
}

/// A:psp_gfx_if.h:87-112: the ring commands Linux sends this secure OS during its init on 14.0.3 (the 31 frames of
/// the trace: the two reserved-region questions, the TOC, the IP firmware, the autoload, the ASD and the three
/// trusted applications), the only ones a ring here may carry. Not BOOT_CFG 0x22 (it persists a boot configuration;
/// Linux does not send it on this card), SRIOV_SPATIAL_PART 0x27, FB_NPS_MODE 0x48, UNLOAD_TA 0x2, INVOKE_CMD 0x3,
/// SETUP_TMR 0x5 or DESTROY_TMR 0x7 (14.0.3 keeps its boot-time TMR), nor anything else.
pub const RING_COMMANDS: [(u32, &str); 7] = [
    (0x01, "LOAD_TA"),
    (0x04, "LOAD_ASD"),
    (0x06, "LOAD_IP_FW"),
    (0x20, "LOAD_TOC"),
    (0x21, "AUTOLOAD_RLC"),
    (0x50, "FB_FW_RESERV_ADDR"),
    (0x51, "FB_FW_RESERV_EXT_ADDR"),
];

/// A:psp_gfx_if.h:209-301 (`enum psp_gfx_fw_type`): the images LOAD_IP_FW may name, those Linux loads on this card:
/// GC 12, SDMA 7, MES, the SMU, VCN and DMUB (`psp_get_fw_type`, amdgpu_psp.c:3105-3260) and the secure OS file's
/// register list (`psp_rl_load`, amdgpu_psp.c:1177-1205). Never DMCU 11/12 or a type this card's files do not hold.
pub const IP_FW_TYPES: [(u32, &str); 24] = [
    (8, "RLC_G"),
    (13, "VCN"),
    (18, "SMU"),
    (20, "RLC_RESTORE_LIST_GPM_MEM"),
    (21, "RLC_RESTORE_LIST_SRM_MEM"),
    (22, "RLC_RESTORE_LIST_SRM_CNTL"),
    (26, "RLC_IRAM"),
    (33, "CP_MES"),
    (34, "MES_STACK"),
    (48, "RLC_DRAM_BOOT"),
    (51, "DMUB"),
    (67, "REG_LIST"),
    (68, "IMU_I"),
    (69, "IMU_D"),
    (71, "SDMA_UCODE_TH0"),
    (81, "CP_MES_KIQ"),
    (82, "MES_KIQ_STACK"),
    (87, "RS64_PFP"),
    (88, "RS64_ME"),
    (89, "RS64_MEC"),
    (90, "RS64_PFP_P0_STACK"),
    (92, "RS64_ME_P0_STACK"),
    (94, "RS64_MEC_P0_STACK"),
    (95, "RS64_MEC_P1_STACK"),
];

/// Why a ring command with these words (the command union's first dwords) is refused, or `None`. LOAD_IP_FW names its
/// image's type in its fourth word (`psp_gfx_cmd_load_ip_fw.fw_type`, psp_gfx_if.h:315-322).
pub fn ring_command_refused(id: u32, words: &[u32]) -> Option<&'static str> {
    if !RING_COMMANDS.iter().any(|(c, _)| *c == id) {
        return Some("not a ring command Linux sends this secure OS here");
    }
    if id == 0x06 {
        return match words.get(3) {
            Some(t) if IP_FW_TYPES.iter().any(|(x, _)| x == t) => None,
            _ => Some("LOAD_IP_FW of an image type this card's list does not have"),
        };
    }
    None
}
