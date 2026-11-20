//! SYS-K: one reading of a GPU's power-management sensors for sysfs and hwmon (`sysfs_pm`), in the units the
//! Linux files are made from: MHz, millidegrees, percent, mW, mV. A field the backend cannot give is None.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PmSample {
    /// Current GFX clock (`AMDGPU_PP_SENSOR_GFX_SCLK` / 100).
    pub gfx_mhz: Option<u32>,
    /// Edge temperature (`AMDGPU_PP_SENSOR_EDGE_TEMP`).
    pub edge_mc: Option<u32>,
    /// GFX busy (`AMDGPU_PP_SENSOR_GPU_LOAD`).
    pub busy_pct: Option<u32>,
    /// Average and current socket power (`GPU_AVG_POWER`, `GPU_INPUT_POWER`).
    pub avg_mw: Option<u32>,
    pub cur_mw: Option<u32>,
    pub vddgfx_mv: Option<u32>,
    pub vddnb_mv: Option<u32>,
}

/// A sample from the INFO sensors (whole watts): what a backend without its own reading answers.
pub fn from_sensors(read: impl Fn(crate::gpu::Sensor) -> Option<u32>) -> PmSample {
    use crate::gpu::Sensor;
    PmSample {
        gfx_mhz: read(Sensor::GfxSclk),
        edge_mc: read(Sensor::GpuTemp),
        busy_pct: read(Sensor::GpuLoad),
        avg_mw: read(Sensor::GpuAvgPower).map(|w| w * 1000),
        cur_mw: read(Sensor::GpuInputPower).map(|w| w * 1000),
        vddgfx_mv: read(Sensor::Vddgfx),
        vddnb_mv: read(Sensor::Vddnb),
    }
}
