//! SYS-K: what the metric sampler measures, in Linux's places for Linux's tools (all read-only):
//! - hwmon `k10temp` (linux-7.3.1/drivers/hwmon/k10temp.c on Zen: `temp1_input`/`temp1_label` Tctl, `temp3..`
//!   Tccd1.., labels shown on Zen only), values from the sampler's last round (2 s old at most, never device
//!   I/O on the reader's thread);
//! - hwmon `cpu_package` with `power1_average` (µW) and `power1_average_interval` (ms): the package power the
//!   sampler averages over its 2 s round, in steps of 0.1 W. BitsOS's own chip: Linux has no power attribute for
//!   the package on AMD, only RAPL's `energy_uj`, which is root-only since CVE-2020-8694 (fine-grained energy as a
//!   side channel, Platypus). A 2 s average in 0.1 W steps carries no such signal, so every user may read it;
//! - `/sys/class/powercap/intel-rapl:0` (intel_rapl_msr's AMD package domain, linux-7.3.1/drivers/powercap/
//!   intel_rapl_common.c): `name` package-0, `max_energy_range_uj`, and `energy_uj` 0400 root as Linux. It reads
//!   the energy MSR at the read (an MSR, no device I/O) for root's exact counter.

use crate::metrics::latest;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::msr::{AMD_PKG_ENERGY_STATUS, read_checked};
use uapi::errno::ENODATA;
use vfs::Errno;
use vfs::hwmon::{Attr, Chip};
use vfs::synth::{DirSource, FileSource, FlatEntry, FlatKind, FlatView};

/// The sampler's sensor name to k10temp's channel and label: tctl is temp1, ccdN is temp(3+N) "Tccd(N+1)".
pub fn k10temp_channel(name: &str) -> Option<(u32, String)> {
    if name == "tctl" {
        return Some((1, String::from("Tctl")));
    }
    let n: u32 = name.strip_prefix("ccd")?.parse().ok()?;
    Some((3 + n, format!("Tccd{}", n + 1)))
}

/// `temp*_input`: millidegrees as an integer line.
fn temp_attr(name: String) -> impl Fn() -> Result<String, Errno> + Send + Sync + 'static {
    move || latest::temp(&name, klog::now()).map(|mc| format!("{mc}\n")).ok_or(ENODATA)
}

/// The k10temp function (00:18.3 on every Zen part) when PCI shows it.
fn k10temp_device() -> Option<String> {
    pci::devices().into_iter().find(|f| f.vendor == 0x1022 && f.addr.bus == 0 && f.addr.dev == 0x18 && f.addr.func == 3).map(|f| format!("bus/pci/devices/{:04x}:{}", f.segment, f.addr))
}

/// Registers `k10temp` once the sampler found its sensors.
pub fn k10temp_found<'a>(names: impl Iterator<Item = &'a str>) {
    let mut attrs = Vec::new();
    for n in names {
        let Some((ch, label)) = k10temp_channel(n) else { continue };
        attrs.push(Attr::new(&format!("temp{ch}_input"), temp_attr(String::from(n))));
        attrs.push(Attr::label(&format!("temp{ch}_label"), &label));
    }
    if attrs.is_empty() {
        return;
    }
    attrs.sort_by(|a, b| a.name.cmp(&b.name));
    let n = vfs::hwmon::register(Chip { name: String::from("k10temp"), device: k10temp_device(), attrs });
    klog::event!(Info, "klog", "sysk.hwmon", chip = "k10temp", index = n);
}

/// Package power for every reader: the round's average, down to whole tenths of a watt, in µW.
pub fn public_power_uw(mw: u64) -> u64 {
    mw / 100 * 100 * 1000
}

/// µJ per raw unit times 1000 (`rd->energy_unit`, intel_rapl_common.c:741) and the xlate to µJ (:536).
pub fn energy_uj(raw: u64, esu: u32) -> u64 {
    let unit = (1000u64 * 1_000_000) >> esu.min(63);
    (raw as u128 * unit as u128 / 1000) as u64
}

static ESU: ksync::Once<u32> = ksync::Once::new();

/// Registers `cpu_package` and the powercap zone when the CPU has AMD RAPL.
pub fn rapl_found(esu: u32) {
    ESU.get_or_init(|| esu);
    let attrs = alloc::vec![
        Attr::new("power1_average", || latest::pkg_mw(klog::now()).map(|mw| format!("{}\n", public_power_uw(mw))).ok_or(ENODATA)),
        Attr::new("power1_average_interval", || Ok(format!("{}\n", crate::metrics::PERIOD_NS / 1_000_000))),
        Attr::label("power1_label", "Package"),
    ];
    let n = vfs::hwmon::register(Chip { name: String::from("cpu_package"), device: None, attrs });
    klog::event!(Info, "klog", "sysk.hwmon", chip = "cpu_package", index = n);
}

/// A powercap attribute read at each read.
struct Live(fn(u32) -> Result<String, Errno>);

impl FileSource for Live {
    fn may_open(&self, _who: vfs::Cred, write: bool) -> Result<(), Errno> {
        if write { Err(uapi::errno::EACCES) } else { Ok(()) }
    }
    fn render(&self, _who: vfs::Cred) -> Result<Vec<u8>, Errno> {
        (self.0)(ESU.get().copied().ok_or(uapi::errno::ENODEV)?).map(String::into_bytes)
    }
    fn size(&self) -> u64 {
        4096
    }
}

const ZONE: &str = "devices/virtual/powercap/intel-rapl/intel-rapl:0";

fn powercap_entries() -> Vec<FlatEntry> {
    if ESU.get().is_none() {
        return Vec::new();
    }
    let dir = |p: &str| FlatEntry { path: String::from(p), kind: FlatKind::Dir };
    let file = |name: &str, perm: u32, f: fn(u32) -> Result<String, Errno>| FlatEntry { path: format!("{ZONE}/{name}"), kind: FlatKind::File { perm, src: Arc::new(Live(f)) } };
    let link = |p: &str, t: &str| FlatEntry { path: String::from(p), kind: FlatKind::Link(String::from(t)) };
    let mut v = alloc::vec![
        dir("class"),
        dir("class/powercap"),
        link("class/powercap/intel-rapl", "../../devices/virtual/powercap/intel-rapl"),
        link("class/powercap/intel-rapl:0", "../../devices/virtual/powercap/intel-rapl/intel-rapl:0"),
        dir("devices"),
        dir("devices/virtual"),
        dir("devices/virtual/powercap"),
        dir("devices/virtual/powercap/intel-rapl"),
        dir(ZONE),
        file("energy_uj", 0o400, |esu| read_checked(AMD_PKG_ENERGY_STATUS).map(|r| format!("{}\n", energy_uj(r & 0xffff_ffff, esu))).ok_or(uapi::errno::EIO)),
        file("max_energy_range_uj", 0o444, |esu| Ok(format!("{}\n", energy_uj(0xffff_ffff, esu)))),
        FlatEntry { path: format!("{ZONE}/name"), kind: FlatKind::Text(String::from("package-0\n")) },
        link(&format!("{ZONE}/subsystem"), "../../../../../class/powercap"),
    ];
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

fn powercap() -> Arc<dyn DirSource> {
    Arc::new(FlatView::new(|| Arc::new(powercap_entries())))
}
vfs::procdir!("/sys", powercap);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k10temp_channels_and_labels_follow_linux() {
        assert_eq!(k10temp_channel("tctl"), Some((1, String::from("Tctl"))));
        assert_eq!(k10temp_channel("ccd0"), Some((3, String::from("Tccd1"))));
        assert_eq!(k10temp_channel("ccd7"), Some((10, String::from("Tccd8"))));
        assert_eq!(k10temp_channel("gpu"), None);
    }

    #[test]
    fn energy_and_power_units() {
        // esu 16: Linux's unit is 15258 thousandths of a µJ (truncated), so 65536 units are 999948 µJ, not 1 J.
        assert_eq!(energy_uj(65_536, 16), 999_948);
        assert_eq!(energy_uj(0xffff_ffff, 16), 65_532_610_987, "max_energy_range_uj as Linux computes it");
        assert_eq!(public_power_uw(12_345), 12_300_000, "12.345 W shows as 12.3 W");
        assert_eq!(public_power_uw(99), 0);
    }
}
