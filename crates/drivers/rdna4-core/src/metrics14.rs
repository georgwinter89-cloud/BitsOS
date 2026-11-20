//! Wave 44: the PMFW's metrics table as Linux reads a temperature from it (`smu_v14_0_2_read_sensor`,
//! `P:` pm/swsmu/smu14/smu_v14_0_2_ppt.c:801-880, over `smu_v14_0_2_get_smu_metrics_data` P:624-720): one
//! TransferTableSmu2Dram of table 5 (`TABLE_SMU_METRICS`, pm/swsmu/inc/pmfw_if/smu14_driver_if_v14_0.h:1867,
//! mapped by P:227-239) into the driver table (`smu_cmn_update_table`, pm/swsmu/smu_cmn.c:1109-1160), then
//! `sizeof(SmuMetricsExternal_t)` bytes copied out of it, and `AvgTemperature[TEMP_x] * 1000` (P:690-710).
//!
//! The offsets are the C compiler's for Linux's headers (`tests/pptable_offsets.rs` holds them so). Stricter
//! than Linux: a buffer shorter than the table is an error (Linux's memcpy cannot fail), and the parse says when
//! the three temperatures are all 0 (a table nothing wrote reads 0 degrees).

use crate::pptable14::{TEMP_EDGE, TEMP_HOTSPOT, TEMP_MEM};

/// `TABLE_SMU_METRICS`.
pub const TABLE_SMU_METRICS: u32 = 5;
/// TransferTableSmu2Dram's argument for it: `((argument & 0xFFFF) << 16) | table_id` with argument 0
/// (smu_cmn.c:1140, from `smu_cmn_get_metrics_table` :1209-1213). The trace's late init sends exactly this
/// (init.txt lines 9904 and 9954).
pub const TRANSFER_ARG: u32 = TABLE_SMU_METRICS;
/// `sizeof(SmuMetricsExternal_t)`: what Linux copies out of the driver table.
pub const METRICS_BYTES: usize = 412;
/// `sizeof(SmuMetrics_t)`, the part in front of the spare words.
pub const SMU_METRICS_BYTES: usize = 260;
/// `offsetof(SmuMetrics_t, MetricsCounter)`.
pub const METRICS_COUNTER: usize = 104;
/// `offsetof(SmuMetrics_t, AvgTemperature)`, an array of `TEMP_COUNT` u16 in degrees Celsius.
pub const AVG_TEMPERATURE: usize = 140;
pub const TEMP_COUNT: usize = 12;
/// `offsetof(SmuMetrics_t, AvgTemperatureFanIntake)`.
pub const AVG_TEMPERATURE_FAN_INTAKE: usize = 164;

/// Linux's millidegrees (`SMU_TEMPERATURE_UNITS_PER_CENTIGRADES`, pm/swsmu/inc/amdgpu_smu.h:37).
pub const MILLI: u32 = 1000;

/// The temperatures of one read, degrees Celsius (moving averages of the PMFW, not instant values).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Temps {
    pub edge: u16,
    pub hotspot: u16,
    pub mem: u16,
    /// Every entry of `AvgTemperature`, in `TEMP_e`'s order.
    pub all: [u16; TEMP_COUNT],
    pub counter: u32,
}

impl Temps {
    /// What `read_sensor` answers for AMDGPU_PP_SENSOR_HOTSPOT_TEMP.
    pub const fn hotspot_milli(&self) -> u32 {
        self.hotspot as u32 * MILLI
    }

    /// The three Linux's handler and hwmon read are all 0: a table nobody wrote, most likely.
    pub const fn all_zero(&self) -> bool {
        self.edge == 0 && self.hotspot == 0 && self.mem == 0
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

/// The documented fields out of the bytes Linux copies.
pub fn parse(b: &[u8]) -> Result<Temps, &'static str> {
    if b.len() < METRICS_BYTES {
        return Err("the metrics table is shorter than SmuMetricsExternal_t");
    }
    let mut all = [0u16; TEMP_COUNT];
    for (i, t) in all.iter_mut().enumerate() {
        *t = u16_at(b, AVG_TEMPERATURE + 2 * i);
    }
    let c = METRICS_COUNTER;
    Ok(Temps { edge: all[TEMP_EDGE], hotspot: all[TEMP_HOTSPOT], mem: all[TEMP_MEM], all, counter: u32::from_le_bytes([b[c], b[c + 1], b[c + 2], b[c + 3]]) })
}

/// A table as a PMFW would write it, for the fakes: the temperatures and the counter, zeros elsewhere.
pub fn synthetic(t: &Temps, out: &mut [u8]) {
    let mut all = t.all;
    (all[TEMP_EDGE], all[TEMP_HOTSPOT], all[TEMP_MEM]) = (t.edge, t.hotspot, t.mem);
    for (i, v) in all.iter().enumerate() {
        out[AVG_TEMPERATURE + 2 * i..AVG_TEMPERATURE + 2 * i + 2].copy_from_slice(&v.to_le_bytes());
    }
    out[METRICS_COUNTER..METRICS_COUNTER + 4].copy_from_slice(&t.counter.to_le_bytes());
}
