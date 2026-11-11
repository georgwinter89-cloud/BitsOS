//! GX-9b: PMFW 13.0.4's two driver tables as bytes (smu13_driver_if_v13_0_4.h): DpmClocks_t for the GFX clock's range
//! and SmuMetrics_t for what `gpu` and SENSOR report. Offsets checked with offsetof against that header.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use drm::gpu::Sensor;

/// TABLE_DPMCLOCKS and TABLE_SMU_METRICS (smu13_driver_if_v13_0_4.h:274, :277).
pub const TABLE_DPMCLOCKS: u32 = 4;
pub const TABLE_SMU_METRICS: u32 = 7;
/// sizeof(DpmClocks_t), and MinGfxClk and MaxGfxClk in it; the display's clock levels before them are never read.
pub const DPMCLOCKS_BYTES: usize = 304;
pub const MIN_GFX_AT: usize = 296;
pub const MAX_GFX_AT: usize = 300;
/// sizeof(SmuMetrics_t).
pub const METRICS_BYTES: usize = 160;

fn u16_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 2)?;
    Some(u32::from(u16::from_le_bytes([s[0], s[1]])))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// MinGfxClk and MaxGfxClk (MHz): Linux's AUTO limits (smu_v13_0_4_get_dpm_ultimate_freq, smu_v13_0_4_ppt.c:782, :813).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxRange {
    pub min: u32,
    pub max: u32,
}

/// `None`: a table no message can follow (nothing written by PMFW, or a range that names no frequency).
pub fn gfx_range(b: &[u8]) -> Option<GfxRange> {
    let (min, max) = (u32_at(b, MIN_GFX_AT)?, u32_at(b, MAX_GFX_AT)?);
    (min != 0 && min <= max && max <= 0xffff).then_some(GfxRange { min, max })
}

/// The metrics a tool may want, in the table's own units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metrics {
    pub gfx_mhz: u32,
    pub soc_mhz: u32,
    pub mem_mhz: u32,
    pub gfx_activity_centi: u32,
    pub vdd_mv: u32,
    pub soc_mv: u32,
    pub gfx_temp_centi: u32,
    pub soc_temp_centi: u32,
    pub throttler: u32,
    pub socket_mw: u32,
    pub apu_mw: u32,
    pub avg_gfx_mhz: u32,
    pub avg_gfx_activity_centi: u32,
    pub avg_socket_mw: u32,
    pub counter: u32,
}

/// `None`: too short, or all zero: the wiped page came back as it went, PMFW wrote nothing there.
pub fn metrics(b: &[u8]) -> Option<Metrics> {
    let t = b.get(..METRICS_BYTES)?;
    if t.iter().all(|&x| x == 0) {
        return None;
    }
    Some(Metrics {
        gfx_mhz: u16_at(t, 0)?,
        soc_mhz: u16_at(t, 2)?,
        mem_mhz: u16_at(t, 8)?,
        gfx_activity_centi: u16_at(t, 12)?,
        vdd_mv: u16_at(t, 16)?,
        soc_mv: u16_at(t, 18)?,
        gfx_temp_centi: u16_at(t, 80)?,
        soc_temp_centi: u16_at(t, 82)?,
        throttler: u16_at(t, 84)?,
        socket_mw: u16_at(t, 86)?,
        apu_mw: u32_at(t, 92)?,
        avg_gfx_mhz: u16_at(t, 120)?,
        avg_gfx_activity_centi: u16_at(t, 124)?,
        avg_socket_mw: u16_at(t, 136)?,
        counter: u32_at(t, 156)?,
    })
}

impl Metrics {
    /// `AMDGPU_INFO_SENSOR` in Linux's user units (smu_v13_0_4_get_smu_metrics_data, smu_v13_0_4_ppt.c:288-370;
    /// amdgpu_kms.c:990-1047). Stable pstate clocks answer 0 as pm_info on the 780M, peak ones were never reported.
    pub fn sensor(&self, s: Sensor) -> Option<u32> {
        match s {
            Sensor::GfxSclk => Some(self.gfx_mhz),
            Sensor::GfxMclk => Some(self.mem_mhz),
            Sensor::GpuTemp => Some(self.gfx_temp_centi / 100 * 1000),
            Sensor::GpuLoad => Some(self.gfx_activity_centi / 100),
            Sensor::GpuAvgPower => Some(self.avg_socket_mw / 1000),
            Sensor::Vddnb => Some(self.soc_mv),
            Sensor::Vddgfx => Some(self.vdd_mv),
            Sensor::StablePstateSclk | Sensor::StablePstateMclk => Some(0),
            Sensor::PeakPstateSclk | Sensor::PeakPstateMclk => None,
        }
    }
}

const THROTTLERS: [&str; 13] = ["SPL", "FPPT", "SPPT", "SPPT_APU", "THM_CORE", "THM_GFX", "THM_SOC", "TDC_VDD", "TDC_SOC", "PROCHOT_CPU", "PROCHOT_GFX", "EDC_CPU", "EDC_GFX"];

/// The throttler bits by name (smu13_driver_if_v13_0_4.h:150-162).
pub fn throttler_names(bits: u32) -> String {
    let names: Vec<&str> = THROTTLERS.iter().enumerate().filter(|(i, _)| bits & (1 << i) != 0).map(|(_, n)| *n).collect();
    match (names.is_empty(), bits >> THROTTLERS.len()) {
        (true, 0) => String::from("none"),
        (_, 0) => names.join(" "),
        (_, rest) => format!("{} other {:#x}", names.join(" "), rest << THROTTLERS.len()),
    }
}

/// `x.yy` from hundredths.
pub fn centi(v: u32) -> String {
    format!("{}.{:02}", v / 100, v % 100)
}

/// `x.yyy` from thousandths.
pub fn milli(v: u32) -> String {
    format!("{}.{:03}", v / 1000, v % 1000)
}
