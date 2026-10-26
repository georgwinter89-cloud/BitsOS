//! Ring size and levels from CONFIG and from the machine's memory.

use crate::{EXPLICIT_SIZE, HEAP_READY, RING_MAX, RING_MIN, levels, resize, ring_info, ring_size, set_levels};
use alloc::string::String;
use core::sync::atomic::Ordering;

/// The ring size for a machine with `total_ram` bytes: RAM / 6144, so the Mini-PC's ~24 GB give 4 MiB, rounded
/// to 64 KiB, at least 1 MiB, at most 16 MiB.
pub fn ring_bytes_for_memory(total_ram: u64) -> usize {
    let b = (total_ram / 6144).clamp(1 << 20, 16 << 20) as usize;
    (b + 0xFFFF) & !0xFFFF
}

/// Sizes the ring from the machine's memory, unless CONFIG `log_buf=` chose a size.
pub fn size_for_memory(total_ram: u64) {
    if EXPLICIT_SIZE.load(Ordering::Acquire) || !HEAP_READY.load(Ordering::Acquire) {
        return;
    }
    let want = ring_bytes_for_memory(total_ram);
    if ring_info().bytes == ring_size(want) {
        return;
    }
    match resize(want) {
        Ok(bytes) => event!(Info, "klog", "ring.sized", bytes = bytes, ram_mb = total_ram >> 20),
        Err(e) => event!(Warn, "klog", "ring.size_failed", want = want, error = alloc::format!("{e:?}")),
    }
}

/// `4194304`, `512K`, `4M`, `1G` (also `KiB`/`MB`, any case) -> bytes.
pub fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n: usize = s[..digits].parse().ok()?;
    let unit = s[digits..].trim().to_ascii_lowercase();
    let shift = match unit.trim_end_matches("ib").trim_end_matches('b') {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        _ => return None,
    };
    n.checked_mul(1usize << shift)
}

/// Applies CONFIG `loglevel=` (`info` or `warn,net=debug`) and `log_buf=` (ring bytes). The kernel calls it
/// once CONFIG is read.
pub fn configure(get: &dyn Fn(&str) -> Option<String>) {
    if let Some(v) = get("loglevel")
        && let Err(bad) = set_levels(&v)
    {
        event!(Warn, "klog", "config.bad", key = "loglevel", part = bad; "keeping {}", levels::describe());
    }
    configure_rb7(get);
    if let Some(v) = get("log_buf") {
        match parse_size(&v).filter(|b| (RING_MIN..=RING_MAX).contains(b)) {
            Some(b) => {
                EXPLICIT_SIZE.store(true, Ordering::Release);
                match resize(b) {
                    Ok(bytes) => event!(Info, "klog", "ring.sized", bytes = bytes, source = "log_buf"),
                    Err(e) => event!(Warn, "klog", "ring.size_failed", want = b, error = alloc::format!("{e:?}")),
                }
            }
            None => event!(Warn, "klog", "config.bad", key = "log_buf", value = v; "want 64K..1G"),
        }
    }
}


/// RB-7a keys: `log_ratelimit=` (`off` or `<burst>/<rate>`), `trace=` (kinds), `trace_buf=` (bytes per CPU),
/// `trace_syscall_ms=`, `trace_lock_us=`, `trace_irq_rate=` (per s), `trace_faults=` (per 100 ms).
fn configure_rb7(get: &dyn Fn(&str) -> Option<String>) {
    if let Some(v) = get("log_ratelimit") {
        match crate::ratelimit::parse(&v) {
            Some((b, r)) => crate::ratelimit::set(b, r),
            None => event!(Warn, "klog", "config.bad", key = "log_ratelimit", value = v; "want off or burst/rate, e.g. 1000/100"),
        }
    }
    if let Some(v) = get("trace")
        && let Err(bad) = crate::trace::set_kinds(&v)
    {
        event!(Warn, "klog", "config.bad", key = "trace", part = bad; "want all, off or switch,irq,syscall,lock,fault,mark");
    }
    if let Some(v) = get("trace_buf") {
        match parse_size(&v).filter(|b| (4096..=(64 << 20)).contains(b)) {
            Some(b) => crate::trace::set_explicit_bytes(b),
            None => event!(Warn, "klog", "config.bad", key = "trace_buf", value = v; "want 4K..64M per CPU"),
        }
    }
    let mut l = crate::trace::limits();
    let keys: [(&str, &mut u64, u64); 4] = [
        ("trace_syscall_ms", &mut l.syscall_ns, 1_000_000),
        ("trace_lock_us", &mut l.lock_wait_ns, 1000),
        ("trace_irq_rate", &mut l.irq_per_s, 1),
        ("trace_faults", &mut l.faults_per_window, 1),
    ];
    for (key, field, unit) in keys {
        if let Some(v) = get(key) {
            match v.trim().parse::<u64>().ok().filter(|&n| n > 0).and_then(|n| n.checked_mul(unit)) {
                Some(n) => *field = n,
                None => event!(Warn, "klog", "config.bad", key = key, value = v; "want a positive number"),
            }
        }
    }
    if l != crate::trace::limits() {
        crate::trace::set_limits(l);
        event!(Info, "klog", "trace.limits", syscall_ns = l.syscall_ns, lock_wait_ns = l.lock_wait_ns, irq_per_s = l.irq_per_s, faults = l.faults_per_window);
    }
}
