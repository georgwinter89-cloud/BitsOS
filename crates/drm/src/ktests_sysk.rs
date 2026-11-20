//! SYS-K in-kernel tests of the GPU's sysfs and hwmon files: on the fake (QEMU, `QemuAll`) every value against what the
//! backend answers, the card link and the chip going with the device; on the Mini-PC's 780M (`Hardware`) the real
//! values are plausible.

use crate::ktests::{Dev, with_dev};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use linux::errno::ENOENT;

fn text(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 4096).map_err(|e| format!("{path}: errno {e}"))?;
    String::from_utf8(b).map_err(|_| format!("{path}: not UTF-8"))
}

fn number(path: &str) -> Result<u64, String> {
    text(path)?.strip_suffix('\n').and_then(|t| t.parse().ok()).ok_or(format!("{path}: no number"))
}

fn same(a: &str, b: &str) -> Result<bool, String> {
    let x = vfs::root_ns().lookup(a).map_err(|e| format!("{a}: errno {e}"))?;
    let y = vfs::root_ns().lookup(b).map_err(|e| format!("{b}: errno {e}"))?;
    Ok(x.inode.meta.ino == y.inode.meta.ino)
}

/// The amdgpu chip's directory for render minor `m`.
fn chip(m: u32) -> Result<String, String> {
    let d = format!("/sys/class/drm/card{}/device/hwmon", m - crate::device::RENDER_BASE);
    let l = vfs::root_ns().list_dir(&d).map_err(|e| format!("{d}: errno {e}"))?;
    let n = l.iter().map(|(n, _)| String::from_utf8_lossy(n).into_owned()).find(|n| n.starts_with("hwmon")).ok_or(format!("{d} is empty"))?;
    Ok(format!("{d}/{n}"))
}

fn fake_values() -> TestResult {
    with_dev!(d, {
        let Some(fake) = d.dev.driver().as_fake() else { return ktest::fail("not the fake GPU") };
        let m = d.dev.render_minor();
        let card = format!("/sys/class/drm/card{}", m - crate::device::RENDER_BASE);
        kassert!(same(&format!("{card}/device"), &format!("/sys/dev/char/226:{m}/device"))?, "card and render node share the function");
        let mem = crate::gpu::GpuDevice::memory(fake);
        kassert!(number(&format!("{card}/device/mem_info_vram_total"))? == mem.vram_total, "vram total");
        kassert!(number(&format!("{card}/device/mem_info_gtt_total"))? == mem.gtt_total, "gtt total");
        kassert!(number(&format!("{card}/device/mem_info_vram_used"))? <= mem.vram_total, "vram used");
        let s = |k: crate::gpu::Sensor| crate::profile_780m::SENSORS.iter().find(|(x, _)| *x == k).map(|(_, v)| *v as u64).unwrap_or(0);
        use crate::gpu::Sensor::*;
        kassert!(number(&format!("{card}/device/gpu_busy_percent"))? == s(GpuLoad), "busy");
        let h = chip(m)?;
        kassert!(text(&format!("{h}/name"))? == "amdgpu\n");
        kassert!(number(&format!("{h}/temp1_input"))? == s(GpuTemp) && text(&format!("{h}/temp1_label"))? == "edge\n", "edge");
        kassert!(number(&format!("{h}/freq1_input"))? == s(GfxSclk) * 1_000_000 && text(&format!("{h}/freq1_label"))? == "sclk\n", "sclk");
        kassert!(number(&format!("{h}/power1_average"))? == s(GpuAvgPower) * 1_000_000 && text(&format!("{h}/power1_label"))? == "PPT\n", "PPT");
        kassert!(number(&format!("{h}/in0_input"))? == s(Vddgfx) && number(&format!("{h}/in1_input"))? == s(Vddnb), "voltages");
        let mode = vfs::root_ns().lookup(&format!("{h}/temp1_input")).map_err(|e| format!("errno {e}"))?.inode.meta.mode() & 0o7777;
        kassert!(mode == 0o444, "temp1_input mode {mode:o}");
        Ok(())
    })
}
ktest!("so.sysk.drm_fake", QemuAll, fake_values);

/// A second device brings its own card and chip, and both go with it.
fn chip_goes() -> TestResult {
    with_dev!(_first, {
        let d = crate::add_device(Arc::new(crate::fake::FakeGpu::at(0xc9, 0))).map_err(|e| format!("add: errno {e}"))?;
        let m = d.render_minor();
        let r = (|| -> TestResult {
            let h = chip(m)?;
            kassert!(text(&format!("{h}/name"))? == "amdgpu\n");
            let class = vfs::hwmon::chips().iter().filter(|(_, c)| c.name == "amdgpu").count();
            kassert!(class >= 2, "{class} amdgpu chips for two devices");
            crate::remove_device(&d);
            kassert!(vfs::root_ns().lookup(&h).err() == Some(ENOENT), "{h} stays after unplug");
            kassert!(vfs::root_ns().lookup(&format!("/sys/class/drm/card{}", m - crate::device::RENDER_BASE)).is_err(), "card stays");
            Ok(())
        })();
        crate::remove_device(&d);
        r
    })
}
ktest!("so.sysk.drm_chip_goes", QemuAll, chip_goes);

/// The Mini-PC's 780M: the real numbers are plausible and the sampler serves repeated reads.
fn real_values() -> TestResult {
    let Some(dev) = crate::device::by_render_minor(crate::device::RENDER_BASE) else { return ktest::fail("no GPU at renderD128") };
    kassert!(dev.driver().as_fake().is_none(), "the fake GPU on hardware");
    let m = dev.render_minor();
    let card = format!("/sys/class/drm/card{}/device", m - crate::device::RENDER_BASE);
    let total = number(&format!("{card}/mem_info_vram_total"))?;
    kassert!(total >= 256 << 20 && number(&format!("{card}/mem_info_vram_used"))? <= total, "vram {total}");
    kassert!(number(&format!("{card}/mem_info_gtt_total"))? > 0, "gtt total");
    let busy = number(&format!("{card}/gpu_busy_percent"))?;
    kassert!(busy <= 100, "busy {busy}");
    let h = chip(m)?;
    let edge = number(&format!("{h}/temp1_input"))?;
    kassert!((15_000..=105_000).contains(&edge), "edge {edge} m°C");
    let sclk = number(&format!("{h}/freq1_input"))?;
    kassert!((100_000_000..=3_500_000_000).contains(&sclk), "sclk {sclk} Hz");
    let ppt = number(&format!("{h}/power1_average"))?;
    kassert!(ppt < 120_000_000, "PPT {ppt} µW");
    // A second read within the sampler's period comes from the kept reading.
    kassert!(number(&format!("{h}/temp1_input"))?.abs_diff(edge) <= 10_000, "edge jumped");
    Ok(())
}
ktest!("so.sysk.drm_real", Hardware, real_values);
