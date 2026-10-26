//! The whole /data volume back to a snapshot, and "everything as at 15:00" (VS-3, B1). Both happen at the next
//! start (or Reload): the kernel switches the volume's root before ext4 mounts it and keeps the state of that
//! moment as the snapshot "before-rollback", so a rollback is undone by a rollback to that snapshot (or, before
//! the start, by cancelling it). A switch under the mounted /data is not possible: ext4's caches and journal and
//! every program's open files would still show the old tree (crates/snapshots/src/lib.rs `schedule_rollback`).
//! The time restore is the kernel's (`restore-at`): /data's snapshot of then plus the system generation of then
//! (RB-9's `system_pin`), all or nothing; the console's `restore_at` is the same call.

use crate::dev::{Dev, RestoreAt};

/// What a volume rollback request did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rollback {
    Scheduled { to: u64, replaced: Option<u64> },
    Cancelled(Option<u64>),
    Pending(Option<u64>),
}

pub fn schedule(dev: &mut Dev, id: u64) -> Result<Rollback, String> {
    let replaced = dev.rollback(id).map_err(|e| format!("rollback to snapshot {id}: {e}"))?;
    crate::log(&format!("/data goes back to snapshot {id} at the next start (replaced: {})", replaced.map_or("none".into(), |r| r.to_string())));
    Ok(Rollback::Scheduled { to: id, replaced: replaced.filter(|&r| r != id) })
}

pub fn cancel(dev: &mut Dev) -> Result<Rollback, String> {
    let c = dev.rollback_cancel().map_err(|e| format!("rollback cancel: {e}"))?;
    if let Some(id) = c {
        crate::log(&format!("the rollback to snapshot {id} is cancelled"));
    }
    Ok(Rollback::Cancelled(c))
}

pub fn status(dev: &mut Dev) -> Result<Rollback, String> {
    dev.rollback_pending().map(Rollback::Pending).map_err(|e| format!("rollback: {e}"))
}

pub fn describe(r: &Rollback) -> String {
    match r {
        Rollback::Scheduled { to, replaced } => format!(
            "/data goes back to snapshot {to} at the next start (reboot, or a Reload); the state then is kept as snapshot \"before-rollback\", a rollback to it undoes this{}",
            replaced.map_or(String::new(), |r| format!(" (replaces the rollback to {r})"))
        ),
        Rollback::Cancelled(Some(id)) => format!("the rollback to snapshot {id} is cancelled: the next start keeps /data as it is"),
        Rollback::Cancelled(None) => "no rollback was scheduled".into(),
        Rollback::Pending(Some(id)) => format!("/data goes back to snapshot {id} at the next start"),
        Rollback::Pending(None) => "no rollback scheduled".into(),
    }
}

/// `YYYY-MM-DD HH:MM[:SS]` in the local time `bitssnap list` and the MCP tool `snapshots` show (UTC offset
/// `off_min`, the kernel's now), or plain UTC seconds -> UTC seconds. Without seconds the whole minute counts
/// (its last second): a snapshot shown as "15:00" is one of "as at 15:00".
pub fn parse_at(s: &str, off_min: i64) -> Result<i64, String> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse().map_err(|_| format!("{s}: too large"));
    }
    let bad = || format!("{s:?}: want YYYY-MM-DD HH:MM (local time) or UTC seconds");
    let (date, time) = s.split_once([' ', 'T']).ok_or_else(bad)?;
    let d: Vec<i64> = date.split('-').map(|x| x.parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
    let t: Vec<i64> = time.split(':').map(|x| x.parse().map_err(|_| bad())).collect::<Result<_, _>>()?;
    let ([y, m, day], [h, min, sec @ ..]) = (d.as_slice(), t.as_slice()) else { return Err(bad()) };
    let sec = match sec {
        [] => 59,
        [x] => *x,
        _ => return Err(bad()),
    };
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1970..=9999).contains(y) || !(1..=12).contains(m) || *day < 1 || *day > mdays[(*m - 1) as usize] || *h > 23 || *min > 59 || !(0..=59).contains(&sec) {
        return Err(bad());
    }
    // Howard Hinnant's days_from_civil (as lib.rs local_time's inverse).
    let (y2, m2) = if *m <= 2 { (y - 1, m + 9) } else { (*y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400 + h * 3600 + min * 60 + sec - off_min * 60)
}

/// "Everything as at `utc_s`" (`dry`: what would happen).
pub fn restore_system(dev: &mut Dev, utc_s: i64, dry: bool) -> Result<RestoreAt, String> {
    let r = dev.restore_at(utc_s, dry).map_err(|e| format!("restore-at: {e}"))?;
    if !r.ok {
        return Err(r.note);
    }
    if !dry {
        crate::log(&format!("everything as at {utc_s}: /data to snapshot {}, {}", r.snapshot, r.note));
    }
    Ok(r)
}

/// The answer of a system restore in words (`off_s`: the UTC offset of the times shown).
pub fn system_text(r: &RestoreAt, dry: bool, off_s: i64) -> String {
    let when = crate::local_time(r.snapshot_ns / 1_000_000_000, off_s);
    let mut t = format!("/data: snapshot {} of {when}; system: {}", r.snapshot, r.note);
    if dry {
        t.push_str(". Dry run: nothing changed.");
        return t;
    }
    if r.replaced != 0 && r.replaced != r.snapshot {
        t.push_str(&format!(" (replaces the rollback to {})", r.replaced));
    }
    let now = if r.pin != 0 { format!("system_pin {{\"n\":{},\"now\":true}} (a Reload) or reboot", r.pin) } else { "reboot".into() };
    t.push_str(&format!(
        ". Both at the next start; at once: {now}. Undo before that: restore kind system with cancel=true; after it: a rollback to the \"before-rollback\" snapshot plus system_pin off. Should /data's rollback fail at the start (a full pool), the generation still changes: tool snapshots then shows rollback_not_done."
    ));
    t
}

/// The undo of a system restore before the next start: the kernel takes back the /data rollback and the PIN
/// of this start's last restore-at, each only while it is still the one restore-at set, and puts back the PIN
/// from before it. A stray or repeated undo changes nothing (`Ok((false, why))`): a PIN Georg set stays.
pub fn undo_system(dev: &mut Dev) -> Result<(bool, String), String> {
    let r = dev.restore_at_cancel().map_err(|e| format!("restore-at cancel: {e}"))??;
    if r.0 {
        crate::log(&format!("the restore-at of this start is undone: {}", r.1));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        // 2026-10-23 15:00 in Berlin (UTC+2 in summer time) is 13:00 UTC.
        let t = parse_at("2026-10-23 15:00:00", 120).unwrap();
        assert_eq!(t, 1_792_760_400);
        assert_eq!(parse_at("2026-10-23 15:00", 120), Ok(t + 59), "the whole minute");
        assert_eq!(crate::local_time(t as u64 + 59, 120 * 60), "2026-10-23 15:00");
        assert_eq!(parse_at("2026-10-23T15:00:30", 120), Ok(t + 30));
        assert_eq!(parse_at("1792760400", 120), Ok(1_792_760_400));
        assert_eq!(parse_at("2024-02-29 00:00:00", 0).map(|t| crate::local_time(t as u64, 0)), Ok("2024-02-29 00:00".into()));
        for bad in ["", "15:00", "2026-10-23", "2026-13-01 10:00", "2025-02-29 10:00", "2026-10-23 24:00", "2026-10-23 10:60", "2026-10-23 10:00:00:00", "x-y-z 1:2", "1969-12-31 23:59"] {
            assert!(parse_at(bad, 0).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn texts() {
        let r = RestoreAt { ok: true, snapshot: 7, snapshot_ns: 1_792_760_400_000_000_000, pin: 1, running: 2, next: 2, replaced: 0, note: "generation 1 ran then: pinned for the next start".into() };
        let t = system_text(&r, false, 7200);
        assert!(t.starts_with("/data: snapshot 7 of 2026-10-23 15:00; system: generation 1 ran then"), "{t}");
        assert!(t.contains(r#"system_pin {"n":1,"now":true}"#) && t.contains("cancel=true"), "{t}");
        let d = system_text(&RestoreAt { pin: 0, ..r }, true, 7200);
        assert!(d.ends_with("Dry run: nothing changed.") && !d.contains("system_pin"), "{d}");
        assert!(describe(&Rollback::Scheduled { to: 5, replaced: Some(3) }).contains("snapshot 5 at the next start") );
        assert!(describe(&Rollback::Scheduled { to: 5, replaced: Some(3) }).contains("replaces the rollback to 3"));
        assert_eq!(describe(&Rollback::Pending(None)), "no rollback scheduled");
    }
}
