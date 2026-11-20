//! SYS-K in-kernel tests of what the metric sampler feeds (all `Safe`, rerunnable): `/proc/bitsos/metrics`, the hwmon
//! class with a chip of the test's own, the k10temp and package-power chips and the powercap zone (present exactly
//! when the sampler found the sensors and RAPL; QEMU has neither, the Mini-PC both), and cpufreq (present exactly
//! when the CPU has APERF/MPERF). Each test waits for sampler rounds where it needs them, at most a few seconds.

use crate::metrics::latest;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::{ENODATA, ENOENT};

fn text(path: &str) -> Result<String, String> {
    let b = vfs::root_ns().read_file(path, 1 << 20).map_err(|e| format!("{path}: errno {e}"))?;
    String::from_utf8(b).map_err(|_| format!("{path}: not UTF-8"))
}

fn mode(path: &str) -> Result<u32, String> {
    Ok(vfs::root_ns().lookup(path).map_err(|e| format!("{path}: errno {e}"))?.inode.meta.mode() & 0o7777)
}

fn missing(path: &str) -> bool {
    vfs::root_ns().lookup(path).err() == Some(ENOENT)
}

fn number(t: &str) -> Option<i64> {
    t.strip_suffix('\n')?.parse().ok()
}

/// Waits until the sampler has a fresh round (it rounds every 2 s); the round's time.
fn fresh_round() -> Result<alloc::sync::Arc<latest::Latest>, String> {
    for _ in 0..40 {
        if let Some(l) = latest::get(klog::now()) {
            return Ok(l);
        }
        sched::sleep_ms(200);
    }
    Err(String::from("the metric sampler left no fresh round in 8 s"))
}

fn next_round(after_ns: u64) -> Result<alloc::sync::Arc<latest::Latest>, String> {
    for _ in 0..40 {
        if let Some(l) = latest::get(klog::now()).filter(|l| l.at_ns > after_ns) {
            return Ok(l);
        }
        sched::sleep_ms(200);
    }
    Err(String::from("no new sampler round in 8 s"))
}

/// The hwmon directory of the chip named `name` (in the class, as tools find it).
fn chip_dir(name: &str) -> Option<String> {
    vfs::hwmon::chips().iter().find(|(_, c)| c.name == name).map(|(n, _)| format!("/sys/class/hwmon/hwmon{n}"))
}

fn metrics_file() -> TestResult {
    const P: &str = "/proc/bitsos/metrics";
    kassert!(mode(P)? == 0o444, "{P} mode {:o}", mode(P)?);
    // The first round only sets the rates' baselines: the second has cpu.all.busy.
    let first = next_round(fresh_round()?.at_ns)?;
    let a = text(P)?;
    for l in a.lines() {
        let w: Vec<&str> = l.split(' ').collect();
        kassert!(w.len() == 3 && !w[0].is_empty() && w[1].parse::<f64>().is_ok() && !w[2].is_empty(), "line {l:?}");
        kassert!(!crate::metrics::procfile::per_program(w[0]), "a program's series: {l}");
    }
    for k in ["cpu.all.busy ", "mem.free ", "mem.total ", "crash.kernel.previous_start "] {
        kassert!(a.lines().any(|l| l.starts_with(k)), "{P} has no {k:?}: {a}");
    }
    next_round(first.at_ns)?;
    let b = text(P)?;
    kassert!(a != b, "{P} did not change over a sampler round");
    Ok(())
}
ktest!("so.sysk.metrics_file", Safe, metrics_file);

fn hwmon_class() -> TestResult {
    use vfs::hwmon::{Attr, Chip};
    let attrs = alloc::vec![
        Attr::new("temp1_input", || Ok(String::from("42125\n"))),
        Attr::label("temp1_label", "Probe"),
        Attr::new("temp2_input", || Err(ENODATA)),
        Attr { name: String::from("temp1_crit"), mode: 0o400, read: alloc::sync::Arc::new(|| Ok(String::from("90000\n"))) },
    ];
    let n = vfs::hwmon::register(Chip { name: String::from("sysk_probe"), device: Some(String::from("devices/system/cpu")), attrs });
    let d = format!("/sys/class/hwmon/hwmon{n}");
    let r = (|| -> TestResult {
        kassert!(text(&format!("{d}/name"))? == "sysk_probe\n");
        kassert!(text(&format!("{d}/temp1_input"))? == "42125\n" && text(&format!("{d}/temp1_label"))? == "Probe\n");
        kassert!(mode(&format!("{d}/temp1_input"))? == 0o444 && mode(&format!("{d}/temp1_crit"))? == 0o400, "modes");
        kassert!(vfs::root_ns().read_file(&format!("{d}/temp2_input"), 64).err() == Some(ENODATA), "an unreadable sensor answers its errno");
        let same = |a: &str, b: &str| -> Result<bool, String> {
            let x = vfs::root_ns().lookup(a).map_err(|e| format!("{a}: errno {e}"))?;
            let y = vfs::root_ns().lookup(b).map_err(|e| format!("{b}: errno {e}"))?;
            Ok(x.inode.meta.ino == y.inode.meta.ino)
        };
        kassert!(same(&format!("{d}/device"), "/sys/devices/system/cpu")?, "device link");
        kassert!(same(&format!("{d}/subsystem"), "/sys/class/hwmon")?, "subsystem link");
        kassert!(same(&d, &format!("/sys/devices/virtual/hwmon/hwmon{n}"))?, "class link");
        Ok(())
    })();
    kassert!(vfs::hwmon::unregister(n), "unregister");
    r?;
    kassert!(missing(&d), "{d} stays after the chip went");
    Ok(())
}
ktest!("so.sysk.hwmon_class", Safe, hwmon_class);

/// `real`: the machine has the sensor (the Mini-PC's Ryzen), so its absence is a failure; otherwise (QEMU, no
/// SMN) the files must be missing as well.
fn k10temp(real: bool) -> TestResult {
    let l = fresh_round()?;
    let Some(&(_, tctl)) = l.temps.iter().find(|(n, _)| n == "tctl") else {
        kassert!(!real, "the sampler read no Tctl on this machine");
        kassert!(chip_dir("k10temp").is_none(), "a k10temp chip without the sampler's sensors");
        return Ok(());
    };
    let d = chip_dir("k10temp").ok_or("the sampler reads Tctl but there is no k10temp chip")?;
    kassert!(text(&format!("{d}/temp1_label"))? == "Tctl\n");
    let v = number(&text(&format!("{d}/temp1_input"))?).ok_or("temp1_input is no number")?;
    kassert!((5_000..=110_000).contains(&v), "Tctl {v} m°C");
    kassert!((v - tctl as i64).abs() <= 5_000, "hwmon Tctl {v} vs the sampler's {tctl}");
    for (name, _) in l.temps.iter().filter(|(n, _)| n.starts_with("ccd")) {
        let (ch, label) = crate::sysk::k10temp_channel(name).ok_or("ccd name")?;
        kassert!(text(&format!("{d}/temp{ch}_label"))? == format!("{label}\n"), "{name}");
        kassert!(number(&text(&format!("{d}/temp{ch}_input"))?).is_some_and(|t| (5_000..=110_000).contains(&t)), "{name}");
    }
    Ok(())
}
fn k10temp_qemu() -> TestResult {
    k10temp(false)
}
ktest!("so.sysk.k10temp", QemuAll, k10temp_qemu);
fn k10temp_real() -> TestResult {
    k10temp(true)
}
ktest!("so.sysk.k10temp_real", Hardware, k10temp_real);

fn package_power(real: bool) -> TestResult {
    const ZONE: &str = "/sys/class/powercap/intel-rapl:0";
    if hal::msr::read_checked(hal::msr::AMD_RAPL_POWER_UNIT).is_none() {
        kassert!(!real, "no AMD RAPL on this machine");
        kassert!(chip_dir("cpu_package").is_none() && missing(ZONE), "power files without AMD RAPL");
        return Ok(());
    }
    let first = fresh_round()?;
    next_round(first.at_ns)?;
    let d = chip_dir("cpu_package").ok_or("RAPL but no cpu_package chip")?;
    let uw = number(&text(&format!("{d}/power1_average"))?).ok_or("power1_average")?;
    kassert!(uw % 100_000 == 0 && (0..300_000_000).contains(&uw), "power1_average {uw} µW");
    kassert!(text(&format!("{d}/power1_average_interval"))? == "2000\n" && text(&format!("{d}/power1_label"))? == "Package\n");
    kassert!(text(&format!("{ZONE}/name"))? == "package-0\n");
    kassert!(mode(&format!("{ZONE}/energy_uj"))? == 0o400 && mode(&format!("{ZONE}/max_energy_range_uj"))? == 0o444, "modes");
    let max = number(&text(&format!("{ZONE}/max_energy_range_uj"))?).ok_or("max")?;
    let e1 = number(&text(&format!("{ZONE}/energy_uj"))?).ok_or("energy")?;
    sched::sleep_ms(500);
    let e2 = number(&text(&format!("{ZONE}/energy_uj"))?).ok_or("energy")?;
    kassert!(e1 <= max && e2 <= max && (e2 > e1 || e2 < e1 / 2), "energy {e1} -> {e2} of {max}: it must run on (or wrap)");
    Ok(())
}
fn package_power_qemu() -> TestResult {
    package_power(false)
}
ktest!("so.sysk.package_power", QemuAll, package_power_qemu);
fn package_power_real() -> TestResult {
    package_power(true)
}
ktest!("so.sysk.package_power_real", Hardware, package_power_real);

fn cpufreq(real: bool) -> TestResult {
    const D: &str = "/sys/devices/system/cpu/cpu0/cpufreq";
    if !hal::freq::supported() {
        kassert!(!real, "no APERF/MPERF on this machine");
        kassert!(missing(D), "cpufreq without APERF/MPERF");
        return Ok(());
    }
    let first = fresh_round()?;
    next_round(next_round(first.at_ns)?.at_ns)?;
    let n = |f: &str| -> Result<i64, String> { number(&text(&format!("{D}/{f}"))?).ok_or(format!("{f} is no number")) };
    let (cur, lo, hi) = (n("scaling_cur_freq")?, n("cpuinfo_min_freq")?, n("cpuinfo_max_freq")?);
    kassert!(lo <= cur && cur <= hi && (200_000..8_000_000).contains(&cur), "cur {cur} kHz in {lo}..{hi}");
    kassert!(mode(&format!("{D}/scaling_cur_freq"))? == 0o444);
    kassert!(hal::freq::current_khz(0).is_some(), "the sampler measured no clock on CPU 0");
    kassert!(text("/proc/bitsos/metrics")?.lines().any(|l| l.starts_with("cpu.0.khz ") && l.ends_with(" kHz")), "no cpu.0.khz series in kHz");
    Ok(())
}
fn cpufreq_qemu() -> TestResult {
    cpufreq(false)
}
ktest!("so.sysk.cpufreq", QemuAll, cpufreq_qemu);
fn cpufreq_real() -> TestResult {
    cpufreq(true)
}
ktest!("so.sysk.cpufreq_real", Hardware, cpufreq_real);
// GP-CPU2: the same machine-neutral check (APERF/MPERF) on the Gaming-PC, in its own class and its round's list.
ktest!("gp.cpu.cpufreq", GamingPc, cpufreq_real);
