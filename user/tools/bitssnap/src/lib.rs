//! bitssnap (VS-2, V2-PLAN §1a B1): snapshots of /data. The kernel freezes ext4, snapshots its bitsvol volume and
//! thaws (crates/snapshots, `/dev/bitssnap`); this tool schedules them (`serve`, started by init), keeps them by
//! the retention rules (`retention`, settings `snap.*`), lists them and brings a file or folder back
//! (`restore`). VS-4 builds it for CachyOS too (the rescue side).
#![forbid(unsafe_code)]

pub mod dev;
pub mod restore;
pub mod restore_tool;
pub mod retention;
pub mod sched;
pub mod serve;
pub mod settings;
pub mod space;

use std::io::Write;

/// One line to the kernel log (`/dev/kmsg`: MCP `log` and the mirror see it, level Info), else to stderr.
pub fn log(msg: &str) {
    kmsg("", msg);
}

/// `log` at level Warn: the `<4>` priority prefix as Linux's `devkmsg_emit` reads it (crates/vfs-sys/src/kmsg.rs).
pub fn log_warn(msg: &str) {
    kmsg("<4>", msg);
}

fn kmsg(prio: &str, msg: &str) {
    let line = format!("bitssnap: {msg}\n");
    let sent = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").and_then(|mut f| f.write_all(format!("{prio}{line}").as_bytes())).is_ok();
    if !sent {
        eprint!("{line}");
    }
}

/// `YYYY-MM-DD HH:MM` of UTC seconds `t` shifted by `off_s` (`-` for an unknown time).
pub fn local_time(t: u64, off_s: i64) -> String {
    if t == 0 {
        return "-".into();
    }
    let s = (t as i64 + off_s).max(0);
    let (days, secs) = (s.div_euclid(86400), s.rem_euclid(86400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, secs % 3600 / 60)
}

/// `12.3 MB` style sizes.
pub fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn times_and_sizes() {
        assert_eq!(super::local_time(1_792_368_000, 0), "2026-10-19 00:00");
        assert_eq!(super::local_time(1_792_368_000, 7200), "2026-10-19 02:00");
        assert_eq!(super::local_time(951_782_400, 0), "2000-02-29 00:00");
        assert_eq!(super::local_time(0, 0), "-");
        assert_eq!(super::human(999), "999 B");
        assert_eq!(super::human(12_345_678), "12.3 MB");
    }
}
