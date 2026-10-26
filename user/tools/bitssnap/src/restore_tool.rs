//! The MCP tool `restore` (safety class delete_overwrite, B5): every restore kind of VS-3 behind one tool, so
//! Claude finds them in one place and Georg's B3 rule for `restore` covers them all.

use crate::dev::Dev;
use crate::restore::{self, Report, program, volume};
use mcpreg::{Json, Reply};
use std::path::Path;

pub const SCHEMA: &str = r#"{"type":"object","properties":{"kind":{"type":"string","enum":["path","program","volume","system"],"description":"path (default): a file or folder below /data; program: a program's package version and its data folder /data/appdata/<name>; volume: the whole /data back to a snapshot at the next start; system: everything as at a time (/data's snapshot of then and the system generation of then) at the next start"},"snapshot":{"type":"integer","description":"snapshot id (tool snapshots); for path, program and volume"},"path":{"type":"string","description":"kind path: file or folder below /data"},"program":{"type":"string","description":"kind program: the package name"},"root":{"type":"string","description":"kind program: the bitspkg store root, default /"},"at":{"description":"kind system: the local time YYYY-MM-DD HH:MM (as tool snapshots shows times) or UTC seconds","oneOf":[{"type":"string"},{"type":"integer"}]},"cancel":{"type":"boolean","description":"kind volume or system: take back what the next start would do"},"dry_run":{"type":"boolean","description":"kind system: only say what would happen"}}}"#;

pub const DESCRIPTION: &str = "Bring things back as they were in a snapshot of /data (B1). kind path (default): a file or folder below /data, at once. kind program: a program's package version and its data folder, at once. kind volume: the whole /data at the next start (reboot, or a Reload). kind system: everything as at a time, /data plus the system generation that ran then (system_pin), at the next start. The state before is always kept as a snapshot (before-restore at once, before-rollback at the switch), so every restore is undone by restoring from that one; volume and system can be cancelled before the start with cancel=true.";

fn n(x: u64) -> Json {
    Json::Num(x as f64)
}

fn opt(x: Option<u64>) -> Json {
    x.map_or(Json::Null, n)
}

pub fn report_json(r: &Report) -> Json {
    Json::obj(vec![
        ("restored", Json::str(&r.path)),
        ("from", n(r.from)),
        ("before", n(r.before)),
        ("replaced", Json::Bool(r.replaced)),
        ("files", n(r.files)),
        ("dirs", n(r.dirs)),
        ("links", n(r.links)),
        ("hardlinks", n(r.hardlinks)),
        ("skipped", n(r.skipped)),
        ("not_restored", Json::Arr(r.special.iter().map(|s| Json::str(s)).collect())),
        ("bytes", n(r.bytes)),
    ])
}

fn path(dev: &mut Dev, args: &Json) -> Reply {
    let (Some(id), Some(path)) = (args.get("snapshot").and_then(Json::as_u64), args.get("path").and_then(Json::as_str)) else {
        return Reply::bad_argument("want snapshot (an id) and path (below /data)");
    };
    if let Err(e) = restore::below_data(path) {
        return Reply::bad_argument(e);
    }
    match restore::restore(dev, id, path) {
        Ok(r) => {
            crate::log(&format!("MCP restore of {} from snapshot {id} (before: {})", r.path, r.before));
            Reply::text(report_json(&r).to_string())
        }
        Err(e) => Reply::error(e),
    }
}

fn program_kind(dev: &mut Dev, args: &Json) -> Reply {
    let (Some(id), Some(name)) = (args.get("snapshot").and_then(Json::as_u64), args.get("program").and_then(Json::as_str)) else {
        return Reply::bad_argument("want snapshot (an id) and program (a package name)");
    };
    let root = args.get("root").and_then(Json::as_str).unwrap_or("/");
    match program::restore_program(dev, id, name, Path::new(root)) {
        Ok(r) => Reply::text(
            Json::obj(vec![
                ("kind", Json::str("program")),
                ("program", Json::str(&r.name)),
                ("from", n(r.from)),
                ("before", n(r.before)),
                ("version", r.version.as_deref().map_or(Json::Null, Json::str)),
                ("was", r.was.as_deref().map_or(Json::Null, Json::str)),
                ("package", Json::str(&r.package)),
                ("data", r.data.as_ref().map_or(Json::Null, report_json)),
                ("text", Json::str(&r.text())),
            ])
            .to_string(),
        ),
        Err(e) => Reply::error(e),
    }
}

fn volume_kind(dev: &mut Dev, args: &Json) -> Reply {
    let cancel = args.get("cancel").and_then(Json::as_bool).unwrap_or(false);
    let id = args.get("snapshot").and_then(Json::as_u64);
    let r = match (cancel, id) {
        (true, Some(_)) => return Reply::bad_argument("either snapshot (schedule) or cancel=true, not both"),
        (true, None) => volume::cancel(dev),
        (false, Some(id)) => volume::schedule(dev, id),
        (false, None) => volume::status(dev),
    };
    let r = match r {
        Ok(r) => r,
        Err(e) => return Reply::error(e),
    };
    let mut v = vec![("kind", Json::str("volume"))];
    match &r {
        volume::Rollback::Scheduled { to, replaced } => v.extend([("scheduled", n(*to)), ("replaced", opt(*replaced))]),
        volume::Rollback::Cancelled(c) => v.push(("cancelled", opt(*c))),
        volume::Rollback::Pending(p) => v.push(("pending", opt(*p))),
    }
    v.push(("text", Json::str(&volume::describe(&r))));
    Reply::text(Json::obj(v).to_string())
}

fn system_kind(dev: &mut Dev, args: &Json) -> Reply {
    if args.get("cancel").and_then(Json::as_bool).unwrap_or(false) {
        return match volume::undo_system(dev) {
            Ok((done, t)) => Reply::text(Json::obj(vec![("kind", Json::str("system")), ("cancelled", Json::Bool(done)), ("text", Json::str(&t))]).to_string()),
            Err(e) => Reply::error(e),
        };
    }
    let off = match dev.stats() {
        Ok(s) => s.utc_offset_min,
        Err(e) => return Reply::error(e.to_string()),
    };
    let utc = match args.get("at") {
        Some(Json::Str(s)) => volume::parse_at(s, off),
        Some(j) => j.as_i64().filter(|&t| t > 0).ok_or_else(|| String::from("at: UTC seconds > 0, or YYYY-MM-DD HH:MM")),
        None => Err(String::from("want at (YYYY-MM-DD HH:MM local time, or UTC seconds)")),
    };
    let utc = match utc {
        Ok(u) => u,
        Err(e) => return Reply::bad_argument(e),
    };
    let dry = args.get("dry_run").and_then(Json::as_bool).unwrap_or(false);
    match volume::restore_system(dev, utc, dry) {
        Ok(r) => Reply::text(
            Json::obj(vec![
                ("kind", Json::str("system")),
                ("dry_run", Json::Bool(dry)),
                ("at_s", Json::Num(utc as f64)),
                ("snapshot", n(r.snapshot)),
                ("snapshot_time", Json::str(&crate::local_time(r.snapshot_ns / 1_000_000_000, off * 60))),
                ("pin", if r.pin == 0 { Json::Null } else { n(r.pin.into()) }),
                ("running", n(r.running.into())),
                ("next", n(r.next.into())),
                ("text", Json::str(&volume::system_text(&r, dry, off * 60))),
            ])
            .to_string(),
        ),
        Err(e) => Reply::error(e),
    }
}

/// The `restore` call.
pub fn answer(dev: &mut Dev, args: &Json) -> Reply {
    match args.get("kind").map(|k| k.as_str()) {
        None | Some(Some("path")) => path(dev, args),
        Some(Some("program")) => program_kind(dev, args),
        Some(Some("volume")) => volume_kind(dev, args),
        Some(Some("system")) => system_kind(dev, args),
        _ => Reply::bad_argument("kind: path, program, volume or system"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(args: &str) -> bool {
        let mut d = Dev::open_at("/dev/null").unwrap();
        matches!(answer(&mut d, &Json::parse(args).unwrap()), Reply::BadArgument(_))
    }

    #[test]
    fn schema_and_arguments() {
        let s = Json::parse(SCHEMA).unwrap();
        let kinds = s.get("properties").and_then(|p| p.get("kind")).and_then(|k| k.get("enum")).cloned();
        assert_eq!(kinds, Some(Json::Arr(["path", "program", "volume", "system"].iter().map(|k| Json::str(k)).collect())));
        for a in [
            r#"{"kind":"bogus"}"#,
            r#"{"kind":7}"#,
            r#"{"snapshot":3}"#,
            r#"{"snapshot":3,"path":"/etc/passwd"}"#,
            r#"{"kind":"path","snapshot":3,"path":"/data"}"#,
            r#"{"kind":"program","snapshot":3}"#,
            r#"{"kind":"program","program":"brave"}"#,
            r#"{"kind":"volume","snapshot":3,"cancel":true}"#,
            r#"{"kind":"system"}"#,
            r#"{"kind":"system","at":"yesterday"}"#,
            r#"{"kind":"system","at":-5}"#,
        ] {
            assert!(bad(a), "{a} was not refused as a bad argument");
        }
    }
}
