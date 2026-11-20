//! SYS-A: where the program "System" reads, under a root (`/`; host tests point it at a fixture tree). The K1 paths of the measurement contract (Linux's layout, so Linux tools
//! read the same): cpufreq, hwmon (k10temp, amdgpu, nvme, the package's power), the drm card's busy share and
//! memory, DMI, USB, block devices. A missing file is a None, never an error: lane SO's kernel side may come later,
//! and the page starts showing the value the moment the file appears.

use std::path::{Path, PathBuf};

use crate::sys_snap::{Disk, Facts, Gpu, Heat};

const MAX_FILE: u64 = 1 << 20;

#[derive(Clone, Debug)]
pub struct Root(pub PathBuf);

impl Root {
    pub fn system() -> Root {
        Root(PathBuf::from("/"))
    }

    /// Whether this is the machine itself (actions on processes are only allowed then).
    pub fn is_real(&self) -> bool {
        self.0 == Path::new("/")
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.0.join(p.trim_start_matches('/'))
    }

    pub fn read(&self, p: &str) -> Option<String> {
        use std::io::Read;
        let mut s = String::new();
        std::fs::File::open(self.path(p)).ok()?.take(MAX_FILE).read_to_string(&mut s).ok()?;
        Some(s)
    }

    pub fn line(&self, p: &str) -> Option<String> {
        let s = self.read(p)?;
        let l = s.lines().next()?.trim().to_string();
        (!l.is_empty()).then_some(l)
    }

    pub fn num<T: std::str::FromStr>(&self, p: &str) -> Option<T> {
        self.line(p)?.parse().ok()
    }

    pub fn list(&self, dir: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.path(dir)).map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
        v.sort_by(|a, b| natural(a, b));
        v
    }

    pub fn exists(&self, p: &str) -> bool {
        self.path(p).exists()
    }
}

/// "hwmon10" after "hwmon2".
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let i = s.find(|c: char| c.is_ascii_digit()).unwrap_or(s.len());
        (s[..i].to_string(), s[i..].parse::<u64>().unwrap_or(0), s.to_string())
    };
    split(a).cmp(&split(b))
}

/// Mean current clock of all threads and the highest clock (kHz), from cpufreq.
pub fn cpufreq(r: &Root, threads: usize) -> (Option<u64>, Option<u64>) {
    let cur: Vec<u64> = (0..threads.max(1)).filter_map(|i| r.num(&format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq"))).collect();
    let mean = (!cur.is_empty()).then(|| cur.iter().sum::<u64>() / cur.len() as u64);
    (mean, r.num("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"))
}

/// Temperatures and power from hwmon: k10temp's Tctl, amdgpu's edge and power, nvme's composite, and the package
/// power any other sensor offers as power1_average (K1: the package power, readable, averaged).
/// hwmon chips whose power1 is the processor package: BitsOS's own `cpu_package` (K1, lane SO: the 2 s average of
/// the package energy, since energy_uj stays root's), and the Zen drivers if they carry one.
pub const CPU_POWER: [&str; 3] = ["cpu_package", "k10temp", "zenpower"];

pub fn hwmon(r: &Root) -> (Heat, Option<f64>) {
    let mut h = Heat::default();
    let mut sclk = None;
    for d in r.list("/sys/class/hwmon") {
        let base = format!("/sys/class/hwmon/{d}");
        let Some(name) = r.line(&format!("{base}/name")) else { continue };
        let files = r.list(&base);
        for f in files.iter().filter(|f| f.starts_with("temp") && f.ends_with("_input")) {
            let Some(mc) = r.num::<f64>(&format!("{base}/{f}")) else { continue };
            let label = r.line(&format!("{base}/{}", f.replace("_input", "_label"))).unwrap_or_else(|| f.trim_end_matches("_input").to_string());
            let c = mc / 1000.0;
            h.all.push((name.clone(), label.clone(), c));
            let first = f == "temp1_input";
            match name.as_str() {
                "k10temp" | "zenpower" if label == "Tctl" || (first && h.cpu.is_none()) => h.cpu = Some(c),
                "amdgpu" if label == "edge" || (first && h.gpu.is_none()) => h.gpu = Some(c),
                "nvme" if label == "Composite" || (first && h.nvme.is_none()) => h.nvme = Some(c),
                _ => {}
            }
        }
        let watts = r.num::<f64>(&format!("{base}/power1_average")).or_else(|| r.num::<f64>(&format!("{base}/power1_input"))).map(|uw| uw / 1e6);
        if name == "amdgpu" {
            h.gpu_power = watts.or(h.gpu_power);
            sclk = r.num::<f64>(&format!("{base}/freq1_input")).map(|hz| hz / 1e6).or(sclk);
        } else if CPU_POWER.contains(&name.as_str()) && watts.is_some() && h.cpu_power.is_none() {
            // Only a processor's own chip: a battery's or a USB-C port's sensor is a real number, but not this one.
            h.cpu_power = watts;
        }
    }
    (h, sclk)
}

/// The drm card's busy share and memory (amdgpu's sysfs files).
pub fn drm(r: &Root) -> Gpu {
    let d = "/sys/class/drm/card0/device";
    let pair = |u: &str, t: &str| Some((r.num::<u64>(&format!("{d}/{u}"))?, r.num::<u64>(&format!("{d}/{t}"))?));
    let g = Gpu {
        busy: r.num(&format!("{d}/gpu_busy_percent")),
        vram: pair("mem_info_vram_used", "mem_info_vram_total"),
        gtt: pair("mem_info_gtt_used", "mem_info_gtt_total"),
        ..Gpu::default()
    };
    Gpu { via: if g.busy.is_some() || g.vram.is_some() { "sysfs" } else { "" }, ..g }
}

/// The card's name from its PCI ids (the 780M of the Mini-PC by name, others by vendor).
pub fn gpu_name(r: &Root) -> Option<String> {
    let d = "/sys/class/drm/card0/device";
    let vendor = r.line(&format!("{d}/vendor"))?;
    let device = r.line(&format!("{d}/device")).unwrap_or_default();
    Some(match (vendor.as_str(), device.as_str()) {
        ("0x1002", "0x15bf") => "Radeon 780M".into(),
        ("0x1002", "0x15c8") => "Radeon 740M".into(),
        ("0x1002", "0x1900") | ("0x1002", "0x1901") => "Radeon 780M".into(),
        ("0x1002", d) => format!("AMD-Grafik ({})", d.trim_start_matches("0x")),
        ("0x8086", _) => "Intel-Grafik".into(),
        ("0x10de", _) => "NVIDIA-Grafik".into(),
        ("0x1234", _) => "QEMU-Grafik".into(),
        (v, d) => format!("Grafik {}:{}", v.trim_start_matches("0x"), d.trim_start_matches("0x")),
    })
}

/// DMI facts without serial numbers or UUIDs (those are 0400 root anyway, K1).
pub fn dmi(r: &Root) -> Vec<(String, String)> {
    [("sys_vendor", "Hersteller"), ("product_name", "Modell"), ("board_vendor", "Hauptplatine von"), ("board_name", "Hauptplatine"), ("bios_vendor", "Firmware von"), ("bios_version", "Firmware"), ("bios_date", "Firmware vom")]
        .iter()
        .filter_map(|(f, de)| Some((de.to_string(), r.line(&format!("/sys/class/dmi/id/{f}")).filter(|v| !v.eq_ignore_ascii_case("default string") && !v.eq_ignore_ascii_case("to be filled by o.e.m."))?)))
        .collect()
}

/// USB devices that are not hubs: (name, speed in words).
pub fn usb(r: &Root) -> Vec<(String, String)> {
    let mut v = Vec::new();
    for port in r.list("/sys/bus/usb/devices") {
        let b = format!("/sys/bus/usb/devices/{port}");
        if port.contains(':') || r.line(&format!("{b}/bDeviceClass")).as_deref() == Some("09") {
            continue;
        }
        let Some(id) = r.line(&format!("{b}/idVendor")) else { continue };
        let prod = r.line(&format!("{b}/product"));
        let maker = r.line(&format!("{b}/manufacturer"));
        let name = match (maker, prod) {
            (Some(m), Some(p)) if !p.contains(&m) => format!("{m} {p}"),
            (_, Some(p)) => p,
            (Some(m), None) => m,
            (None, None) => format!("USB-Gerät {id}:{}", r.line(&format!("{b}/idProduct")).unwrap_or_default()),
        };
        let speed = match r.line(&format!("{b}/speed")).as_deref() {
            Some("1.5") | Some("12") => "USB 1".to_string(),
            Some("480") => "USB 2".to_string(),
            Some("5000") => "USB 3".to_string(),
            Some(s) if s.parse::<u32>().is_ok_and(|n| n > 5000) => "USB 3.2".to_string(),
            _ => "USB".to_string(),
        };
        v.push((name, speed));
    }
    v
}

/// Block devices with their partitions (not loop, ram or zram devices).
pub fn disks(r: &Root) -> Vec<Disk> {
    let mut v = Vec::new();
    for dev in r.list("/sys/block") {
        if ["loop", "ram", "zram"].iter().any(|p| dev.starts_with(p)) {
            continue;
        }
        let b = format!("/sys/block/{dev}");
        let parts = r
            .list(&b)
            .into_iter()
            .filter(|p| p.starts_with(&dev) && r.exists(&format!("{b}/{p}/partition")))
            .map(|p| {
                let s = r.num::<u64>(&format!("{b}/{p}/size")).unwrap_or(0) * 512;
                (p, s)
            })
            .collect();
        v.push(Disk {
            size: r.num::<u64>(&format!("{b}/size")).map(|s| s * 512),
            rotational: r.num::<u8>(&format!("{b}/queue/rotational")).map(|x| x == 1),
            model: r.line(&format!("{b}/device/model")),
            name: dev,
            parts,
            ..Disk::default()
        });
    }
    v
}

pub fn facts(r: &Root) -> Facts {
    let info = r.read("/proc/cpuinfo").unwrap_or_default();
    let threads = info.lines().filter(|l| l.starts_with("processor")).count() as u32;
    Facts {
        cpu_model: crate::sys_parse::cpu_model(&info),
        cores: crate::sys_parse::cpu_cores(&info),
        threads: (threads > 0).then_some(threads),
        gpu_name: gpu_name(r),
        mem_total: r.read("/proc/meminfo").as_deref().and_then(crate::sys_parse::meminfo).map(|m| m.total),
        dmi: dmi(r),
        usb: usb(r),
        pci: r.exists("/sys/bus/pci/devices").then(|| r.list("/sys/bus/pci/devices").len()),
        inputs: Vec::new(),
    }
}

/// The file systems worth showing from /proc/mounts: (device, mount point), real ones only.
pub fn mounts(text: &str) -> Vec<(String, String)> {
    const REAL: [&str; 8] = ["ext4", "ext3", "ext2", "vfat", "fat", "exfat", "btrfs", "xfs"];
    let mut v: Vec<(String, String)> = Vec::new();
    for l in text.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let (Some(dev), Some(at), Some(ty)) = (w.first(), w.get(1), w.get(2)) else { continue };
        let at = at.replace("\\040", " ");
        if REAL.contains(ty) && !v.iter().any(|(_, p)| *p == at) {
            v.push((dev.to_string(), at));
        }
    }
    v
}
