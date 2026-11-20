//! SYS-A: the GPU's numbers from the render node (DRM_IOCTL_AMDGPU_INFO, linux-7.3.1 include/uapi/drm/amdgpu_drm.h)
//! where the drm card's sysfs files (K1) are missing: memory use and the sensors (load, temperature, power, clocks,
//! voltage). One open node while the program's sampler runs; a query the driver refuses is a None.

use std::os::fd::{AsRawFd, OwnedFd};

use crate::sys_snap::Gpu;

pub const NODE: &str = "/dev/dri/renderD128";
/// _IOW('d', DRM_COMMAND_BASE + DRM_AMDGPU_INFO, struct drm_amdgpu_info): 32 bytes.
const IOCTL_INFO: libc::c_ulong = 0x4020_6445;
const INFO_MEMORY: u32 = 0x19;
const INFO_SENSOR: u32 = 0x1d;
const SENSOR_SCLK: u32 = 1;
const SENSOR_MCLK: u32 = 2;
const SENSOR_TEMP: u32 = 3;
const SENSOR_LOAD: u32 = 4;
const SENSOR_POWER: u32 = 5;
const SENSOR_VDDGFX: u32 = 7;

#[repr(C)]
#[derive(Default)]
struct Info {
    return_pointer: u64,
    return_size: u32,
    query: u32,
    arg: [u32; 4],
}

pub struct Node(OwnedFd);

impl Node {
    pub fn open() -> Option<Node> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(NODE).ok()?;
        Some(Node(OwnedFd::from(f)))
    }

    fn query(&self, query: u32, arg: u32, out: &mut [u8]) -> bool {
        let mut i = Info { return_pointer: out.as_mut_ptr() as u64, return_size: out.len() as u32, query, arg: [arg, 0, 0, 0] };
        // SAFETY: the kernel writes at most `return_size` bytes to `out`, which lives across the call; `i` is the
        // 32-byte struct the request number names.
        unsafe { libc::ioctl(self.0.as_raw_fd(), IOCTL_INFO, &mut i as *mut Info) == 0 }
    }

    fn sensor(&self, s: u32) -> Option<u32> {
        let mut b = [0u8; 4];
        self.query(INFO_SENSOR, s, &mut b).then(|| u32::from_ne_bytes(b))
    }

    /// The GPU's numbers in the page's units.
    pub fn read(&self) -> Gpu {
        let mut m = [0u8; 96];
        // drm_amdgpu_memory_info: vram, cpu_accessible_vram, gtt, each {total, usable, usage, max_allocation}.
        let mem = self.query(INFO_MEMORY, 0, &mut m).then(|| {
            let q = |i: usize| u64::from_ne_bytes(m[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
            ((q(2), q(1)), (q(10), q(9)))
        });
        Gpu {
            busy: self.sensor(SENSOR_LOAD).map(f64::from),
            vram: mem.map(|m| m.0).filter(|v| v.1 > 0),
            gtt: mem.map(|m| m.1).filter(|v| v.1 > 0),
            temp: self.sensor(SENSOR_TEMP).map(|mc| f64::from(mc) / 1000.0),
            power: self.sensor(SENSOR_POWER).map(f64::from),
            sclk_mhz: self.sensor(SENSOR_SCLK).map(f64::from),
            mclk_mhz: self.sensor(SENSOR_MCLK).map(f64::from),
            volt: self.sensor(SENSOR_VDDGFX).map(|mv| f64::from(mv) / 1000.0),
            via: "amdgpu",
        }
    }
}

/// `sysfs`'s values first, the node's for the rest.
pub fn merge(sysfs: Gpu, node: Option<Gpu>) -> Gpu {
    let Some(n) = node else { return sysfs };
    Gpu {
        busy: sysfs.busy.or(n.busy),
        vram: sysfs.vram.or(n.vram),
        gtt: sysfs.gtt.or(n.gtt),
        temp: sysfs.temp.or(n.temp),
        power: sysfs.power.or(n.power),
        sclk_mhz: sysfs.sclk_mhz.or(n.sclk_mhz),
        mclk_mhz: sysfs.mclk_mhz.or(n.mclk_mhz),
        volt: sysfs.volt.or(n.volt),
        via: if sysfs.via.is_empty() { n.via } else { sysfs.via },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_number_is_amdgpu_info() {
        // _IOW: direction write (1 << 30), size 32 << 16, type 'd' << 8, nr 0x40 + 0x05.
        assert_eq!(IOCTL_INFO, (1 << 30) | (32 << 16) | ((b'd' as libc::c_ulong) << 8) | 0x45);
        assert_eq!(std::mem::size_of::<Info>(), 32);
    }

    #[test]
    fn sysfs_wins_and_the_node_fills_gaps() {
        let s = Gpu { busy: Some(31.0), via: "sysfs", ..Gpu::default() };
        let n = Gpu { busy: Some(5.0), temp: Some(48.0), via: "amdgpu", ..Gpu::default() };
        let g = merge(s, Some(n));
        assert_eq!((g.busy, g.temp, g.via), (Some(31.0), Some(48.0), "sysfs"));
        assert_eq!(merge(Gpu::default(), None), Gpu::default());
    }
}
