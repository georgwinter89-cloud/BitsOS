//! RB-8: MCP `events` and `metrics` reach back into the journal. With `since`/`until` (wall time) a call goes to
//! the journal service's tool `journal` (service `bitsjournal`, uid 0 only: another program cannot answer in its
//! name), which has every boot's events and metrics for weeks. Without the service the call is answered from
//! this boot's rings for the part of the range that lies in this boot, and the answer says so.
//!
//! Times: unix seconds (a number or digits), `YYYY-MM-DD HH:MM[:SS]` (also with `T`), `YYYY-MM-DD`, or
//! `HH:MM[:SS]` = the most recent such time (today, or yesterday when that is still to come), all in the
//! machine's zone (CONFIG `tz=`, default Europe/Berlin).

use crate::tool::bad;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use mcp::svc::SvcTool;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult};
use time::tz::{DateTime, Tz};

pub const SERVICE: &str = "bitsjournal";
pub const TOOL: &str = "journal";

fn digits(s: &str) -> Option<u32> {
    (!s.is_empty() && s.len() <= 4 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

fn clock(s: &str) -> Option<(u8, u8, u8)> {
    let mut p = s.split(':');
    let h = digits(p.next()?)?;
    let m = digits(p.next()?)?;
    let sec = match p.next() {
        Some(x) => digits(x)?,
        None => 0,
    };
    (p.next().is_none() && h < 24 && m < 60 && sec < 60).then_some((h as u8, m as u8, sec as u8))
}

fn date(s: &str) -> Option<(u32, u8, u8)> {
    let mut p = s.split('-');
    let (y, m, d) = (digits(p.next()?)?, digits(p.next()?)?, digits(p.next()?)?);
    (p.next().is_none() && (1970..=9999).contains(&y) && (1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m as u8, d as u8))
}

/// UTC seconds of a wall-time argument; `now` = UTC seconds now (for `HH:MM`).
pub fn parse_wall(s: &str, now: i64, tz: Tz) -> Option<i64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<f64>()
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        return (0.0..1e11).contains(&v).then_some(v as i64);
    }
    let local = |y, mo, d, (h, mi, se): (u8, u8, u8)| tz.to_utc(&DateTime { year: y, month: mo, day: d, hour: h, min: mi, sec: se });
    let (d, t) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (Some(d), Some(t)),
        None if s.contains('-') => (Some(s), None),
        None => (None, Some(s)),
    };
    match (d, t) {
        (Some(d), t) => {
            let (y, mo, da) = date(d)?;
            let c = match t {
                Some(t) => clock(t)?,
                None => (0, 0, 0),
            };
            Some(local(y, mo, da, c))
        }
        (None, Some(t)) => {
            let c = clock(t)?;
            let today = DateTime::from_unix(now + tz.offset_at(now) as i64 * 60);
            let at = local(today.year, today.month, today.day, c);
            if at <= now {
                return Some(at);
            }
            let y = DateTime::from_unix(today.unix() - 86_400);
            Some(local(y.year, y.month, y.day, c))
        }
        (None, None) => None,
    }
}

/// UTC seconds of `since`/`until` (`until` defaults to the next second, `since` to an hour before `until`); None when neither is given.
fn range(args: &Json, now: i64, tz: Tz) -> Result<Option<(i64, i64)>, ToolError> {
    let one = |key: &str| -> Result<Option<i64>, ToolError> {
        match args.get(key) {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Num(v)) if (0.0..1e11).contains(v) => Ok(Some(*v as i64)),
            Some(Json::Str(s)) => parse_wall(s, now, tz).map(Some).ok_or_else(|| bad(key)),
            Some(_) => Err(bad(key)),
        }
    };
    let (since, until) = (one("since")?, one("until")?);
    if since.is_none() && until.is_none() {
        return Ok(None);
    }
    // Now in whole seconds is up to 1 s behind the newest record: the default end is the next second.
    let until = until.unwrap_or(now + 1);
    let since = since.unwrap_or(until.saturating_sub(3600));
    if since > until {
        return Err(bad("until"));
    }
    Ok(Some((since, until)))
}

/// The journal's tool when bitsjournal (uid 0) registered it.
fn tool() -> Option<Arc<SvcTool>> {
    let t = mcp::svc::kernel_registry()?.find(TOOL)?;
    (t.owner == SERVICE && t.uid == 0).then_some(t)
}

fn without(args: &Json, drop: &[&str]) -> Vec<(String, Json)> {
    match args {
        Json::Obj(v) => v.iter().filter(|(k, _)| !drop.contains(&k.as_str())).cloned().collect(),
        _ => Vec::new(),
    }
}

/// Whether bitsjournal's tool is registered.
pub fn present() -> bool {
    tool().is_some()
}

/// `events`/`metrics` with `since`/`until`: Some(answer) from the journal (or this boot's rings without it);
/// None when neither argument is given (the tool answers from the rings as before).
pub fn reach_back(ctx: &ToolContext, kind: &'static str, args: &Json, ring: fn(&ToolContext, &Json) -> ToolResult) -> Option<ToolResult> {
    let now_ns = time::wall::realtime_ns();
    let now = now_ns.map_or(0, |n| (n / 1_000_000_000) as i64);
    let (since, until) = match range(args, now, time::wall::tz()) {
        Ok(Some(r)) => r,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    if let Some(t) = tool() {
        let mut a = without(args, &["since", "until", "from", "to", "last"]);
        a.push((String::from("kind"), Json::Str(String::from(kind))));
        a.push((String::from("since"), Json::Num(since as f64)));
        a.push((String::from("until"), Json::Num(until as f64)));
        return Some(mcp::catalog::Tool::Service(t).invoke(ctx, &Json::Obj(a)));
    }
    // No journal: the part of the range in this boot, from the rings.
    let Some(wall_ns) = now_ns else {
        return Some(Err(ToolError::Failed(String::from("no wall clock (RTC unreadable) and no journal (bitsjournal is not running): use from/to in seconds since boot"))));
    };
    let boot_s = wall_ns as f64 / 1e9 - time::now_ns() as f64 / 1e9;
    let from = (since as f64 - boot_s).max(0.0);
    let to = until as f64 - boot_s;
    let note = alloc::format!("journal not running (bitsjournal): only this boot's {kind} (boot at unix {}), from its rings\n", boot_s as i64);
    if to < 0.0 {
        return Some(Ok(alloc::vec![ToolOutput::Text(alloc::format!("{note}{kind}: the range ends before this boot"))]));
    }
    let mut a = without(args, &["since", "until", "from", "to", "last"]);
    a.push((String::from("from"), Json::Num(from)));
    a.push((String::from("to"), Json::Num(to)));
    Some(ring(ctx, &Json::Obj(a)).map(|mut out| {
        if let Some(ToolOutput::Text(t)) = out.first_mut() {
            t.insert_str(0, &note);
        }
        out
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BERLIN: Tz = time::tz::EUROPE_BERLIN;
    // 2026-10-23 10:00:00 UTC = 12:00 in Berlin (summer time until 25 Oct).
    const NOW: i64 = 1_792_749_600;

    #[test]
    fn wall_times_in_the_local_zone() {
        assert_eq!(parse_wall("1792749600", NOW, BERLIN), Some(NOW));
        assert_eq!(parse_wall("2026-10-23 12:00", NOW, BERLIN), Some(NOW));
        assert_eq!(parse_wall("2026-10-23T12:00:30", NOW, BERLIN), Some(NOW + 30));
        assert_eq!(parse_wall("2026-10-23", NOW, BERLIN), Some(NOW - 12 * 3600));
        // "03:12 last night": today 03:12 local is in the past.
        assert_eq!(parse_wall("03:12", NOW, BERLIN), Some(NOW - 8 * 3600 - 48 * 60));
        // 13:00 has not come yet today: yesterday's.
        assert_eq!(parse_wall("13:00", NOW, BERLIN), Some(NOW + 3600 - 86_400));
        for bad in ["", "25:00", "12:61", "2026-13-01 00:00", "yesterday", "-5", "1e3"] {
            assert_eq!(parse_wall(bad, NOW, BERLIN), None, "{bad}");
        }
    }

    #[test]
    fn range_defaults_and_errors() {
        let a = |s: &str| Json::parse(s).unwrap();
        assert_eq!(range(&a("{}"), NOW, BERLIN), Ok(None));
        assert_eq!(range(&a(r#"{"since":"11:00"}"#), NOW, BERLIN), Ok(Some((NOW - 3600, NOW + 1))));
        assert_eq!(range(&a(r#"{"until":100000}"#), NOW, BERLIN), Ok(Some((96_400, 100_000))));
        assert_eq!(range(&a(r#"{"since":"x"}"#), NOW, BERLIN), Err(bad("since")));
        assert_eq!(range(&a(r#"{"since":5000,"until":4000}"#), NOW, BERLIN), Err(bad("until")));
        assert_eq!(range(&a(r#"{"since":true}"#), NOW, BERLIN), Err(bad("since")));
    }
}
