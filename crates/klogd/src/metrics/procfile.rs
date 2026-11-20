//! SYS-K: `/proc/bitsos/metrics` (0444): one line per system-wide series of the newest round, `name value unit`
//! (`cpu.all.busy 12.5 %`, `temp.tctl 48.125 C`, `power.pkg 9.871 W`), then the crash facts. Per-program series
//! (`proc.<pid>.<name>.cpu`) are left out: another user's programs are not every reader's business; the counters
//! are tallies and never carry a secret. Empty but for the crash facts while the sampler has no fresh round.

use super::latest;
use super::store::Unit;
use alloc::string::String;
use core::fmt::Write;

/// A program's own series: `proc.` then its pid.
pub fn per_program(name: &str) -> bool {
    name.strip_prefix("proc.").and_then(|r| r.split('.').next()).is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Power for every reader in the steps of the hwmon chip `cpu_package` (whole tenths of a watt, sysk.rs): the
/// same 2 s average, never finer here than there (energy_uj is root's since CVE-2020-8694).
fn public(unit: Unit, v: u64) -> u64 {
    if unit == Unit::Watt { crate::sysk::public_power_uw(v) / 1000 } else { v }
}

/// The text for a round's values and the crash facts.
pub fn render<'a>(values: impl Iterator<Item = (&'a str, Unit, u64)>, previous_crash: bool, programs_crashed: Option<u64>) -> String {
    let mut out = String::new();
    for (name, unit, v) in values.filter(|(n, _, _)| !per_program(n)) {
        out.push_str(name);
        out.push(' ');
        unit.write(public(unit, v), &mut out);
        let _ = writeln!(out, " {}", unit.label());
    }
    // The start before this one ended in a kernel crash (its record was found at boot), and programs a fault
    // ended since this start (`proc.faulted`, shown again under a plain name).
    let _ = writeln!(out, "crash.kernel.previous_start {} n", u8::from(previous_crash));
    if let Some(n) = programs_crashed {
        let _ = writeln!(out, "crash.programs {n} n");
    }
    out
}

fn proc_render() -> String {
    let now = klog::now();
    let l = latest::get(now);
    let values: &[(String, Unit, u64)] = l.as_deref().map_or(&[], |l| &l.values);
    let crashed = values.iter().find(|(n, _, _)| n == "proc.faulted").map(|(_, _, v)| *v);
    render(values.iter().map(|(n, u, v)| (n.as_str(), *u, *v)), kexec::crash::previous().is_some(), crashed)
}
vfs::procfile!("/proc/bitsos/metrics", proc_render);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_name_value_unit_without_programs() {
        let v = [("cpu.all.busy", Unit::Pct, 1250), ("proc.7.bitscomp.cpu", Unit::Pct, 5000), ("proc.faulted", Unit::Count, 2), ("temp.tctl", Unit::Celsius, 48_125)];
        let t = render(v.into_iter(), true, Some(2));
        assert_eq!(t, "cpu.all.busy 12.5 %\nproc.faulted 2 n\ntemp.tctl 48.125 C\ncrash.kernel.previous_start 1 n\ncrash.programs 2 n\n");
        assert!(per_program("proc.12.x.cpu") && !per_program("proc.faulted") && !per_program("proc.") && !per_program("cpu.0.busy"));
        assert_eq!(render(core::iter::empty(), false, None), "crash.kernel.previous_start 0 n\n");
    }

    #[test]
    fn power_shows_in_the_public_steps() {
        let t = render([("power.pkg", Unit::Watt, 12_345), ("power.gpu", Unit::Watt, 99)].into_iter(), false, None);
        assert_eq!(t, "power.pkg 12.3 W\npower.gpu 0 W\ncrash.kernel.previous_start 0 n\n");
    }
}
