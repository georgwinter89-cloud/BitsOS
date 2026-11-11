//! W35 MM-3: `/proc/vmstat` (Linux 7.x mm/vmstat.c `vmstat_text`, `vmstat_show`: "name value\n" per line in the
//! table's order). BitsOS shows the lines it has a meaning for, in Linux's order and under Linux's names: the free
//! and file pages, the pages reclaim took back (kswapd and direct), the compaction events and the THP fault and split
//! events. Tools read the names they know and treat a missing one as 0 (procps `vmstat`, `sar`).

use alloc::string::String;
use core::fmt::Write;

/// (Linux name, BitsOS counter) in `vmstat_text` order; the first two are computed (`render_vmstat`).
const LINES: [(&str, &str); 17] = [
    ("nr_free_pages", ""),
    ("nr_file_pages", ""),
    ("pgsteal_kswapd", "vfs.reclaim.background_pages"),
    ("pgsteal_direct", "vfs.reclaim.direct_pages"),
    ("compact_migrate_scanned", "mm.compact.migrate_scanned"),
    ("compact_free_scanned", "mm.compact.free_scanned"),
    ("compact_isolated", "mm.compact.isolated"),
    ("compact_stall", "mm.compact.stall"),
    ("compact_fail", "mm.compact.fail"),
    ("compact_success", "mm.compact.success"),
    ("compact_daemon_wake", "mm.compact.daemon_wake"),
    ("compact_daemon_migrate_scanned", "mm.compact.daemon_migrate_scanned"),
    ("compact_daemon_free_scanned", "mm.compact.daemon_free_scanned"),
    ("thp_fault_alloc", "mm.thp.fault_alloc"),
    ("thp_fault_fallback", "mm.thp.fault_fallback"),
    ("thp_split_page", "mm.thp.split_page"),
    ("thp_split_pmd", "mm.thp.split_pmd"),
];

/// The text from the values of `LINES` (same order).
pub(super) fn vmstat(values: &[u64; LINES.len()]) -> String {
    let mut s = String::new();
    for ((name, _), v) in LINES.iter().zip(values) {
        let _ = writeln!(s, "{name} {v}");
    }
    s
}

fn render_vmstat() -> String {
    let mut values = [0u64; LINES.len()];
    values[0] = super::buddyinfo::zones().iter().map(|z| z.free_frames).sum();
    values[1] = vfs::reclaim::file_pages();
    for (name, v) in klog::metrics::snapshot() {
        if let Some(i) = LINES.iter().position(|&(_, k)| !k.is_empty() && k == name) {
            values[i] = v;
        }
    }
    vmstat(&values)
}
vfs::procfile!("/proc/vmstat", render_vmstat);

/// One /proc/vmstat value (0 when the line is missing).
fn vmstat_value(ns: &vfs::MountNs, name: &str) -> Result<u64, String> {
    let text = ns.read_file("/proc/vmstat", 1 << 16).map_err(|e| alloc::format!("/proc/vmstat: {e}"))?;
    let text = core::str::from_utf8(&text).map_err(|_| String::from("/proc/vmstat is not text"))?;
    Ok(text.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok()).unwrap_or(0))
}

/// Writes `data` to the existing file `path` through the VFS (as a program's write(2) would).
fn write_existing(ns: &vfs::MountNs, path: &str, data: &[u8]) -> Result<usize, super::Errno> {
    ns.open_flags(path.as_bytes(), 1, 0)?.write(data)
}

/// W35 MM-3 through the files a program sees: writing 1 to /proc/sys/vm/compact_memory runs a whole-memory
/// compaction (compact_migrate_scanned in /proc/vmstat grows), 2 is EINVAL; the defrag file takes `never` and shows
/// it in brackets, refuses a word it does not know, and gets its old mode back. QEMU only: compact_memory drops
/// clean cache on the machine it runs on and the defrag mode is global.
fn compact_procfs() -> ktest::TestResult {
    let ns = vfs::root_ns();
    let before = vmstat_value(&ns, "compact_migrate_scanned")?;
    ktest::kassert!(write_existing(&ns, "/proc/sys/vm/compact_memory", b"1\n").is_ok(), "writing 1 to compact_memory");
    let after = vmstat_value(&ns, "compact_migrate_scanned")?;
    ktest::kassert!(after > before, "compact_memory scanned nothing: compact_migrate_scanned {before} -> {after}");
    ktest::kassert!(write_existing(&ns, "/proc/sys/vm/compact_memory", b"2\n").is_err(), "compact_memory took 2");
    let old = mm::thp::defrag();
    let r = (|| -> ktest::TestResult {
        let path = "/sys/kernel/mm/transparent_hugepage/defrag";
        ktest::kassert!(write_existing(&ns, path, b"never\n").is_ok(), "writing never to defrag");
        let text = ns.read_file(path, 4096).map_err(|e| alloc::format!("{path}: {e}"))?;
        ktest::kassert!(text == b"always defer defer+madvise madvise [never]\n", "defrag reads {:?}", String::from_utf8_lossy(&text));
        ktest::kassert!(write_existing(&ns, path, b"sometimes\n").is_err(), "defrag took an unknown word");
        ktest::kassert!(mm::thp::defrag() == mm::thp::Defrag::Never, "the refused write changed the mode");
        Ok(())
    })();
    mm::thp::set_defrag(old);
    klog::event!(Info, "mm", "ktest.compact_procfs", migrate_scanned = after - before);
    r
}
ktest::ktest!("mm.compact.procfs", QemuOnly, compact_procfs);

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of CachyOS's /proc/vmstat (Linux 7.3.1 on the gaming PC, 6.11.2026), in its order: BitsOS's lines
    /// are a subsequence of it, under the same names.
    const CACHYOS: &[&str] = &[
        "nr_free_pages", "nr_free_pages_blocks", "nr_file_pages", "pgdemote_kswapd", "pgsteal_kswapd", "pgsteal_direct", "pgsteal_khugepaged",
        "pgscan_kswapd", "pgfree", "pgfault", "drop_pagecache", "thp_migration_success", "compact_migrate_scanned", "compact_free_scanned",
        "compact_isolated", "compact_stall", "compact_fail", "compact_success", "compact_daemon_wake", "compact_daemon_migrate_scanned",
        "compact_daemon_free_scanned", "thp_fault_alloc", "thp_fault_fallback", "thp_fault_fallback_charge", "thp_split_page",
        "thp_split_page_failed", "thp_split_pmd", "thp_split_pud",
    ];

    #[test]
    fn lines_are_linux_names_in_linux_order() {
        let mut at = 0;
        for (name, _) in LINES {
            let i = CACHYOS.iter().position(|&n| n == name).unwrap_or_else(|| panic!("{name} is not a Linux vmstat name"));
            assert!(i >= at, "{name} out of Linux's order");
            at = i;
        }
    }

    #[test]
    fn format_is_name_space_value_newline() {
        let mut v = [0u64; LINES.len()];
        v[0] = 123_456;
        v[7] = 3;
        let s = vmstat(&v);
        let mut l = s.lines();
        assert_eq!(l.next(), Some("nr_free_pages 123456"));
        assert_eq!(s.lines().nth(7), Some("compact_stall 3"));
        assert_eq!(s.lines().count(), LINES.len());
        assert!(s.ends_with("thp_split_pmd 0\n"));
    }
}
