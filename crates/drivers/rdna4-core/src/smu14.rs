//! SMU 14.0.2, the power controller's firmware (PMFW) of this card, as Linux 7.3.1's headers name it: the messages
//! of its start (`P:` pm/swsmu/inc/pmfw_if/smu_v14_0_2_ppsmc.h), the answer codes and how Linux reads them
//! (`C:` pm/swsmu/smu_cmn.c), the feature bits and clocks Linux asks about (`D:`
//! pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0.h, `T:` pm/swsmu/smu14/smu_v14_0_2_ppt.c). Facts only; `smuseq` is
//! the order they go out in, `refuse` what never goes out.

/// P:39-148, the messages of Linux's start of the power controller (the trace's section smu).
pub mod msg {
    pub const GET_SMU_VERSION: u16 = 0x02;
    pub const GET_DRIVER_IF_VERSION: u16 = 0x03;
    pub const ENABLE_ALL_SMU_FEATURES: u16 = 0x06;
    pub const GET_RUNNING_SMU_FEATURES_LOW: u16 = 0x0C;
    pub const GET_RUNNING_SMU_FEATURES_HIGH: u16 = 0x0D;
    pub const SET_DRIVER_DRAM_ADDR_HIGH: u16 = 0x0E;
    pub const SET_DRIVER_DRAM_ADDR_LOW: u16 = 0x0F;
    pub const SET_TOOLS_DRAM_ADDR_HIGH: u16 = 0x10;
    pub const SET_TOOLS_DRAM_ADDR_LOW: u16 = 0x11;
    pub const TRANSFER_TABLE_SMU2DRAM: u16 = 0x12;
    pub const GET_DPM_FREQ_BY_INDEX: u16 = 0x1F;
    pub const OVERRIDE_PCIE_PARAMETERS: u16 = 0x20;
    pub const POWER_UP_VCN: u16 = 0x2A;
    pub const POWER_DOWN_VCN: u16 = 0x2B;
    pub const POWER_UP_JPEG: u16 = 0x2C;
    pub const POWER_DOWN_JPEG: u16 = 0x2D;
    pub const RUN_DC_BTC: u16 = 0x36;
    pub const ALLOW_IH_HOST_INTERRUPT: u16 = 0x4D;
}

/// The driver interface Linux 7.3.1 is built for (T:74). The firmware names its own with GetDriverIfVersion; Linux
/// prints both and goes on (C:1084-1106).
pub const DRIVER_IF: u32 = 0x2E;

/// `TABLE_COMBO_PPTABLE` (D:1863), the one table Linux has the PMFW write during its start: the argument of
/// TransferTableSmu2Dram is `(argument & 0xFFFF) << 16 | table id` with argument 0 (C:1136, C:1227-1236).
pub const TABLE_COMBO_PPTABLE: u32 = 1;
/// `MP0_MP1_DATA_REGION_SIZE_COMBOPPTABLE` (T:77): the largest table, so the size of the driver table Linux
/// allocates (`smu_init_fb_allocations`, pm/swsmu/amdgpu_smu.c:1005-1068).
pub const DRIVER_TABLE_BYTES: u64 = 0x4000;
/// `SMU14_TOOL_SIZE` (pm/swsmu/inc/smu_v14_0.h:43), the tools table (PMSTATUSLOG).
pub const TOOLS_TABLE_BYTES: u64 = 0x19000;

/// The reads after which Linux gives the answer register up: `ctl->default_timeout = adev->usec_timeout * 20`
/// (T:2154) rounds of one read and `udelay(1)` (`__smu_msg_v1_poll_stat`, C:294-309).
pub const ANSWER_READS: u32 = 2_000_000;

/// What the answer register holds (C:76-84; P:30-34 for the firmware's own names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resp {
    /// 0: still executing.
    None,
    Ok,
    /// 0xFF: the command ran and failed.
    Fail,
    /// 0xFE: a command the firmware does not know.
    Unknown,
    /// 0xFD: a valid command whose prerequisites do not hold.
    BadPrereq,
    /// 0xFC: busy with other commands.
    Busy,
    /// Anything else, 0xFB (the debug mailbox's end) and all ones among them: Linux takes the firmware for hung.
    Other(u32),
}

/// `smu_msg_v1_decode_response` (C:249-292).
pub const fn decode(answer: u32) -> Resp {
    match answer {
        0 => Resp::None,
        1 => Resp::Ok,
        0xFF => Resp::Fail,
        0xFE => Resp::Unknown,
        0xFD => Resp::BadPrereq,
        0xFC => Resp::Busy,
        v => Resp::Other(v),
    }
}

impl Resp {
    /// Linux's error number for it.
    pub const fn errno(self) -> &'static str {
        match self {
            Resp::None => "-ETIME",
            Resp::Ok => "0",
            Resp::Fail => "-EIO",
            Resp::Unknown => "-EOPNOTSUPP",
            Resp::BadPrereq => "-EINVAL",
            Resp::Busy => "-EBUSY",
            Resp::Other(_) => "-EREMOTEIO",
        }
    }

    /// After this answer Linux sends the firmware nothing more: `smc_fw_state = SMU_FW_HANG` (C:536-538, :503-507).
    pub const fn hung(self) -> bool {
        matches!(self, Resp::Other(_))
    }

    /// A pre-poll that found this goes on to send (C:514-522): everything but no answer and a hung firmware.
    pub const fn lets_send(self) -> bool {
        !matches!(self, Resp::None | Resp::Other(_))
    }
}

/// D:46-103, the feature bits Linux's start asks about.
pub mod feature {
    pub const DPM_GFXCLK: u8 = 1;
    pub const DPM_UCLK: u8 = 3;
    pub const DPM_FCLK: u8 = 4;
    pub const DPM_SOCCLK: u8 = 5;
    pub const DPM_LINK: u8 = 6;
    pub const DPM_DCN: u8 = 7;
    pub const MM_DPM: u8 = 20;
    pub const FAN_CONTROL: u8 = 29;
}

/// `smu_v14_0_2_dpm_features` (T:66-72): `smu_v14_0_2_is_dpm_running` wants all of them (T:598-608).
pub const DPM_FEATURES: u64 = 1 << feature::DPM_GFXCLK | 1 << feature::DPM_UCLK | 1 << feature::DPM_LINK | 1 << feature::DPM_SOCCLK | 1 << feature::DPM_FCLK;

/// A clock whose levels Linux reads at its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    pub name: &'static str,
    /// `PPCLK_e` (D:455-467), mapped by `smu_v14_0_2_clk_map` (T:152-162).
    pub ppclk: u8,
    /// The feature that must run for the PMFW to hold a table (`smu_v14_0_2_feature_mask_map`, T:164-226: the
    /// video clocks' DPM bits both map to MM_DPM).
    pub feature: u8,
    /// `smu_cmn_clk_dpm_is_enabled` asks the feature mask again before every question about this clock
    /// (C:774-810); for DCEFCLK its `default` branch answers yes without asking.
    pub asked_each_time: bool,
}

/// In the order `smu_v14_0_2_set_default_dpm_table` reads them (T:465-596).
pub const CLOCKS: [Clock; 7] = [
    Clock { name: "SOCCLK", ppclk: 1, feature: feature::DPM_SOCCLK, asked_each_time: true },
    Clock { name: "GFXCLK", ppclk: 0, feature: feature::DPM_GFXCLK, asked_each_time: true },
    Clock { name: "UCLK", ppclk: 2, feature: feature::DPM_UCLK, asked_each_time: true },
    Clock { name: "FCLK", ppclk: 3, feature: feature::DPM_FCLK, asked_each_time: true },
    Clock { name: "VCLK", ppclk: 5, feature: feature::MM_DPM, asked_each_time: true },
    Clock { name: "DCLK", ppclk: 4, feature: feature::MM_DPM, asked_each_time: true },
    Clock { name: "DCEFCLK", ppclk: 9, feature: feature::DPM_DCN, asked_each_time: false },
];
/// The index of GFXCLK in `CLOCKS`: its top level is capped (T:502-509).
pub const GFXCLK: usize = 1;

/// `SMU_MAX_DPM_LEVELS` (pm/swsmu/inc/amdgpu_smu.h:327): Linux's table has room for this many levels and does not
/// check the count the PMFW answers.
pub const MAX_DPM_LEVELS: u32 = 16;
/// The level index that asks for the number of levels, with the fine-grained flag in bit 31 of the answer
/// (`smu_v14_0_get_dpm_level_count`, `smu_v14_0_get_fine_grained_status`, pm/swsmu/smu14/smu_v14_0.c:1324-1373).
pub const LEVEL_COUNT: u32 = 0xFF;
pub const FINE_GRAINED: u32 = 0x8000_0000;

/// GetDpmFreqByIndex's argument (smu_v14_0.c:1312).
pub const fn freq_arg(ppclk: u8, level: u32) -> u32 {
    (ppclk as u32) << 16 | (level & 0xFFFF)
}

/// A clock's levels as the PMFW answered them, in MHz, lowest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpmTable {
    pub clock: &'static str,
    pub ppclk: u8,
    /// `SMU_DPM_TABLE_FINE_GRAINED`: the PMFW takes any frequency between the first and the last level.
    pub fine_grained: bool,
    pub mhz: alloc::vec::Vec<u32>,
    /// The clock's feature did not run, so the PMFW was not asked: Linux then keeps one level, the VBIOS's boot
    /// value (T:480-484), which BitsOS does not read.
    pub from_pmfw: bool,
}

/// `smu_v14_0_2_get_thermal_temperature_range` (T:1498-1530) in millidegrees, from `smu14_thermal_policy[0]`
/// (T:1493) and the combo table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThermalRange {
    pub min: i32,
    pub max: i32,
    pub edge_emergency_max: i32,
    pub hotspot_crit_max: i32,
    pub hotspot_emergency_max: i32,
    pub mem_crit_max: i32,
    pub mem_emergency_max: i32,
    /// In degrees, as the table holds it.
    pub software_shutdown_temp: u16,
    pub software_shutdown_temp_offset: i16,
}

/// `smu14_thermal_policy[0].min` (T:1493).
pub const THERMAL_MIN_MILLI: i32 = -273_150;
/// `CTF_OFFSET_EDGE`, `_HOTSPOT`, `_MEM` (pm/swsmu/inc/smu_v14_0.h:45-47).
pub const CTF_OFFSET: i32 = 5;

impl ThermalRange {
    pub fn of(t: &crate::pptable14::Pptable) -> ThermalRange {
        let milli = |deg: u16, plus: i32| (i32::from(deg) + plus) * 1000;
        let [edge, hotspot, mem] = t.temperature_limit;
        ThermalRange {
            min: THERMAL_MIN_MILLI,
            max: milli(edge, 0),
            edge_emergency_max: milli(edge, CTF_OFFSET),
            hotspot_crit_max: milli(hotspot, 0),
            hotspot_emergency_max: milli(hotspot, CTF_OFFSET),
            mem_crit_max: milli(mem, 0),
            mem_emergency_max: milli(mem, CTF_OFFSET),
            software_shutdown_temp: t.software_shutdown_temp,
            software_shutdown_temp_offset: t.fan_abnormal_temp_limit_offset,
        }
    }
}
