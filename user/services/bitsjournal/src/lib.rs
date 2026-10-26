//! bitsjournal (RB-8, V2-PLAN §1a B6 "a system that explains itself"): keeps the kernel's events, trace entries
//! and metrics for weeks. It reads them from `/dev/bitsos/events` (crates/klogd/src/devevents.rs, one line per
//! item), writes them as zlib blocks to `/data/system/journal/` (`store`), deletes the oldest by the settings
//! `journal.keep_days` and `journal.max_mb` (`retention`), and answers queries by time, subsystem, name, level
//! and fields (`query`) through the MCP tool `journal`, which the kernel's `events` and `metrics` call when asked
//! with `since`/`until`. A Reload or reboot waits (bounded) until it has written the last records (`F`/`A`).
#![forbid(unsafe_code)]

pub mod fields;
pub mod line;
pub mod query;
pub mod serve;
pub mod settings;
pub mod store;

use std::io::Write;

/// The journal's folder (root only).
pub const DIR: &str = "/data/system/journal";
/// The kernel's reader.
pub const NODE: &str = "/dev/bitsos/events";

/// One line to the kernel log (`/dev/kmsg`), else to stderr.
pub fn log(msg: &str) {
    kmsg("", msg);
}

/// `log` at level Warn (`<4>`, as Linux's devkmsg_emit reads it).
pub fn log_warn(msg: &str) {
    kmsg("<4>", msg);
}

fn kmsg(prio: &str, msg: &str) {
    let line = format!("bitsjournal: {msg}\n");
    let sent = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").and_then(|mut f| f.write_all(format!("{prio}{line}").as_bytes())).is_ok();
    if !sent {
        eprint!("{line}");
    }
}

/// UTC milliseconds now (0 without a wall clock).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// `YYYY-MM-DD HH:MM:SS.mmm` of UTC milliseconds `ms` shifted by `off_min` minutes (`-` for 0).
pub fn local_time(ms: u64, off_min: i32) -> String {
    if ms == 0 {
        return "-".into();
    }
    let s = (ms / 1000) as i64 + off_min as i64 * 60;
    let (days, secs) = (s.div_euclid(86400), s.rem_euclid(86400));
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", secs / 3600, secs % 3600 / 60, secs % 60, ms % 1000)
}

/// Howard Hinnant's civil_from_days: (year, month, day) of a day number since 1970-01-01.
pub fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Where the kernel says what became of /data (`fs_ext4::data::STATE_FILE`).
pub const DATA_STATE: &str = "/run/bitsos/data-state";

#[derive(Debug, PartialEq, Eq)]
pub enum DataState {
    /// Mounted read-write (pool or plain): the journal can be written.
    Ready,
    /// The boot is still making or mounting it.
    Pending,
    /// No writable /data on this boot: the reason.
    Never(String),
}

/// bitssnap's reading of the state file (user/tools/bitssnap/src/serve.rs), except that a plain /data is
/// enough here: the journal needs a writable folder, not snapshots.
pub fn data_state(text: Option<&str>) -> DataState {
    let Some(t) = text.map(str::trim) else { return DataState::Pending };
    if t.starts_with("mounted pool rw") || t.starts_with("mounted plain rw") {
        DataState::Ready
    } else if t.is_empty() || t == "waiting" || t == "creating" {
        DataState::Pending
    } else {
        DataState::Never(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_states() {
        assert_eq!(local_time(1_792_368_000_123, 0), "2026-10-19 00:00:00.123");
        assert_eq!(local_time(1_792_368_000_000, 120), "2026-10-19 02:00:00.000");
        assert_eq!(local_time(0, 60), "-");
        assert_eq!(data_state(Some("mounted pool rw\n")), DataState::Ready);
        assert_eq!(data_state(Some("mounted plain rw\n")), DataState::Ready);
        assert_eq!(data_state(Some("waiting")), DataState::Pending);
        assert_eq!(data_state(None), DataState::Pending);
        assert!(matches!(data_state(Some("mounted pool ro")), DataState::Never(_)));
    }
}
