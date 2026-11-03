//! DR-4: `DRM_IOCTL_AMDGPU_INFO` (Linux `amdgpu_info_ioctl`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_kms.c:535)
//! against a `GpuDevice`. The answer goes to `return_pointer`, `min(return_size, sizeof answer)` bytes; the
//! 32-byte request itself is not written back. Unknown queries answer EINVAL like Linux's default case.

use crate::device::Device;
use crate::dispatch::Ctx;
use crate::gpu::{GpuDevice, MemoryState, Sensor, HW_IP_NUM};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::vec::Vec;
use linux::errno::*;
use vfs::Errno;

/// `AMDGPU_VM_RESERVED_VRAM` (amdgpu_vm.h:120): VRAM the VM code keeps for itself.
const VM_RESERVED_VRAM: u64 = 8 << 20;
/// `AMDGPU_GFX_MAX_SE`, `AMDGPU_GFX_MAX_SH_PER_SE` (amdgpu_gfx.h:167) and the READ_MMR_REG count bound (kms.c:745).
const MAX_SE: u32 = 4;
const MAX_SH_PER_SE: u32 = 2;
const MAX_MMR_COUNT: u32 = 128;
/// `amdgpu_ctx_num_entities` (amdgpu_ctx.c:35; VPE as newer kernels): rings a context can use per HW IP type.
pub const CTX_ENTITIES: [u32; HW_IP_NUM] = [1, 4, 2, 1, 1, 1, 1, 1, 1, 1];

/// Copies `answer` (its first `min(size, len)` bytes) to the caller.
fn out(ctx: &Ctx, ptr: u64, size: u32, answer: &[u8]) -> Result<i64, Errno> {
    let n = (size as usize).min(answer.len());
    if ctx.mem.write(ptr, &answer[..n]) { Ok(0) } else { Err(EFAULT) }
}

fn bytes<T: Wire>(v: &T) -> Vec<u8> {
    let mut b = alloc::vec![0u8; T::SIZE];
    let _ = v.encode(&mut b);
    b
}

pub fn ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let Some(info) = drm_amdgpu_info::decode(data) else { return Err(EINVAL) };
    let (ptr, size) = (info.return_pointer, info.return_size);
    if size == 0 || ptr == 0 {
        return Err(EINVAL);
    }
    let q = info.anon0;
    let u32_out = |v: u32| out(ctx, ptr, size, &v.to_le_bytes());
    let u64_out = |v: u64| out(ctx, ptr, size, &v.to_le_bytes());
    let p = gpu.profile();
    match info.query {
        AMDGPU_INFO_ACCEL_WORKING => u32_out(gpu.accel_working() as u32),
        // No KMS on the render-only device: no CRTC has the id.
        AMDGPU_INFO_CRTC_FROM_ID => Err(EINVAL),
        AMDGPU_INFO_HW_IP_INFO => {
            let ip = hw_ip_info(gpu, &q.query_hw_ip())?;
            out(ctx, ptr, size, &bytes(&ip))
        }
        AMDGPU_INFO_HW_IP_COUNT => {
            let ty = q.query_hw_ip().r#type;
            if ty as usize >= HW_IP_NUM {
                return Err(EINVAL);
            }
            // One instance per type at most (AMDGPU_HW_IP_INSTANCE_MAX_COUNT).
            u32_out(p.hw_ip(ty).is_some() as u32)
        }
        AMDGPU_INFO_TIMESTAMP => u64_out(gpu.gpu_clock_counter()),
        AMDGPU_INFO_FW_VERSION => {
            let f = q.query_fw();
            if f.ip_instance != 0 {
                return Err(EINVAL);
            }
            let fw = p.firmware(f.fw_type, f.index).ok_or(EINVAL)?;
            out(ctx, ptr, size, &bytes(&drm_amdgpu_info_firmware { ver: fw.ver, feature: fw.feature }))
        }
        AMDGPU_INFO_NUM_BYTES_MOVED => u64_out(gpu.memory().bytes_moved),
        AMDGPU_INFO_NUM_EVICTIONS => u64_out(gpu.memory().evictions),
        AMDGPU_INFO_NUM_VRAM_CPU_PAGE_FAULTS => u64_out(gpu.memory().vram_cpu_page_faults),
        AMDGPU_INFO_VRAM_USAGE => u64_out(gpu.memory().vram_usage),
        AMDGPU_INFO_VIS_VRAM_USAGE => u64_out(gpu.memory().vis_vram_usage),
        AMDGPU_INFO_GTT_USAGE => u64_out(gpu.memory().gtt_usage),
        AMDGPU_INFO_GDS_CONFIG => {
            let g = p.gds;
            let mut v = drm_amdgpu_info_gds::ZERO;
            v.compute_partition_size = g.gds_size;
            v.gds_total_size = g.gds_size;
            v.gws_per_compute_partition = g.gws_size;
            v.oa_per_compute_partition = g.oa_size;
            out(ctx, ptr, size, &bytes(&v))
        }
        AMDGPU_INFO_VRAM_GTT => out(ctx, ptr, size, &bytes(&vram_gtt(&gpu.memory()))),
        AMDGPU_INFO_MEMORY => out(ctx, ptr, size, &bytes(&memory_info(&gpu.memory()))),
        AMDGPU_INFO_READ_MMR_REG => read_mmr(ctx, gpu, &q.read_mmr_reg(), ptr, size),
        AMDGPU_INFO_DEV_INFO => out(ctx, ptr, size, &bytes(&gpu.dev_info())),
        // swsmu has no VCE clock states: an empty table.
        AMDGPU_INFO_VCE_CLOCK_TABLE => out(ctx, ptr, size, &bytes(&drm_amdgpu_info_vce_clock_table::ZERO)),
        AMDGPU_INFO_VBIOS => vbios(ctx, gpu, &q.vbios_info(), ptr, size),
        AMDGPU_INFO_NUM_HANDLES => match q.query_hw_ip().r#type {
            // Polaris and newer have no UVD handle limit (kms.c:968).
            AMDGPU_HW_IP_UVD => Err(ENODATA),
            _ => Err(EINVAL),
        },
        AMDGPU_INFO_SENSOR => {
            if !gpu.dpm_enabled() {
                return Err(ENOENT);
            }
            let s = sensor(q.sensor_info().r#type).ok_or(EINVAL)?;
            u32_out(gpu.sensor(s).ok_or(EINVAL)?)
        }
        AMDGPU_INFO_VRAM_LOST_COUNTER => u32_out(gpu.vram_lost_counter()),
        AMDGPU_INFO_RAS_ENABLED_FEATURES => u64_out(p.ras_features.ok_or(EINVAL)?),
        AMDGPU_INFO_VIDEO_CAPS => {
            let (dec, enc) = p.video.as_ref().ok_or(EINVAL)?;
            let table = match q.video_cap().r#type {
                AMDGPU_INFO_VIDEO_CAPS_DECODE => dec,
                AMDGPU_INFO_VIDEO_CAPS_ENCODE => enc,
                _ => return Err(EINVAL),
            };
            let mut caps = drm_amdgpu_info_video_caps::ZERO;
            for (c, slot) in table.iter().zip(caps.codec_info.iter_mut()) {
                if let Some(c) = c {
                    *slot = drm_amdgpu_info_video_codec_info {
                        valid: 1,
                        max_width: c.max_width,
                        max_height: c.max_height,
                        max_pixels_per_frame: c.max_pixels_per_frame,
                        max_level: c.max_level,
                        pad: 0,
                    };
                }
            }
            out(ctx, ptr, size, &bytes(&caps))
        }
        // DR-9: not in 6.6; CachyOS's kernel (and libdrm's header) have it: the last GPU page fault of the
        // file's VM, zero before any (there is always a VM, so never EINVAL for that).
        AMDGPU_INFO_GPUVM_FAULT => {
            let f = ctx.file.vm.backend_vm().and_then(|v| v.last_fault()).unwrap_or_default();
            out(ctx, ptr, size, &bytes(&drm_amdgpu_info_gpuvm_fault { addr: f.addr, status: f.status, vmhub: f.vmhub }))
        }
        AMDGPU_INFO_MAX_IBS => {
            let m = p.max_ibs.ok_or(EINVAL)?;
            out(ctx, ptr, size, &bytes(&m))
        }
        other => {
            unknown(ctx.dev, other);
            Err(EINVAL)
        }
    }
}

/// A query V2 does not know: EINVAL as Linux, logged once per number (the next query a program wants).
fn unknown(dev: &Device, query: u32) {
    dev.note_unknown("info.unknown_query", query);
}

/// `amdgpu_hw_ip_info`: zeros for a type the chip lacks, EINVAL for a type number the driver does not know.
pub fn hw_ip_info(gpu: &dyn GpuDevice, q: &drm_amdgpu_info__query_hw_ip) -> Result<drm_amdgpu_info_hw_ip, Errno> {
    if q.ip_instance >= AMDGPU_HW_IP_INSTANCE_MAX_COUNT || q.r#type as usize >= HW_IP_NUM {
        return Err(EINVAL);
    }
    let mut r = drm_amdgpu_info_hw_ip::ZERO;
    let Some(h) = gpu.profile().hw_ip(q.r#type) else { return Ok(r) };
    let rings = gpu.rings_ready(q.r#type).min(CTX_ENTITIES[q.r#type as usize]);
    r.hw_ip_version_major = h.major;
    r.hw_ip_version_minor = h.minor;
    r.ip_discovery_version = h.discovery;
    r.available_rings = (1u32 << rings) - 1;
    r.ib_start_alignment = h.ib_start_alignment;
    r.ib_size_alignment = h.ib_size_alignment;
    Ok(r)
}

/// VRAM_GTT (kms.c:672): the usable parts after pins and the VM's reserve.
pub fn vram_gtt(m: &MemoryState) -> drm_amdgpu_info_vram_gtt {
    let vram = usable_vram(m);
    drm_amdgpu_info_vram_gtt {
        vram_size: vram,
        vram_cpu_accessible_size: m.vram_visible.saturating_sub(m.visible_pinned).min(vram),
        gtt_size: m.gtt_total.saturating_sub(m.gart_pinned),
    }
}

fn usable_vram(m: &MemoryState) -> u64 {
    m.vram_total.saturating_sub(m.vram_pinned).saturating_sub(VM_RESERVED_VRAM)
}

/// MEMORY (kms.c:687): each heap's total, usable, usage and a largest allocation of three quarters of usable.
pub fn memory_info(m: &MemoryState) -> drm_amdgpu_memory_info {
    let heap = |total: u64, usable: u64, usage: u64| drm_amdgpu_heap_info {
        total_heap_size: total,
        usable_heap_size: usable,
        heap_usage: usage,
        max_allocation: (usable as u128 * 3 / 4) as u64,
    };
    let vram = usable_vram(m);
    let vis = m.vram_visible.saturating_sub(m.visible_pinned).min(vram);
    drm_amdgpu_memory_info {
        vram: heap(m.vram_total, vram, m.vram_usage),
        cpu_accessible_vram: heap(m.vram_visible, vis, m.vis_vram_usage),
        gtt: heap(m.gtt_total, m.gtt_total.saturating_sub(m.gart_pinned), m.gtt_usage),
    }
}

/// READ_MMR_REG (kms.c:724): SE/SH fields 0xff = broadcast, out of range EINVAL, at most 128 dwords, a register
/// outside the chip's allowed list EFAULT (Linux's answer for an unallowed offset).
fn read_mmr(ctx: &Ctx, gpu: &dyn GpuDevice, r: &drm_amdgpu_info__read_mmr_reg, ptr: u64, size: u32) -> Result<i64, Errno> {
    let field = |shift: u32| (r.instance >> shift) & AMDGPU_INFO_MMR_SE_INDEX_MASK;
    let pick = |v: u32, max: u32| match v {
        0xff => Ok(None),
        v if v >= max => Err(EINVAL),
        v => Ok(Some(v)),
    };
    let se = pick(field(AMDGPU_INFO_MMR_SE_INDEX_SHIFT), MAX_SE)?;
    let sh = pick(field(AMDGPU_INFO_MMR_SH_INDEX_SHIFT), MAX_SH_PER_SE)?;
    if r.count > MAX_MMR_COUNT {
        return Err(EINVAL);
    }
    let mut regs = Vec::new();
    regs.try_reserve_exact(r.count as usize * 4).map_err(|_| ENOMEM)?;
    for i in 0..r.count {
        let off = r.dword_offset.wrapping_add(i);
        let v = gpu
            .profile()
            .registers
            .iter()
            .find(|g| g.offset == off)
            .and_then(|g| gpu.read_register(g, se, sh));
        let Some(v) = v else {
            ctx.dev.note_unknown("info.mmr_refused", off);
            return Err(EFAULT);
        };
        regs.extend_from_slice(&v.to_le_bytes());
    }
    out(ctx, ptr, size, &regs)
}

fn vbios(ctx: &Ctx, gpu: &dyn GpuDevice, q: &drm_amdgpu_info__vbios_info, ptr: u64, size: u32) -> Result<i64, Errno> {
    let v = gpu.profile().vbios;
    match q.r#type {
        AMDGPU_INFO_VBIOS_SIZE => out(ctx, ptr, size, &v.size.to_le_bytes()),
        // No image is kept: every offset is past its end.
        AMDGPU_INFO_VBIOS_IMAGE => Err(EINVAL),
        AMDGPU_INFO_VBIOS_INFO => {
            let mut b = drm_amdgpu_info_vbios::ZERO;
            fill(&mut b.name, v.name);
            fill(&mut b.vbios_pn, v.pn);
            b.version = v.version;
            fill(&mut b.vbios_ver_str, v.ver_str);
            fill(&mut b.date, v.date);
            out(ctx, ptr, size, &bytes(&b))
        }
        _ => Err(EINVAL),
    }
}

fn fill(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len());
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

fn sensor(ty: u32) -> Option<Sensor> {
    Some(match ty {
        AMDGPU_INFO_SENSOR_GFX_SCLK => Sensor::GfxSclk,
        AMDGPU_INFO_SENSOR_GFX_MCLK => Sensor::GfxMclk,
        AMDGPU_INFO_SENSOR_GPU_TEMP => Sensor::GpuTemp,
        AMDGPU_INFO_SENSOR_GPU_LOAD => Sensor::GpuLoad,
        AMDGPU_INFO_SENSOR_GPU_AVG_POWER => Sensor::GpuAvgPower,
        AMDGPU_INFO_SENSOR_VDDNB => Sensor::Vddnb,
        AMDGPU_INFO_SENSOR_VDDGFX => Sensor::Vddgfx,
        AMDGPU_INFO_SENSOR_STABLE_PSTATE_GFX_SCLK => Sensor::StablePstateSclk,
        AMDGPU_INFO_SENSOR_STABLE_PSTATE_GFX_MCLK => Sensor::StablePstateMclk,
        AMDGPU_INFO_SENSOR_PEAK_PSTATE_GFX_SCLK => Sensor::PeakPstateSclk,
        AMDGPU_INFO_SENSOR_PEAK_PSTATE_GFX_MCLK => Sensor::PeakPstateMclk,
        // GPU_INPUT_POWER (0xc) is newer than Linux 6.6.
        _ => return None,
    })
}
