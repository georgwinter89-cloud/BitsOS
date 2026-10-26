//! VS-3's console commands: the restores that work without user space (Georg at the machine when /data's
//! programs or MCP do not come up). `snapshots` reads; `rollback` and `restore_at` change what the next start
//! brings, so they are `physical_only` (an injected line must not get around the `delete_overwrite` class of
//! the MCP tool `restore`, B5; RB-9 does the same for system_pin). File and program restores copy files as root
//! and belong to `bitssnap` (shell or MCP), not to the kernel.

use crate::{Target, list, pending_rollback, system};
use alloc::format;
use alloc::string::String;
use console::monitor::Ctx;

fn target() -> Result<Target, String> {
    Target::data().map_err(|e| format!("/data has no snapshots now (errno {e}: 19 not mounted, 95 a plain image, 30 read-only)"))
}

/// A local time the clock change makes doubtful (October's doubled hour, March's skipped one).
const DOUBLED: &str = "the clocks went back in that hour, so it came twice: the first (summer-time) pass is meant; for the second give UTC seconds (MCP restore kind system, or bitssnap restore-system)";
const SKIPPED: &str = "that local time did not exist (the clocks went forward): read with the standard offset, an hour later";

/// `YYYY-MM-DD HH:MM` local (CONFIG `tz=`) -> UTC seconds of the minute's last second (a snapshot shown as
/// "15:00" belongs to "as at 15:00"), plus a note when the clock change makes the time doubtful (pure,
/// host-tested).
pub fn parse_local(date: &str, hm: &str, tz: time::tz::Tz) -> Option<(i64, Option<&'static str>)> {
    let mut d = date.split('-').map(|x| x.parse::<u32>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = hm.split(':').map(|x| x.parse::<u32>().ok());
    let (h, min) = (t.next()??, t.next()??);
    if d.next().is_some() || t.next().is_some() || !(1..=12).contains(&m) || h > 23 || min > 59 {
        return None;
    }
    let dt = time::tz::DateTime { year: y, month: m as u8, day: u8::try_from(day).ok()?, hour: h as u8, min: min as u8, sec: 59 };
    if !dt.valid() {
        return None;
    }
    let (l, utc) = (dt.unix(), tz.to_utc(&dt));
    let off = |u: i64| i64::from(tz.offset_at(u)) * 60;
    // Another reading of the same local time: an offset of the hours around it that also maps back to it.
    let twice = [utc - 7200, utc + 7200].iter().map(|&u| off(u)).any(|o| o != off(utc) && off(l - o) == o);
    let note = if l - off(utc) != utc { Some(SKIPPED) } else if twice { Some(DOUBLED) } else { None };
    Some((utc, note))
}

fn local(ns: u64) -> String {
    if ns == 0 {
        return String::from("(no time)");
    }
    let utc = (ns / 1_000_000_000) as i64;
    kexec::install::system::date(utc, time::wall::tz().offset_at(utc))
}

fn console_snapshots(c: &mut Ctx) -> Result<(), String> {
    if !c.args.is_empty() {
        return Err(String::from("usage: snapshots"));
    }
    let t = target()?;
    let l = list(&t, false).map_err(|e| format!("errno {e}"))?;
    for s in &l {
        let kind = if s.auto { "auto" } else { s.label.as_str() };
        let mark = if s.rollback { "  <- /data goes back to it at the next start" } else { "" };
        c.println(&format!("{:>6}  {}  {kind}{mark}", s.id, local(s.created_ns)));
    }
    c.println(&format!("{} snapshots of /data", l.len()));
    Ok(())
}
console::command!(name: "snapshots", args: "", help: "snapshots of /data: id, local time, kind (the one a rollback goes back to is marked)", physical_only: false, run: console_snapshots);

fn console_rollback(c: &mut Ctx) -> Result<(), String> {
    let t = target()?;
    match c.args[..] {
        [] => {
            let p = pending_rollback(&t).map_err(|e| format!("errno {e}"))?;
            c.println(&p.map_or_else(|| String::from("no rollback scheduled"), |id| format!("/data goes back to snapshot {id} at the next start")));
            if let Ok(Some(id)) = crate::rollback_not_done(&t) {
                c.println(&format!("this start could not carry out the rollback to {id} (log: volume.rollback_failed); it is tried again at the next start"));
            }
        }
        ["cancel"] => {
            let p = crate::cancel_rollback(&t).map_err(|e| format!("errno {e}"))?;
            c.println(&p.map_or_else(|| String::from("no rollback was scheduled"), |id| format!("rollback to snapshot {id} cancelled")));
        }
        [id] => {
            let id: u64 = id.parse().map_err(|_| String::from("usage: rollback <snapshot id>|cancel"))?;
            let old = crate::schedule_rollback(&t, id).map_err(|e| format!("refused (errno {e}: 22 not a snapshot of /data, 2 no such one)"))?;
            c.println(&format!(
                "/data goes back to snapshot {id} at the next start (reboot); the state then is kept as snapshot \"before-rollback\"{}",
                old.filter(|&o| o != id).map_or_else(String::new, |o| format!(" (replaces the rollback to {o})"))
            ));
        }
        _ => return Err(String::from("usage: rollback [<snapshot id>|cancel]")),
    }
    Ok(())
}
console::command!(name: "rollback", args: "[<id>|cancel]", help: "the whole /data back to a snapshot at the next start (undo: rollback to the before-rollback snapshot)", physical_only: true, run: console_rollback);

fn console_restore_at(c: &mut Ctx) -> Result<(), String> {
    let usage = || String::from("usage: restore_at YYYY-MM-DD HH:MM [dry] | cancel");
    if c.args[..] == ["cancel"] {
        let (_, text) = system::undo_restore_at(&target()?)?;
        c.println(&text);
        return Ok(());
    }
    let (date, hm, dry) = match c.args[..] {
        [d, t] => (d, t, false),
        [d, t, "dry"] => (d, t, true),
        _ => return Err(usage()),
    };
    let (utc, doubt) = parse_local(date, hm, time::wall::tz()).ok_or_else(usage)?;
    if let Some(n) = doubt {
        c.println(&format!("note: {n}"));
    }
    let t = target()?;
    let p = system::restore_at(&t, utc, dry)?;
    c.println(&format!("/data: snapshot {} of {}{}", p.snapshot, local(p.snapshot_ns), if dry { " (dry run: nothing changed)" } else { ", at the next start" }));
    c.println(&format!("system: {}", p.note));
    if !dry {
        c.println("reboot (or system_pin <n> now) brings both; undo before that: restore_at cancel");
    }
    Ok(())
}
console::command!(name: "restore_at", args: "YYYY-MM-DD HH:MM [dry] | cancel", help: "everything as at a time (B1): /data's snapshot of then and the system generation of then, at the next start", physical_only: true, run: console_restore_at);

#[cfg(test)]
mod tests {
    use super::parse_local;
    use time::tz::{EUROPE_BERLIN, Tz};

    #[test]
    fn local_times() {
        // 2026-10-23 15:00 in Berlin is summer time (UTC+2): 13:00 UTC.
        assert_eq!(parse_local("2026-10-23", "15:00", EUROPE_BERLIN), Some((1_792_760_459, None)));
        assert_eq!(parse_local("2026-12-01", "15:00", Tz::Fixed { minutes: 0 }), Some((1_796_137_259, None)));
        // 2026-10-25 02:30 came twice (00:30 and 01:30 UTC): the summer pass, with a note.
        let (u, n) = parse_local("2026-10-25", "02:30", EUROPE_BERLIN).unwrap();
        assert_eq!((u, n.map(|n| n.starts_with("the clocks went back"))), (1_792_888_259, Some(true)));
        assert_eq!(parse_local("2026-10-25", "03:30", EUROPE_BERLIN).unwrap().1, None, "after the doubled hour");
        assert_eq!(parse_local("2026-10-25", "01:30", EUROPE_BERLIN).unwrap().1, None, "before it");
        // 2026-03-29 02:30 never was: standard offset (01:30 UTC), with a note.
        let (u, n) = parse_local("2026-03-29", "02:30", EUROPE_BERLIN).unwrap();
        assert_eq!((u, n.map(|n| n.contains("did not exist"))), (1_774_747_859, Some(true)));
        assert_eq!(parse_local("2026-03-29", "03:30", EUROPE_BERLIN).unwrap().1, None);
        for (d, t) in [("2026-10-23", "24:00"), ("2026-13-01", "10:00"), ("2026-02-30", "10:00"), ("2026-10-23", "10"), ("2026-10", "10:00"), ("x", "y"), ("2026-10-23-1", "10:00"), ("2026-10-23", "10:00:00")] {
            assert_eq!(parse_local(d, t, EUROPE_BERLIN), None, "{d} {t}");
        }
    }
}
