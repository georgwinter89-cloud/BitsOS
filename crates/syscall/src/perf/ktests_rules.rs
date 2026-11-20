//! SELFOPT-1 ktests for Linux's finer rules (the fixer round): /proc/kallsyms decides at open, SET_OUTPUT's and
//! the group's refusals, the order of open's checks, MODIFY_ATTRIBUTES, read-only data pages, lost side-band
//! records, and the sampler's bound for an unprivileged caller. Timing is not judged beyond the share of the
//! CPU the sampling interrupt may take, a limit the kernel itself keeps.

use super::ktests::{A, EXIT0, MS, SEC, T, expect, sleep_ms};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ktest::{TestResult, kassert, ktest};
use perf_abi::attr::flag;
use perf_abi::*;
use uapi::errno::*;
use uapi::nr;

const AT_FDCWD: i64 = -100;
const MAP_SHARED: u64 = 1;
const CLOCK_MONOTONIC_RAW: i32 = 4;
const CLOCK_BOOTTIME: i32 = 7;

/// open(path) in `t`, read up to `len` bytes through `reader`'s fd table (the same open file), the text.
fn read_through(opener: &mut T, reader: &mut T, path: &str, len: u64) -> Result<String, String> {
    let mut p = Vec::from(path.as_bytes());
    p.push(0);
    opener.put(0x800, &p);
    let at = opener.mem + 0x800;
    let fd = opener.call(nr::OPENAT, &[AT_FDCWD as u64, at, 0, 0]);
    kassert!(fd >= 0, "open {path}: {fd}");
    let f = opener.p.fds.get(fd as usize).map_err(|e| format!("fd {fd}: {e}"))?;
    let rfd = reader.p.fds.install_flags(f, 0, false).map_err(|e| format!("install: {e}"))?;
    let buf = reader.mem + 0x2000;
    let n = reader.call(nr::READ, &[rfd as u64, buf, len]);
    kassert!(n > 0, "read {path}: {n}");
    reader.call(nr::CLOSE, &[rfd as u64]);
    opener.close(fd);
    Ok(String::from_utf8_lossy(&reader.get(0x2000, n as usize)).into_owned())
}

/// Lines whose address is not zero.
fn shown(text: &str) -> usize {
    text.lines().filter(|l| l.len() > 16 && l.as_bytes()[..16].iter().any(|&b| b != b'0')).count()
}

// `kallsyms_open` judges the opener once: root reading an fd an unprivileged program opened sees zeros.
ktest!("so.perf.kallsyms_opener", Safe, kallsyms_opener);
fn kallsyms_opener() -> TestResult {
    if kexec::crash::symbols().is_none() {
        return ktest::fail("this boot has no kernel symbol table (boot/kernel.syms)");
    }
    let mut user = T::as_uid("so-perf-ks-user", 1000)?;
    let mut root = T::new("so-perf-ks-root")?;
    let text = read_through(&mut user, &mut root, "/proc/kallsyms", 8192)?;
    kassert!(text.lines().count() > 10 && shown(&text) == 0, "root read kernel addresses through uid 1000's open:\n{}", &text[..text.len().min(400)]);
    let mut root2 = T::new("so-perf-ks-root2")?;
    let text = read_through(&mut root, &mut root2, "/proc/kallsyms", 8192)?;
    kassert!(shown(&text) > 0, "root's own open shows no addresses");
    let text = read_through(&mut root, &mut user, "/proc/kallsyms", 8192)?;
    kassert!(shown(&text) > 0, "uid 1000 reading root's open lost the addresses (the opener decides, as Linux)");
    Ok(())
}

/// `perf_event_set_output` on `fd` toward `to` (None: -1), as the ioctl calls it (the ioctl finds `to` in the
/// current process's fd table, which a ktest's dispatcher has not; abi-perf runs the ioctl itself).
fn set_output(t: &T, fd: i64, to: Option<i64>) -> Result<i64, String> {
    let ev = t.ev(fd)?;
    let to = match to {
        Some(f) => Some(t.ev(f)?),
        None => None,
    };
    Ok(ev.set_output(to.as_ref()).map_or_else(|e| -e, |()| 0))
}

// `perf_event_set_output` (core.c:13715) and the group's clock rule.
ktest!("so.perf.set_output_rules", Safe, set_output_rules);
fn set_output_rules() -> TestResult {
    let mut t = T::new("so-perf-out")?;
    let a = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ | flag::DISABLED, ..Default::default() }.bytes();
    let cpu0 = t.open(&a, -1, 0, -1, 0);
    kassert!(cpu0 >= 0, "cpu-clock on CPU 0: {cpu0}");
    t.ev(cpu0)?.map(0, 3).map_err(|e| format!("ring: {e}"))?;
    expect("SET_OUTPUT to itself", set_output(&t, cpu0, Some(cpu0))?, -EINVAL)?;
    let mapped = t.open(&a, -1, 0, -1, 0);
    t.ev(mapped)?.map(0, 3).map_err(|e| format!("ring: {e}"))?;
    expect("SET_OUTPUT of an event with its own mapping", set_output(&t, mapped, Some(cpu0))?, -EINVAL)?;
    expect("SET_OUTPUT -1 of an event with its own mapping", set_output(&t, mapped, None)?, -EINVAL)?;
    // A thread event pinned to CPU 0 may write into CPU 0's CPU-wide buffer (Linux asks for the same cpu only).
    let th = t.open(&a, 0, 0, -1, 0);
    kassert!(th >= 0, "a thread event on CPU 0: {th}");
    expect("a thread event on CPU 0 into CPU 0's buffer", set_output(&t, th, Some(cpu0))?, 0)?;
    let floating = t.open(&a, 0, -1, -1, 0);
    expect("a thread event on any CPU into CPU 0's buffer", set_output(&t, floating, Some(cpu0))?, -EINVAL)?;
    // Two clocks V2 serves from one source are still two clocks for Linux.
    let raw = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ | flag::DISABLED | flag::USE_CLOCKID, clockid: CLOCK_MONOTONIC_RAW, ..Default::default() }.bytes();
    let boot = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ | flag::DISABLED | flag::USE_CLOCKID, clockid: CLOCK_BOOTTIME, ..Default::default() }.bytes();
    let r = t.open(&raw, -1, 0, -1, 0);
    kassert!(r >= 0, "MONOTONIC_RAW event: {r}");
    t.ev(r)?.map(0, 3).map_err(|e| format!("ring: {e}"))?;
    let b = t.open(&boot, -1, 0, -1, 0);
    kassert!(b >= 0, "BOOTTIME event: {b}");
    expect("BOOTTIME records into a MONOTONIC_RAW buffer", set_output(&t, b, Some(r))?, -EINVAL)?;
    expect("a BOOTTIME member of a MONOTONIC_RAW group", t.open(&boot, -1, 0, r, 0), -EINVAL)?;
    let raw2 = t.open(&raw, -1, 0, -1, 0);
    expect("the same clock id", set_output(&t, raw2, Some(r))?, 0)?;
    for fd in [cpu0, mapped, th, floating, r, b, raw2] {
        t.close(fd);
    }
    Ok(())
}

// Open's order (permission before the group's rules and V2's own refusals), pinned members, MODIFY_ATTRIBUTES.
ktest!("so.perf.open_rules", Safe, open_rules);
fn open_rules() -> TestResult {
    let mut t = T::new("so-perf-rules")?;
    let mut user = T::as_uid("so-perf-rules-user", 1000)?;
    let plain = A { flags: flag::DISABLED, ..Default::default() }.bytes();
    let lead = t.open(&plain, -1, 0, -1, 0);
    kassert!(lead >= 0, "leader: {lead}");
    let pinned = A { flags: flag::DISABLED | flag::PINNED, ..Default::default() }.bytes();
    expect("a pinned group member", t.open(&pinned, -1, 0, lead, 0), -EINVAL)?;
    let excl = A { flags: flag::DISABLED | flag::EXCLUSIVE, ..Default::default() }.bytes();
    expect("an exclusive group member", t.open(&excl, -1, 0, lead, 0), -EINVAL)?;
    let cs = A { flags: flag::EXCLUDE_KERNEL | flag::EXCLUDE_HV | flag::CONTEXT_SWITCH, ..Default::default() }.bytes();
    expect("unprivileged CPU-wide with context_switch", user.open(&cs, -1, 0, -1, 0), -EACCES)?;
    expect("root CPU-wide with context_switch (not built)", t.open(&cs, -1, 0, -1, 0), -EOPNOTSUPP)?;
    // MODIFY_ATTRIBUTES: the argument is read first; then only breakpoints could change. A clock has a type of
    // its own in Linux (EINVAL for any type), a dummy keeps PERF_TYPE_SOFTWARE (EOPNOTSUPP).
    t.put(0x400, &A::default().bytes());
    expect("MODIFY_ATTRIBUTES of a cpu-clock", t.ioctl(lead, IOC_MODIFY_ATTRIBUTES, t.mem + 0x400), -EINVAL)?;
    let dummy = t.open(&A { config: SW_DUMMY, ..Default::default() }.bytes(), -1, 0, -1, 0);
    kassert!(dummy >= 0, "dummy: {dummy}");
    t.put(0x400, &A { config: SW_DUMMY, ..Default::default() }.bytes());
    expect("MODIFY_ATTRIBUTES of a dummy", t.ioctl(dummy, IOC_MODIFY_ATTRIBUTES, t.mem + 0x400), -EOPNOTSUPP)?;
    t.put(0x400, &A { kind: PERF_TYPE_HARDWARE, ..Default::default() }.bytes());
    expect("MODIFY_ATTRIBUTES of a dummy with another type", t.ioctl(dummy, IOC_MODIFY_ATTRIBUTES, t.mem + 0x400), -EINVAL)?;
    t.close(dummy);
    expect("MODIFY_ATTRIBUTES with a bad pointer", t.ioctl(lead, IOC_MODIFY_ATTRIBUTES, 8), -EFAULT)?;
    let mut big = [0u8; 160];
    big[..attr::SIZE].copy_from_slice(&A::default().bytes());
    big[4..8].copy_from_slice(&160u32.to_le_bytes());
    big[150] = 1;
    t.put(0x400, &big);
    expect("MODIFY_ATTRIBUTES with unknown bytes", t.ioctl(lead, IOC_MODIFY_ATTRIBUTES, t.mem + 0x400), -E2BIG)?;
    t.close(lead);
    Ok(())
}

// `perf_mmap_pfn_mkwrite`: only the control page takes stores; a data page is SIGBUS (here: the fault fails).
ktest!("so.perf.data_pages_read_only", Safe, data_pages_read_only);
fn data_pages_read_only() -> TestResult {
    let mut t = T::new("so-perf-ro")?;
    let a = A { sample: 1000, sample_type: SAMPLE_IP, flags: flag::FREQ | flag::DISABLED, ..Default::default() }.bytes();
    let fd = t.open(&a, -1, 0, -1, 0);
    kassert!(fd >= 0, "cpu-clock: {fd}");
    let at = t.call(nr::MMAP, &[0, 3 * 4096, 3, MAP_SHARED, fd as u64, 0]);
    kassert!(at > 0, "1 + 2 pages, PROT_READ|PROT_WRITE: {at}");
    let at = at as u64;
    kassert!(t.p.mm.copy_to_user(at + 2 * 4096, &[1]).is_err(), "a store to an untouched data page went through");
    let mut b = [0u8; 8];
    kassert!(t.p.mm.copy_from_user(&mut b, at + 4096).is_ok(), "a data page cannot be read");
    kassert!(t.p.mm.copy_to_user(at + 4096, &[1]).is_err(), "a store to a data page mapped for reading went through");
    kassert!(t.p.mm.copy_to_user(at + page::DATA_TAIL as u64, &0u64.to_le_bytes()).is_ok(), "the control page takes no store");
    t.close(fd);
    Ok(())
}

// `__perf_output_begin`: a side-band record that cannot be written counts in `lost_samples` (FORMAT_LOST) too.
ktest!("so.perf.lost_side_band", Safe, lost_side_band);
fn lost_side_band() -> TestResult {
    let mut t = T::new("so-perf-lostsb")?;
    let a = A { config: SW_DUMMY, read_format: FORMAT_LOST, flags: flag::COMM | flag::TASK | flag::MMAP | flag::MMAP2 | flag::SAMPLE_ID_ALL, ..Default::default() }.bytes();
    let mut fds = Vec::new();
    for c in 0..hal::percpu::cpu_count() {
        let fd = t.open(&a, -1, c as i64, -1, 0);
        kassert!(fd >= 0, "dummy on CPU {c}: {fd}");
        t.ev(fd)?.map(0, 3).map_err(|e| format!("ring: {e}"))?;
        expect("PAUSE_OUTPUT", t.ioctl(fd, IOC_PAUSE_OUTPUT, 1), 0)?;
        fds.push(fd);
    }
    let child = proc::spawn_image("so-perf-sbchild", &crate::ktests::elf(&EXIT0), &["so-perf-sbchild"], &["HOME=/"], policy::ProgramId(1)).map_err(|e| format!("spawn: {e:?}"))?;
    let _ = child.wait(Some(10 * SEC));
    sleep_ms(10);
    let mut lost = 0;
    for fd in fds {
        let (r, w) = t.read(fd, 16);
        kassert!(r == 16, "read: {r}");
        lost += w[1];
        t.close(fd);
    }
    kassert!(lost >= 3, "a new program's COMM, MMAP2 and EXIT into paused buffers count {lost} lost records");
    Ok(())
}

// The sampler's bound: an unprivileged caller's 10 µs task-clock on itself plus 100 copies (its thread never
// runs) must not drive every CPU's timer at 100 kHz; the interrupt keeps to `perf_cpu_time_max_percent`.
ktest!("so.perf.unprivileged_bound", Safe, unprivileged_bound);
fn unprivileged_bound() -> TestResult {
    let mut t = T::as_uid("so-perf-bound", 1000)?;
    let a = A { config: SW_TASK_CLOCK, sample: 10_000, sample_type: SAMPLE_IP, flags: flag::EXCLUDE_KERNEL | flag::EXCLUDE_HV, ..Default::default() }.bytes();
    let mut fds = Vec::new();
    for _ in 0..101 {
        let fd = t.open(&a, 0, -1, -1, 0);
        kassert!(fd >= 0, "unprivileged 10 µs task-clock on itself: {fd}");
        fds.push(fd);
    }
    let cpus = hal::percpu::cpu_count();
    let before: Vec<(u64, u64)> = (0..cpus).map(super::sampler::cpu_cost).collect();
    let t0 = time::now_ns();
    sleep_ms(200);
    let dt = time::now_ns() - t0;
    let after: Vec<(u64, u64)> = (0..cpus).map(super::sampler::cpu_cost).collect();
    for fd in fds {
        t.close(fd);
    }
    let percent = super::CPU_TIME_MAX_PERCENT.load(Ordering::Relaxed);
    // The timer runs at most at the spread rate; the share may pass the limit by one pass per window (+10).
    let max_fires = dt / super::sampler::spread_period() + 2;
    for c in 0..cpus {
        let (fires, ns) = (after[c].0 - before[c].0, after[c].1 - before[c].1);
        klog::event!(Info, "ktest", "so.perf.bound", cpu = c, fires = fires, busy_ns = ns, window_ns = dt);
        kassert!(fires <= max_fires * 5 / 4, "CPU {c}: {fires} sampler interrupts in {} ms (at most {max_fires})", dt / MS);
        kassert!(ns * 100 <= dt * (percent + 10), "CPU {c}: the sampler took {ns} ns of {dt} ns, more than {percent} % (+10)");
    }
    Ok(())
}
