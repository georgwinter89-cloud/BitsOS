//! Small helpers: random ids, atomic file writes, dates without a date crate.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 16 random bytes from the kernel, Crockford base32 → 26 characters. Never derived from a path.
pub fn new_id(prefix: &str) -> String {
    let mut b = [0u8; 16];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // Fallback only if /dev/urandom is missing: time + pid + counter, still unique per process.
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128;
        let v = t ^ ((std::process::id() as u128) << 64) ^ (n << 96);
        b = v.to_le_bytes();
    }
    let v = u128::from_le_bytes(b);
    let mut s = String::with_capacity(prefix.len() + 27);
    s.push_str(prefix);
    s.push('_');
    for i in 0..26 {
        let idx = (v >> (125 - i * 5)) & 31;
        s.push(CROCKFORD[idx as usize] as char);
    }
    s
}

static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `bytes` to `path` through a temp file and a rename; with `sync` the file and the directory are
/// fsynced, so a crash leaves the old or the new content, never a torn one.
pub fn write_atomic(path: &Path, bytes: &[u8], sync: bool) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("no parent"))?;
    fs::create_dir_all(dir)?;
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp.{}.{n}", std::process::id()));
    {
        let mut f = OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(bytes)?;
        if sync {
            f.sync_all()?;
        }
    }
    fs::rename(&tmp, path)?;
    if sync {
        sync_dir(dir)?;
    }
    Ok(())
}

pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// fsync every file below `dir` and the directories themselves (used before a generation commit).
pub fn sync_tree(dir: &Path) -> io::Result<()> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)? {
            let e = e?;
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(e.path());
            } else if ft.is_file() {
                File::open(e.path())?.sync_all()?;
            }
        }
        sync_dir(&d)?;
    }
    Ok(())
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2025-03-01T12:00:00Z` from seconds since the epoch.
pub fn iso_time(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, (s / 60) % 60, s % 60)
}

/// Parses `YYYY-MM-DD` (optionally followed by a time part, which is ignored) → seconds at 00:00 UTC.
pub fn parse_iso_date(s: &str) -> Option<i64> {
    let s = s.trim();
    let date = s.get(..10)?;
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

/// Cut a string to at most `max` bytes on a char boundary.
pub fn cut_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Create `dir` (and missing parents of it) with mode 0700; an existing folder that others may read is
/// narrowed to 0700 and the change logged.
pub fn private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !dir.is_dir() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        crate::event!("warn", "policy.narrowed", what = "data_folder", mode_before = format!("{mode:o}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_random_and_shaped() {
        let a = new_id("d");
        let b = new_id("d");
        assert_ne!(a, b);
        assert_eq!(a.len(), 28);
        assert!(a.starts_with("d_"));
        assert!(a[2..].bytes().all(|c| CROCKFORD.contains(&c)));
    }

    #[test]
    fn dates_round_trip() {
        for (s, secs) in [("1970-01-01", 0), ("2025-03-01", 1_740_787_200), ("2000-02-29", 951_782_400)] {
            assert_eq!(parse_iso_date(s), Some(secs));
            assert_eq!(&iso_time(secs)[..10], s);
        }
        assert_eq!(parse_iso_date("2025-13-01"), None);
        assert_eq!(parse_iso_date("bogus"), None);
    }

    #[test]
    fn cut_respects_char_boundaries() {
        assert_eq!(cut_bytes("Grüße", 3), "Gr");
        assert_eq!(cut_bytes("abc", 10), "abc");
    }
}
