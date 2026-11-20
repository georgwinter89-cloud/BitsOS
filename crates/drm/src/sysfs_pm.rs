//! SYS-K: a GPU's load, memory and sensors where Linux's amdgpu puts them (linux-7.3.1/drivers/gpu/drm/amd/pm/
//! amdgpu_pm.c, all 0444): `gpu_busy_percent`, `mem_info_{vram,gtt}_{used,total}` in the PCI function's directory,
//! `/sys/class/drm/card<n>` with its `device` link (no `dev`: BitsOS makes no card node, only the render node), and
//! the hwmon chip `amdgpu` (`temp1` edge, `in0` vddgfx, `in1` vddnb, `power1_average`/`power1_input` PPT,
//! `freq1` sclk: the attributes Linux shows on an APU of this generation), linked from `<function>/hwmon`.
//!
//! On hardware a sensor reading is an SMU exchange, so readers never make one: the thread `drm-pm` refreshes
//! every 2 s while somebody read in the last 30 s and sleeps otherwise; a read that finds no fresh reading wakes
//! it and waits at most 300 ms. Without DPM a sensor file answers EOPNOTSUPP (Linux's `smu_read_sensor` with DPM
//! off); with DPM but no reading to be had, ENODATA. The memory numbers are counters and always there.

use crate::device::{self, Device, RENDER_BASE};
use crate::pm::PmSample;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use linux::errno::{ENODATA, ENODEV, EOPNOTSUPP};
use vfs::Errno;
use vfs::hwmon::{Attr, Chip};
use vfs::inode::Cred;
use vfs::synth::{FileSource, FlatEntry, FlatKind};

const PERIOD_MS: u64 = 2000;
const ACTIVE_NS: u64 = 30_000_000_000;
const FRESH_NS: u64 = 5_000_000_000;
const WAIT_NS: u64 = 300_000_000;
const IDLE_WAIT_NS: u64 = 10_000_000_000;

/// (render minor, when, the reading) per device.
static CACHE: SpinLock<Vec<(u32, u64, Option<PmSample>)>> = SpinLock::named("drm.pm.cache", Vec::new());
/// (render minor, hwmon index).
static CHIPS: SpinLock<Vec<(u32, u32)>> = SpinLock::named("drm.pm.chips", Vec::new());
static LAST_READ: AtomicU64 = AtomicU64::new(0);
static WANT: AtomicBool = AtomicBool::new(false);
static GEN: AtomicU64 = AtomicU64::new(0);
static WAKE: WaitQueue = WaitQueue::new();
static DONE: WaitQueue = WaitQueue::new();
static STARTED: ksync::Once<()> = ksync::Once::new();

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

fn refresh() {
    let mut fresh = Vec::new();
    for d in device::devices() {
        if let Some(g) = d.driver().gpu() {
            fresh.push((d.render_minor(), now(), g.pm_sample()));
        }
    }
    *CACHE.lock() = fresh;
    GEN.fetch_add(1, Ordering::AcqRel);
    DONE.wake_all();
}

/// Somebody read in the last `ACTIVE_NS` (0: nobody ever did).
fn active() -> bool {
    let last = LAST_READ.load(Ordering::Acquire);
    last != 0 && now().saturating_sub(last) < ACTIVE_NS
}

fn run() {
    loop {
        if WANT.swap(false, Ordering::AcqRel) || active() {
            refresh();
            // A reader that finds no fresh value (a device that just came) cuts the pause short.
            WAKE.wait_until_timeout(|| WANT.load(Ordering::Acquire), PERIOD_MS * 1_000_000);
        } else {
            WAKE.wait_until_timeout(|| WANT.load(Ordering::Acquire), IDLE_WAIT_NS);
        }
    }
}

fn cached(minor: u32) -> Option<(u64, Option<PmSample>)> {
    CACHE.lock().iter().find(|e| e.0 == minor).map(|e| (e.1, e.2))
}

/// The reading for `minor`, waking the sampler when it is old.
fn sample(minor: u32) -> Option<PmSample> {
    let t = now();
    LAST_READ.store(t, Ordering::Release);
    let fresh = |c: Option<(u64, Option<PmSample>)>| c.filter(|(at, _)| t.saturating_sub(*at) <= FRESH_NS);
    if let Some((_, s)) = fresh(cached(minor)) {
        return s;
    }
    let g0 = GEN.load(Ordering::Acquire);
    WANT.store(true, Ordering::Release);
    WAKE.wake_all();
    DONE.wait_until_timeout(|| GEN.load(Ordering::Acquire) != g0, WAIT_NS);
    fresh(cached(minor)).and_then(|(_, s)| s)
}

/// A sensor value, or Linux's error (EOPNOTSUPP without DPM, ENODATA otherwise).
fn sensor(minor: u32, f: fn(&PmSample) -> Option<u32>) -> Result<u32, Errno> {
    let dpm = device::by_render_minor(minor).and_then(|d| d.driver().gpu().map(|g| g.dpm_enabled())).ok_or(ENODEV)?;
    if !dpm {
        return Err(EOPNOTSUPP);
    }
    sample(minor).as_ref().and_then(f).ok_or(ENODATA)
}

/// Linux's line for a sensor value.
pub fn sensor_text(what: &str, v: u32) -> String {
    match what {
        // MHz to Hz (amdgpu_hwmon_show_sclk: 10 kHz units * 10 * 1000).
        "freq1_input" => format!("{}\n", v as u64 * 1_000_000),
        // mW to µW (amdgpu_hwmon_get_power: query * 1000).
        "power1_average" | "power1_input" => format!("{}\n", v as u64 * 1000),
        _ => format!("{v}\n"),
    }
}

struct SensorFile {
    minor: u32,
    what: &'static str,
    get: fn(&PmSample) -> Option<u32>,
}

impl FileSource for SensorFile {
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write { Err(linux::errno::EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        sensor(self.minor, self.get).map(|v| sensor_text(self.what, v).into_bytes())
    }
    fn size(&self) -> u64 {
        4096
    }
}

struct MemFile {
    minor: u32,
    get: fn(&crate::gpu::MemoryState) -> u64,
}

impl FileSource for MemFile {
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write { Err(linux::errno::EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        let d = device::by_render_minor(self.minor).ok_or(ENODEV)?;
        let g = d.driver().gpu().ok_or(ENODEV)?;
        Ok(format!("{}\n", (self.get)(&g.memory())).into_bytes())
    }
    fn size(&self) -> u64 {
        4096
    }
}

const MEM: [(&str, fn(&crate::gpu::MemoryState) -> u64); 4] = [
    ("mem_info_gtt_total", |m| m.gtt_total),
    ("mem_info_gtt_used", |m| m.gtt_usage),
    ("mem_info_vram_total", |m| m.vram_total),
    ("mem_info_vram_used", |m| m.vram_usage),
];

/// The hwmon attributes with values, and their labels.
const HWMON: [(&str, fn(&PmSample) -> Option<u32>); 6] = [
    ("freq1_input", |s| s.gfx_mhz),
    ("in0_input", |s| s.vddgfx_mv),
    ("in1_input", |s| s.vddnb_mv),
    ("power1_average", |s| s.avg_mw),
    ("power1_input", |s| s.cur_mw),
    ("temp1_input", |s| s.edge_mc),
];
const LABELS: [(&str, &str); 5] = [("freq1_label", "sclk"), ("in0_label", "vddgfx"), ("in1_label", "vddnb"), ("power1_label", "PPT"), ("temp1_label", "edge")];

/// A device came: its hwmon chip, and the sampler thread on the first one.
pub fn added(d: &Arc<Device>) {
    if d.driver().gpu().is_none() {
        return;
    }
    STARTED.get_or_init(|| {
        if let Err(e) = sched::spawn("drm-pm", sched::Class::Normal, run) {
            klog::event!(Warn, "drm", "pm.spawn_failed", error = format!("{e:?}"); "the sensor files answer ENODATA");
        }
    });
    let minor = d.render_minor();
    let mut attrs: Vec<Attr> = HWMON
        .iter()
        .map(|(n, f)| {
            let (what, f) = (*n, *f);
            Attr::new(n, move || sensor(minor, f).map(|v| sensor_text(what, v)))
        })
        .collect();
    attrs.extend(LABELS.iter().map(|(n, t)| Attr::label(n, t)));
    attrs.sort_by(|a, b| a.name.cmp(&b.name));
    let fdir = d.driver().pci().sysfs_dir();
    let n = vfs::hwmon::register(Chip { name: String::from("amdgpu"), device: Some(fdir), attrs });
    CHIPS.lock().push((minor, n));
}

/// A device went: its chip and reading go.
pub fn removed(d: &Arc<Device>) {
    let minor = d.render_minor();
    let gone: Vec<u32> = {
        let mut c = CHIPS.lock();
        let g = c.iter().filter(|e| e.0 == minor).map(|e| e.1).collect();
        c.retain(|e| e.0 != minor);
        g
    };
    for n in gone {
        vfs::hwmon::unregister(n);
    }
    CACHE.lock().retain(|e| e.0 != minor);
}

/// "../" once per component.
fn up(dir: &str) -> String {
    "../".repeat(dir.split('/').filter(|c| !c.is_empty()).count())
}

/// The entries of one device in DRM's /sys view (added to `sysfs::entries_of`).
pub fn entries(out: &mut Vec<FlatEntry>, d: &Device, fdir: &str) {
    let minor = d.render_minor();
    if d.driver().gpu().is_some() {
        out.push(FlatEntry { path: format!("{fdir}/gpu_busy_percent"), kind: FlatKind::File { perm: 0o444, src: Arc::new(SensorFile { minor, what: "gpu_busy_percent", get: |s| s.busy_pct }) } });
        for (n, f) in MEM {
            out.push(FlatEntry { path: format!("{fdir}/{n}"), kind: FlatKind::File { perm: 0o444, src: Arc::new(MemFile { minor, get: f }) } });
        }
        if let Some(&(_, n)) = CHIPS.lock().iter().find(|e| e.0 == minor) {
            let h = format!("{fdir}/hwmon");
            out.push(FlatEntry { path: h.clone(), kind: FlatKind::Dir });
            out.push(FlatEntry { path: format!("{h}/hwmon{n}"), kind: FlatKind::Link(format!("{}devices/virtual/hwmon/hwmon{n}", up(&h))) });
        }
    }
    let card = format!("card{}", minor.saturating_sub(RENDER_BASE));
    let cdir = format!("{fdir}/drm/{card}");
    out.push(FlatEntry { path: cdir.clone(), kind: FlatKind::Dir });
    out.push(FlatEntry { path: format!("{cdir}/device"), kind: FlatKind::Link(String::from("../..")) });
    out.push(FlatEntry { path: format!("{cdir}/subsystem"), kind: FlatKind::Link(format!("{}class/drm", up(&cdir))) });
    out.push(FlatEntry { path: format!("class/drm/{card}"), kind: FlatKind::Link(format!("../../{cdir}")) });
}

#[cfg(test)]
mod tests {
    #[test]
    fn sensor_lines_in_linux_units() {
        assert_eq!(super::sensor_text("freq1_input", 2700), "2700000000\n");
        assert_eq!(super::sensor_text("power1_average", 18_250), "18250000\n");
        assert_eq!(super::sensor_text("temp1_input", 52_000), "52000\n");
        assert_eq!(super::sensor_text("gpu_busy_percent", 7), "7\n");
    }
}
