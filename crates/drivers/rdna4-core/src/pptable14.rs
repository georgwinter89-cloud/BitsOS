//! The combo powerplay table the PMFW writes into the driver table on TransferTableSmu2Dram, read only by Linux
//! 7.3.1's documented headers: `struct smu_14_0_2_powerplay_table` (`H:` pm/swsmu/inc/smu_v14_0_2_pptable.h:140-170,
//! packed) with `PPTable_t` inside it (`D:` pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0.h:1594-1599, natural
//! alignment). Linux reads it by these compiled offsets (`smu_v14_0_2_store_powerplay_table`,
//! pm/swsmu/smu14/smu_v14_0_2_ppt.c:325-334), whatever the table's own header says of its parts, and so does this.
//!
//! Only what the start of the power controller uses is read, and what the waves right after it need:
//!
//! | field | used for |
//! |---|---|
//! | `SkuTable.PcieGenSpeed`, `PcieLaneCount`, `LclkFreq` | OverridePcieParameters' arguments (ppt.c:1411-1489) |
//! | `software_shutdown_temp` | the thermal alert's upper threshold (smu_v14_0.c:754-768) |
//! | `SkuTable.DriverReportedClocks.GameClockAc` | the cap of GFXCLK's top level (ppt.c:502-509), kept for the clocks' users |
//! | `CustomSkuTable.TemperatureLimit`, `FanAbnormalTempLimitOffset` | the thermal range (ppt.c:1498-1530), kept for the thermal interrupt's handler |
//! | `PFE_Settings.FeaturesToRun[0]` | whether the board has a fan the PMFW controls (ppt.c:319-320), said in the stage's answer |
//! | `CustomSkuTable.SocketPowerLimitAc[0]`, `SkuTable.MsgLimits.Power[0][AC]` | the default and the highest power limit (ppt.c:1664-1700), said in the stage's answer for the wave that decides on SetPptLimit |
//! | the header's revisions and its own part offsets | said in the stage's answer beside the compiled ones (Linux never reads them) |
//!
//! Nothing is written back. The offsets are held against the headers themselves by the host test
//! `tests/pptable_offsets.rs` (the C compiler's `offsetof` over Linux's files).

/// `sizeof(struct smu_14_0_2_powerplay_table)`.
pub const TABLE_BYTES: usize = 5812;
/// H:142, `struct atom_common_table_header` (include/atomfirmware.h:233-238).
pub const STRUCTURESIZE: usize = 0;
pub const FORMAT_REVISION: usize = 2;
pub const CONTENT_REVISION: usize = 3;
/// H:145-152: where the table itself says its parts lie (eight 16-bit fields).
pub const PMFW_PARTS: usize = 6;
pub const PLATFORM_CAPS: usize = 32;
pub const THERMAL_CONTROLLER_TYPE: usize = 36;
pub const SOFTWARE_SHUTDOWN_TEMP: usize = 43;
/// H:169, `smc_pptable`: on a 32-byte boundary, as the header's comment asks.
pub const SMC_PPTABLE: usize = 1344;
/// `sizeof(PPTable_t)` and its parts (D:1594-1599).
pub const PPTABLE_BYTES: usize = 4468;
pub const SKU_TABLE: usize = 28;
pub const SKU_TABLE_BYTES: usize = 3552;
pub const CUSTOM_SKU_TABLE: usize = 3580;
pub const CUSTOM_SKU_TABLE_BYTES: usize = 360;
pub const BOARD_TABLE: usize = 3940;
pub const BOARD_TABLE_BYTES: usize = 528;
/// D:1042, in `PFE_Settings_t`.
pub const FEATURES_TO_RUN: usize = 4;
/// D:1231-1233, in `SkuTable_t`.
pub const PCIE_GEN_SPEED: usize = 1108;
pub const PCIE_LANE_COUNT: usize = 1111;
pub const LCLK_FREQ: usize = 1114;
/// D:1003-1013 `DriverReportedClocks_t` in `SkuTable_t`, and `GameClockAc` in it.
pub const DRIVER_REPORTED_CLOCKS: usize = 2576;
pub const GAME_CLOCK_AC: usize = 2;
/// D:975, `MsgLimits_t.Power[PPT_THROTTLER_COUNT][POWER_SOURCE_COUNT]` at the start of `SkuTable_t.MsgLimits`.
pub const MSG_LIMITS: usize = 2604;
/// D:1522, D:1531, D:1565, in `CustomSkuTable_t`.
pub const SOCKET_POWER_LIMIT_AC: usize = 0;
pub const TEMPERATURE_LIMIT: usize = 20;
pub const FAN_ABNORMAL_TEMP_LIMIT_OFFSET: usize = 202;
/// `TEMP_e` (D:536-550).
pub const TEMP_EDGE: usize = 0;
pub const TEMP_HOTSPOT: usize = 1;
pub const TEMP_MEM: usize = 4;
/// `NUM_LINK_LEVELS` (D:41).
pub const LINK_LEVELS: usize = 3;
/// `SMU_14_0_2_TABLE_FORMAT_REVISION` (H:30).
pub const TABLE_FORMAT_REVISION: u8 = 23;
/// The highest values the header documents: `PcieGenSpeed` 0 (Gen1) to 4 (Gen5), `PcieLaneCount` 1 (x1) to 6 (x16)
/// (D:1231-1232).
pub const GEN_MAX: u8 = 4;
pub const LANE_MAX: u8 = 6;

/// The link levels of `SkuTable_t`, as stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PcieLevels {
    pub gen_speed: [u8; LINK_LEVELS],
    pub lane_count: [u8; LINK_LEVELS],
    pub lclk_freq: [u16; LINK_LEVELS],
}

/// Where the table's own header says its parts lie (H:145-152). Linux never reads these.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnParts {
    pub pptable_start: u16,
    pub pptable_size: u16,
    pub sku_table_start: u16,
    pub sku_table_size: u16,
    pub board_table_start: u16,
    pub board_table_size: u16,
    pub custom_sku_table_start: u16,
    pub custom_sku_table_size: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pptable {
    pub structuresize: u16,
    pub format_revision: u8,
    pub content_revision: u8,
    pub own: OwnParts,
    pub platform_caps: u32,
    pub thermal_controller_type: u8,
    /// Degrees Celsius.
    pub software_shutdown_temp: u16,
    pub features_to_run: [u32; 2],
    pub pcie: PcieLevels,
    /// MHz.
    pub game_clock_ac: u16,
    /// Edge, hotspot, memory; degrees Celsius.
    pub temperature_limit: [u16; 3],
    pub fan_abnormal_temp_limit_offset: i16,
    /// Watts.
    pub socket_power_limit_ac: u16,
    pub msg_limit_power_ac: u16,
}

/// Why the bytes are no table to compute a message's argument from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bad {
    /// Fewer bytes than the struct.
    Short { bytes: usize },
    /// `structuresize` 0: nothing was written there (Linux would go on and take every field as 0).
    Empty,
    /// A link level outside what the header documents (D:1231-1232).
    PcieGen { level: usize, value: u8 },
    PcieLane { level: usize, value: u8 },
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The table at the start of `bytes` (the driver table as the PMFW left it).
pub fn parse(bytes: &[u8]) -> Result<Pptable, Bad> {
    if bytes.len() < TABLE_BYTES {
        return Err(Bad::Short { bytes: bytes.len() });
    }
    let structuresize = u16_at(bytes, STRUCTURESIZE);
    if structuresize == 0 {
        return Err(Bad::Empty);
    }
    let part = |i: usize| u16_at(bytes, PMFW_PARTS + 2 * i);
    let (sku, custom) = (SMC_PPTABLE + SKU_TABLE, SMC_PPTABLE + CUSTOM_SKU_TABLE);
    let mut pcie = PcieLevels::default();
    for level in 0..LINK_LEVELS {
        pcie.gen_speed[level] = bytes[sku + PCIE_GEN_SPEED + level];
        pcie.lane_count[level] = bytes[sku + PCIE_LANE_COUNT + level];
        pcie.lclk_freq[level] = u16_at(bytes, sku + LCLK_FREQ + 2 * level);
        if pcie.gen_speed[level] > GEN_MAX {
            return Err(Bad::PcieGen { level, value: pcie.gen_speed[level] });
        }
        if pcie.lane_count[level] > LANE_MAX {
            return Err(Bad::PcieLane { level, value: pcie.lane_count[level] });
        }
    }
    let limit = |t: usize| u16_at(bytes, custom + TEMPERATURE_LIMIT + 2 * t);
    Ok(Pptable {
        structuresize,
        format_revision: bytes[FORMAT_REVISION],
        content_revision: bytes[CONTENT_REVISION],
        own: OwnParts {
            pptable_start: part(0),
            pptable_size: part(1),
            sku_table_start: part(2),
            sku_table_size: part(3),
            board_table_start: part(4),
            board_table_size: part(5),
            custom_sku_table_start: part(6),
            custom_sku_table_size: part(7),
        },
        platform_caps: u32_at(bytes, PLATFORM_CAPS),
        thermal_controller_type: bytes[THERMAL_CONTROLLER_TYPE],
        software_shutdown_temp: u16_at(bytes, SOFTWARE_SHUTDOWN_TEMP),
        features_to_run: [u32_at(bytes, SMC_PPTABLE + FEATURES_TO_RUN), u32_at(bytes, SMC_PPTABLE + FEATURES_TO_RUN + 4)],
        pcie,
        game_clock_ac: u16_at(bytes, sku + DRIVER_REPORTED_CLOCKS + GAME_CLOCK_AC),
        temperature_limit: [limit(TEMP_EDGE), limit(TEMP_HOTSPOT), limit(TEMP_MEM)],
        fan_abnormal_temp_limit_offset: u16_at(bytes, custom + FAN_ABNORMAL_TEMP_LIMIT_OFFSET) as i16,
        socket_power_limit_ac: u16_at(bytes, custom + SOCKET_POWER_LIMIT_AC),
        msg_limit_power_ac: u16_at(bytes, sku + MSG_LIMITS),
    })
}

impl Pptable {
    /// `pm.no_fan` (ppt.c:319-320): the table does not ask the PMFW to run its fan control.
    pub fn no_fan(&self) -> bool {
        self.features_to_run[0] & (1 << crate::smu14::feature::FAN_CONTROL) == 0
    }

    /// The parts as Linux's compiled header places them, in the form the table's own header would name them if
    /// its offsets count from the table's start and `pptable_start` is `smc_pptable`. Only for the answer line
    /// that shows both: what the fields mean is the header comments' word, nothing checks it.
    pub const COMPILED: OwnParts = OwnParts {
        pptable_start: SMC_PPTABLE as u16,
        pptable_size: PPTABLE_BYTES as u16,
        sku_table_start: (SMC_PPTABLE + SKU_TABLE) as u16,
        sku_table_size: SKU_TABLE_BYTES as u16,
        board_table_start: (SMC_PPTABLE + BOARD_TABLE) as u16,
        board_table_size: BOARD_TABLE_BYTES as u16,
        custom_sku_table_start: (SMC_PPTABLE + CUSTOM_SKU_TABLE) as u16,
        custom_sku_table_size: CUSTOM_SKU_TABLE_BYTES as u16,
    };
}

/// A table with these fields and zeros elsewhere, as bytes (a model's PMFW and tests write it; nothing of the
/// driver does).
pub fn synthetic(t: &Pptable, out: &mut [u8]) {
    let put16 = |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    let put32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let (sku, custom) = (SMC_PPTABLE + SKU_TABLE, SMC_PPTABLE + CUSTOM_SKU_TABLE);
    put16(out, STRUCTURESIZE, t.structuresize);
    out[FORMAT_REVISION] = t.format_revision;
    out[CONTENT_REVISION] = t.content_revision;
    let o = t.own;
    for (i, v) in [o.pptable_start, o.pptable_size, o.sku_table_start, o.sku_table_size, o.board_table_start, o.board_table_size, o.custom_sku_table_start, o.custom_sku_table_size].into_iter().enumerate() {
        put16(out, PMFW_PARTS + 2 * i, v);
    }
    put32(out, PLATFORM_CAPS, t.platform_caps);
    out[THERMAL_CONTROLLER_TYPE] = t.thermal_controller_type;
    put16(out, SOFTWARE_SHUTDOWN_TEMP, t.software_shutdown_temp);
    put32(out, SMC_PPTABLE + FEATURES_TO_RUN, t.features_to_run[0]);
    put32(out, SMC_PPTABLE + FEATURES_TO_RUN + 4, t.features_to_run[1]);
    for level in 0..LINK_LEVELS {
        out[sku + PCIE_GEN_SPEED + level] = t.pcie.gen_speed[level];
        out[sku + PCIE_LANE_COUNT + level] = t.pcie.lane_count[level];
        put16(out, sku + LCLK_FREQ + 2 * level, t.pcie.lclk_freq[level]);
    }
    put16(out, sku + DRIVER_REPORTED_CLOCKS + GAME_CLOCK_AC, t.game_clock_ac);
    for (i, at) in [TEMP_EDGE, TEMP_HOTSPOT, TEMP_MEM].into_iter().enumerate() {
        put16(out, custom + TEMPERATURE_LIMIT + 2 * at, t.temperature_limit[i]);
    }
    put16(out, custom + FAN_ABNORMAL_TEMP_LIMIT_OFFSET, t.fan_abnormal_temp_limit_offset as u16);
    put16(out, custom + SOCKET_POWER_LIMIT_AC, t.socket_power_limit_ac);
    put16(out, sku + MSG_LIMITS, t.msg_limit_power_ac);
}
