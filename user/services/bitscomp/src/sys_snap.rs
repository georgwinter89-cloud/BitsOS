//! SYS-A: one look at the machine as the sampler hands it to the page: every value an Option (None: the source is
//! missing, the page shows "–" with its reason), rates from the difference to the previous look.

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cpu {
    /// Busy share of the whole processor, of it programs and kernel (percent).
    pub busy: Option<f64>,
    pub programs: Option<f64>,
    pub kernel: Option<f64>,
    /// Per thread (percent).
    pub threads: Vec<Option<f64>>,
    pub cur_khz: Option<u64>,
    pub max_khz: Option<u64>,
    pub switches: Option<f64>,
    pub load: Option<(f64, f64, f64)>,
    pub running: Option<u64>,
    pub all_threads: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Memory {
    pub mem: Option<crate::sys_parse::Mem>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gpu {
    pub busy: Option<f64>,
    pub vram: Option<(u64, u64)>,
    pub gtt: Option<(u64, u64)>,
    pub temp: Option<f64>,
    pub power: Option<f64>,
    pub sclk_mhz: Option<f64>,
    pub mclk_mhz: Option<f64>,
    pub volt: Option<f64>,
    /// Where the numbers came from ("sysfs", "amdgpu", "").
    pub via: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    pub name: String,
    pub size: Option<u64>,
    pub read: Option<f64>,
    pub write: Option<f64>,
    pub read_total: Option<u64>,
    pub write_total: Option<u64>,
    pub rotational: Option<bool>,
    pub model: Option<String>,
    /// Partitions: (name, size).
    pub parts: Vec<(String, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mount {
    pub dev: String,
    pub path: String,
    pub used: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Net {
    pub name: String,
    pub rx: Option<f64>,
    pub tx: Option<f64>,
    pub rx_total: u64,
    pub tx_total: u64,
    pub up: Option<bool>,
    pub speed_mbps: Option<u64>,
    /// (link quality, signal dBm) of a WLAN interface.
    pub wifi: Option<(f64, f64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heat {
    pub cpu: Option<f64>,
    pub gpu: Option<f64>,
    pub nvme: Option<f64>,
    pub cpu_power: Option<f64>,
    pub gpu_power: Option<f64>,
    /// Every hwmon reading: (sensor name, label, °C).
    pub all: Vec<(String, String, f64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgrp: u32,
    pub uid: Option<u32>,
    pub comm: String,
    pub argv: Vec<String>,
    pub state: char,
    pub cpu: Option<f64>,
    pub rss: Option<u64>,
    pub shared: Option<u64>,
    pub threads: u32,
    pub nice: i32,
    /// Start in clock ticks since boot (field 22), also what makes a pid the same process.
    pub start: u64,
    pub disk: Option<f64>,
    pub gpu_mem: Option<u64>,
    pub files: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Kernel {
    pub generation: Option<(u64, String)>,
    pub boot_ms: Option<u64>,
    pub uptime: Option<f64>,
    pub boot_wall: Option<i64>,
    pub crash: Option<(i64, u64)>,
    /// Crashes since the start (K1 metrics `crash.*`).
    pub crashes: Option<f64>,
    pub calls: Option<f64>,
    pub refused: Option<f64>,
    pub oom: Option<f64>,
    pub reclaim: Option<f64>,
}

/// What does not change while the program is open (read on open and every half minute).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    pub cpu_model: Option<String>,
    pub cores: Option<u32>,
    pub threads: Option<u32>,
    pub gpu_name: Option<String>,
    pub mem_total: Option<u64>,
    pub dmi: Vec<(String, String)>,
    pub usb: Vec<(String, String)>,
    pub pci: Option<usize>,
    pub inputs: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Seconds on the sampler's monotonic clock, and the wall clock.
    pub t: u64,
    pub wall: i64,
    pub cpu: Cpu,
    pub mem: Memory,
    pub gpu: Gpu,
    pub disks: Vec<Disk>,
    pub mounts: Vec<Mount>,
    pub nets: Vec<Net>,
    pub heat: Heat,
    pub kernel: Kernel,
    pub facts: Facts,
    /// Empty unless a shown tab needs the processes.
    pub procs: Vec<Proc>,
    pub procs_read: bool,
    /// How many processes there are (one directory listing, every look).
    pub proc_count: Option<usize>,
    /// Clock ticks per second (start times).
    pub hz: u64,
    /// The metrics of /proc/bitsos/metrics (K1), when the kernel has them.
    pub metrics: Vec<(String, f64, String)>,
}
