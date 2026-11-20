//! The perf knobs of `/proc/sys/kernel` (linux-7.3.1/kernel/events/core.c `events_core_sysctl_table`,
//! kernel/events/callchain.c, kernel/printk/sysctl.c `kptr_restrict`) and `/proc/kallsyms`
//! (kernel/kallsyms.c, `kallsyms_show_value`): the kernel's function symbols from the table the crash
//! backtraces use (`boot/kernel.syms`), with address 0 for readers that may not see kernel addresses.

use super::*;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use uapi::errno::*;
use vfs::synth::{DirSource, FileSource, SynthEntry};
use vfs::{Cred, Errno};

/// A signed `proc_dointvec[_minmax]` knob.
struct Int {
    at: &'static AtomicI64,
    min: i64,
    max: i64,
    /// `proc_dointvec_minmax_sysadmin`: a write needs CAP_SYS_ADMIN (EPERM).
    sysadmin: bool,
    name: &'static str,
}

impl FileSource for Int {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("{}\n", self.at.load(Ordering::Relaxed)).into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        if self.sysadmin && proc::current().is_some_and(|p| !p.capable(proc::cred::Cap::SysAdmin)) {
            return Err(EPERM);
        }
        vfs::sysctl::write_int(data, |v| {
            if !(self.min..=self.max).contains(&v) {
                return Err(EINVAL);
            }
            self.at.store(v, Ordering::Relaxed);
            klog::event!(Info, "perf", "sysctl.set", knob = self.name, value = v);
            Ok(())
        })
    }
}

/// An unsigned knob kept in an `AtomicU64`, `min..=max`, with an extra refusal (`max_sample_rate`).
struct Uint {
    at: &'static AtomicU64,
    min: i64,
    max: i64,
    refuse: fn() -> bool,
    name: &'static str,
}

impl FileSource for Uint {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(format!("{}\n", self.at.load(Ordering::Relaxed)).into_bytes())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        if (self.refuse)() {
            return Err(EINVAL);
        }
        vfs::sysctl::write_int(data, |v| {
            if !(self.min..=self.max).contains(&v) {
                return Err(EINVAL);
            }
            self.at.store(v as u64, Ordering::Relaxed);
            klog::event!(Info, "perf", "sysctl.set", knob = self.name, value = v);
            Ok(())
        })
    }
}

fn never() -> bool {
    false
}

struct KernelDir;

/// Cookies above the ones `/proc/sys/kernel`'s other source uses.
const COOKIE: u64 = 0x5e1f_0000;

impl DirSource for KernelDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let u = |at: &'static AtomicU64, min: i64, max: i64, refuse: fn() -> bool, name: &'static str| Arc::new(Uint { at, min, max, refuse, name });
        let mut v = alloc::vec![
            SynthEntry::file("kptr_restrict", 0o644, Arc::new(Int { at: &KPTR_RESTRICT, min: 0, max: 2, sysadmin: true, name: "kptr_restrict" })),
            SynthEntry::file("perf_cpu_time_max_percent", 0o644, u(&CPU_TIME_MAX_PERCENT, 0, 100, never, "perf_cpu_time_max_percent")),
            SynthEntry::file("perf_event_max_contexts_per_stack", 0o644, u(&MAX_CONTEXTS, 0, 1000, never, "perf_event_max_contexts_per_stack")),
            SynthEntry::file("perf_event_max_sample_rate", 0o644, u(&MAX_SAMPLE_RATE, 1, i32::MAX as i64, super::throttling_off, "perf_event_max_sample_rate")),
            SynthEntry::file("perf_event_max_stack", 0o644, u(&MAX_STACK, 0, 640 * 1024, never, "perf_event_max_stack")),
            SynthEntry::file("perf_event_mlock_kb", 0o644, u(&MLOCK_KB, 0, i32::MAX as i64, never, "perf_event_mlock_kb")),
            SynthEntry::file("perf_event_paranoid", 0o644, Arc::new(Int { at: &PARANOID, min: i32::MIN as i64, max: i32::MAX as i64, sysadmin: false, name: "perf_event_paranoid" })),
        ];
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = COOKIE + i as u64;
        }
        Ok(v)
    }
}

fn kernel_dir() -> Arc<dyn DirSource> {
    Arc::new(KernelDir)
}
vfs::procdir!("/proc/sys/kernel", kernel_dir);

/// `/proc/kallsyms`: `<address> t <name>` per function, address order; zeros unless the opener may see them.
struct Kallsyms;

/// Whether the opener may see kernel addresses, decided once at open (`kallsyms_open` with `file->f_cred`): a
/// privileged reader of an fd an unprivileged program opened (its stdin, say) must not reveal them.
fn show_for_opener(who: Cred) -> bool {
    match proc::current() {
        Some(p) => kallsyms_show(Some(&p)),
        // A kernel caller without a process (the ktests' dispatcher, kernel reads): the file credentials decide.
        None => who.uid == 0,
    }
}

impl FileSource for Kallsyms {
    fn bind(&self, who: Cred) -> Result<Option<Arc<dyn FileSource>>, Errno> {
        Ok(Some(Arc::new(KallsymsOpen { show: show_for_opener(who) })))
    }
    fn render(&self, who: Cred) -> Result<Vec<u8>, Errno> {
        render_kallsyms(show_for_opener(who))
    }
}

/// One open of /proc/kallsyms with its decision.
struct KallsymsOpen {
    show: bool,
}

impl FileSource for KallsymsOpen {
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        render_kallsyms(self.show)
    }
}

fn render_kallsyms(show: bool) -> Result<Vec<u8>, Errno> {
    let Some((t, _)) = kexec::crash::symbols() else { return Ok(Vec::new()) };
    let mut s = String::new();
    s.try_reserve(t.len() * 64).map_err(|_| ENOMEM)?;
    for i in 0..t.len() {
        if let Some((a, n)) = t.get(i) {
            let _ = writeln!(s, "{:016x} t {n}", if show { a } else { 0 });
        }
    }
    Ok(s.into_bytes())
}

struct ProcRoot;

impl DirSource for ProcRoot {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(alloc::vec![SynthEntry::file("kallsyms", 0o444, Arc::new(Kallsyms)).cookie(COOKIE)])
    }
}

fn proc_root() -> Arc<dyn DirSource> {
    Arc::new(ProcRoot)
}
vfs::procdir!("/proc", proc_root);

/// `/proc/bitsos/perf`: the perf events' counters (`name value` lines), so the profiler can write its own cost
/// beside its profile (the sampling interrupt's time is not visible in the samples themselves).
fn counters() -> String {
    let mut s = String::new();
    for (n, c) in [
        ("events_opened", &EVENTS),
        ("samples", &SAMPLES),
        ("samples_excluded", &SAMPLES_SKIPPED),
        ("records_lost", &LOST),
        ("side_band_records", &SIDE_BAND),
        ("wakeups", &WAKEUPS),
        ("sampler_ticks", &TICKS),
        ("sampler_ns", &TICK_NS),
    ] {
        let _ = writeln!(s, "{n} {}", c.get());
    }
    s
}
vfs::procfile!("/proc/bitsos/perf", counters);
