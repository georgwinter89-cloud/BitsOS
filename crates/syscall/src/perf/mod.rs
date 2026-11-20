//! SELFOPT-1: `perf_event_open(2)` for a sampling profiler (linux-7.3.1/kernel/events/core.c,
//! include/uapi/linux/perf_event.h; the ABI types are the `perf-abi` crate, shared with `bitsprof`).
//!
//! What is built (plan/spec/self-optimise.md has the reasons):
//! - Events: PERF_TYPE_SOFTWARE with CPU_CLOCK, TASK_CLOCK and DUMMY, per CPU (pid -1, cpu N) or per thread
//!   (pid 0 or a tid, cpu -1 or N). Every other type or config answers ENOENT, Linux's answer for an event no PMU
//!   knows (perf then falls back to cpu-clock by itself). No hardware counters: the AMD PMU is not driven.
//! - Sampling (`sampler`): one high-resolution timer per CPU, at the shortest period of its events; each expiry
//!   looks at the interrupted code (`hal::sampling`) and writes PERF_RECORD_SAMPLE with IP, TID, TIME, ADDR (0),
//!   ID, STREAM_ID, CPU, PERIOD, READ, IDENTIFIER and CALLCHAIN (kernel frames by frame pointers, the program's by
//!   frame pointers as far as its pages are present, PERF_CONTEXT_KERNEL/USER marks). A thread event samples on
//!   the expiries that find its thread running.
//! - The ring buffer (`ring`): `mmap` of 1 + 2^n pages, `perf_event_mmap_page` with data_head/data_tail, LOST
//!   records when the reader is behind, wake-ups by `wakeup_events` or the watermark (half the area by default).
//! - Side-band records (`side`): COMM and MMAP2 of a new image, MMAP2 of an mmap with PROT_EXEC, FORK, EXIT.
//! - ioctl ENABLE, DISABLE, RESET, REFRESH, PERIOD, ID, SET_OUTPUT, PAUSE_OUTPUT (and PERF_IOC_FLAG_GROUP),
//!   read(2) with every read_format, poll, close. Groups (group_fd) of these events.
//! - Permission as Linux: `kernel.perf_event_paranoid` (default 2, as CachyOS), CAP_PERFMON or CAP_SYS_ADMIN for
//!   CPU-wide events and kernel samples, ptrace access for another thread, the mlock budget for the buffer;
//!   `/proc/kallsyms` with `kernel.kptr_restrict` (default 1: addresses only for CAP_SYSLOG).

mod event;
mod ktests;
mod ktests_prog;
mod ktests_rules;
mod open;
mod procfs;
mod ring;
mod sampler;
mod side;

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

pub(crate) use side::mmapped;

klog::counter!(EVENTS, "perf.events_opened");
klog::counter!(SAMPLES, "perf.samples");
klog::counter!(SAMPLES_SKIPPED, "perf.samples_excluded");
klog::counter!(LOST, "perf.records_lost");
klog::counter!(SIDE_BAND, "perf.side_band_records");
klog::counter!(WAKEUPS, "perf.wakeups");
klog::counter!(TICKS, "perf.sampler_ticks");
// Time spent in the sampling interrupt (ns, all CPUs): the profiler's own cost, beside `TICKS`.
klog::counter!(TICK_NS, "perf.sampler_ns");
// Sampler expiries pushed to the next 1 ms window because the CPU-time share was used up.
klog::counter!(THROTTLED, "perf.throttled");

/// `kernel.perf_event_paranoid`.
pub(crate) static PARANOID: AtomicI64 = AtomicI64::new(perf_abi::DEFAULT_PARANOID);
/// `kernel.perf_event_max_sample_rate` (Hz).
pub(crate) static MAX_SAMPLE_RATE: AtomicU64 = AtomicU64::new(perf_abi::DEFAULT_MAX_SAMPLE_RATE);
/// `kernel.perf_event_mlock_kb`.
pub(crate) static MLOCK_KB: AtomicU64 = AtomicU64::new(perf_abi::DEFAULT_MLOCK_KB);
/// `kernel.perf_event_max_stack`.
pub(crate) static MAX_STACK: AtomicU64 = AtomicU64::new(perf_abi::DEFAULT_MAX_STACK);
/// `kernel.perf_event_max_contexts_per_stack`.
pub(crate) static MAX_CONTEXTS: AtomicU64 = AtomicU64::new(perf_abi::MAX_CONTEXTS_PER_STACK);
/// `kernel.perf_cpu_time_max_percent`: the share of each CPU the sampling interrupt may take (`sampler::fire`).
pub(crate) static CPU_TIME_MAX_PERCENT: AtomicU64 = AtomicU64::new(25);
/// `kernel.kptr_restrict`: 1, so the profiler running as root sees kernel addresses and nobody else does.
pub(crate) static KPTR_RESTRICT: AtomicI64 = AtomicI64::new(1);

/// CAP_SYSLOG and CAP_PERFMON (V2's `Cap` does not name them).
const CAP_SYSLOG: u32 = 34;
const CAP_PERFMON: u32 = 38;

/// `perfmon_capable()`: CAP_PERFMON or CAP_SYS_ADMIN in the initial user namespace.
pub(crate) fn perfmon_capable(p: &proc::Process) -> bool {
    let c = p.cred();
    proc::cred::ns_capable_raw(&c, proc::ns::user::init_user_ns(), CAP_PERFMON) || proc::cred::capable(&c, proc::cred::Cap::SysAdmin)
}

/// `perf_allow_kernel()`: kernel samples, kernel callchains.
pub(crate) fn allow_kernel(p: &proc::Process) -> bool {
    PARANOID.load(Ordering::Relaxed) <= 1 || perfmon_capable(p)
}

/// `perf_allow_cpu()`: CPU-wide events.
pub(crate) fn allow_cpu(p: &proc::Process) -> bool {
    PARANOID.load(Ordering::Relaxed) <= 0 || perfmon_capable(p)
}

/// `perf_event_max_sample_rate_handler`: no limit (and no write of the rate) at 0 or 100 percent.
pub(crate) fn throttling_off() -> bool {
    matches!(CPU_TIME_MAX_PERCENT.load(Ordering::Relaxed), 0 | 100)
}

/// `perf_is_paranoid()`.
pub(crate) fn is_paranoid() -> bool {
    PARANOID.load(Ordering::Relaxed) > -1
}

/// `kallsyms_show_value()` for the reader `p` (None: a kernel reader, who may).
pub(crate) fn kallsyms_show(p: Option<&proc::Process>) -> bool {
    let Some(p) = p else { return true };
    let syslog = || proc::cred::ns_capable_raw(&p.cred(), proc::ns::user::init_user_ns(), CAP_SYSLOG);
    match KPTR_RESTRICT.load(Ordering::Relaxed) {
        0 => PARANOID.load(Ordering::Relaxed) <= 1 || syslog(),
        1 => syslog(),
        _ => false,
    }
}
