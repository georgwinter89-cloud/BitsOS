//! REWIND: "Zurückspulen" (Verlauf.dc.html, V2-LANES VS-5): the snapshots of /data on a time line, newest first,
//! and for the chosen one three ways back: a file or folder (bitssnap's restore, the state before saved first), a
//! program (its version and data folder), or everything (data and system as then, at the next start). The view
//! only asks bitssnap's desktop socket (snap_client); bitssnap decides. Each change first shows what it would
//! change (bitssnap's dry run) in the pages' confirmation dialog and goes only with Georg's own yes. Pure here:
//! the model, the answers read, the German texts; rewind_app.rs is the App side, rewind_view.rs the places,
//! rewind_draw.rs the look.

use std::collections::HashMap;
use std::time::Duration;

use mcpreg::Json;

use crate::page_confirm::Dialog;

/// The labels bitssnap gives the snapshots it takes before a restore (bitssnap restore.rs).
pub const BEFORE_RESTORE: &str = "before-restore";
pub const BEFORE_ROLLBACK: &str = "before-rollback";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapKind {
    Auto,
    Manual,
    BeforeRestore,
    BeforeRollback,
}

impl SnapKind {
    pub fn of(s: &str) -> SnapKind {
        match s {
            "auto" => SnapKind::Auto,
            "before-restore" => SnapKind::BeforeRestore,
            "before-rollback" => SnapKind::BeforeRollback,
            _ => SnapKind::Manual,
        }
    }

    /// How the time line names it.
    pub fn word(self) -> &'static str {
        match self {
            SnapKind::Auto => "automatisch",
            SnapKind::Manual => "von Hand",
            SnapKind::BeforeRestore => "vor dem Zurückholen",
            SnapKind::BeforeRollback => "vor dem Zurücksetzen",
        }
    }

    /// The time line's chips have room for less.
    pub fn short(self) -> &'static str {
        match self {
            SnapKind::BeforeRestore => "vor Zurückholen",
            SnapKind::BeforeRollback => "vor Zurücksetzen",
            k => k.word(),
        }
    }

    /// For `ctl rewind state` and `ctl hot`.
    pub fn name(self) -> &'static str {
        match self {
            SnapKind::Auto => "auto",
            SnapKind::Manual => "manual",
            SnapKind::BeforeRestore => "before-restore",
            SnapKind::BeforeRollback => "before-rollback",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub id: u64,
    pub created_s: u64,
    pub kind: SnapKind,
    pub label: String,
}

/// Whether there is anything to rewind to, and if not why.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Avail {
    /// Not asked yet.
    #[default]
    Unknown,
    /// No socket: bitssnap does not run.
    Absent,
    /// It is there and did not answer as it should.
    Failed,
    /// /data is still being mounted.
    Pending,
    /// No snapshots on this start (plain image, read-only ...).
    Never,
    Empty,
    Ready(usize),
}

impl Avail {
    pub fn ready(&self) -> bool {
        matches!(self, Avail::Ready(_))
    }

    pub fn name(&self) -> String {
        match self {
            Avail::Unknown => "unknown".into(),
            Avail::Absent => "absent".into(),
            Avail::Failed => "failed".into(),
            Avail::Pending => "pending".into(),
            Avail::Never => "never".into(),
            Avail::Empty => "empty".into(),
            Avail::Ready(n) => format!("ready:{n}"),
        }
    }

    /// The sentence a click on the greyed button shows.
    pub fn reason(&self) -> Option<&'static str> {
        Some(match self {
            Avail::Ready(_) => return None,
            Avail::Unknown => "Zurückspulen: BitsOS prüft noch, ob es gespeicherte Stände gibt.",
            Avail::Absent => "Zurückspulen geht gerade nicht: der Dienst für gespeicherte Stände läuft nicht.",
            Avail::Failed => "Zurückspulen geht gerade nicht: der Dienst für gespeicherte Stände antwortet nicht richtig.",
            Avail::Pending => "Zurückspulen geht gleich: der Datenbereich wird noch eingebunden.",
            Avail::Never => "Zurückspulen geht auf diesem Start nicht: für den Datenbereich werden keine Stände gespeichert.",
            Avail::Empty => "Zurückspulen geht noch nicht: es ist noch kein Stand gespeichert.",
        })
    }

    /// The short line under the greyed button (on hover).
    pub fn short(&self) -> &'static str {
        match self {
            Avail::Ready(_) => "",
            Avail::Unknown => "Wird geprüft",
            Avail::Absent => "Dienst läuft nicht",
            Avail::Failed => "Dienst antwortet nicht",
            Avail::Pending => "Daten noch nicht da",
            Avail::Never => "Keine Stände hier",
            Avail::Empty => "Noch kein Stand",
        }
    }
}

/// What the right side shows: the choice, or one of the three ways back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Choose,
    Path,
    Program,
    All,
}

impl Tab {
    pub const WAYS: [Tab; 3] = [Tab::Path, Tab::Program, Tab::All];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Choose => "choose",
            Tab::Path => "path",
            Tab::Program => "program",
            Tab::All => "all",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Tab::Choose => "",
            Tab::Path => "Eine Datei oder einen Ordner zurückholen",
            Tab::Program => "Ein Programm zurücksetzen",
            Tab::All => "Alles auf diesen Zeitpunkt",
        }
    }

    pub fn line(self) -> &'static str {
        match self {
            Tab::Choose => "",
            Tab::Path => "Sofort. Was jetzt dort liegt, wird vorher gesichert.",
            Tab::Program => "Sofort: seine Version und sein Datenordner von damals.",
            Tab::All => "Beim nächsten Start: Daten und System wie damals.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Folder,
    File,
    Link,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// A folder the view can go into.
    pub open: bool,
    pub bytes: Option<u64>,
}

/// The folders of the chosen snapshot, one at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Browse {
    pub path: String,
    pub entries: Vec<Entry>,
    pub more: bool,
    /// The chosen entry (None: the folder itself).
    pub pick: Option<usize>,
    pub first: usize,
    pub loading: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prog {
    pub name: String,
    /// The version then and now (None: none, or the packages are not on /data).
    pub version: Option<String>,
    pub now: Option<String>,
    pub data: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progs {
    pub list: Vec<Prog>,
    pub pick: Option<usize>,
    pub first: usize,
    pub loading: bool,
    pub error: Option<String>,
}

/// What the dialog's yes does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    Path { snap: u64, path: String },
    Program { snap: u64, name: String },
    All { snap: u64, at: u64 },
    /// Take back the "Alles" of this start (before the restart).
    Undo,
    Restart,
}

impl Ask {
    /// The snapshot the question is about, and the way it belongs to (None: the undo and the restart).
    pub fn about(&self) -> Option<(u64, Tab)> {
        match self {
            Ask::Path { snap, .. } => Some((*snap, Tab::Path)),
            Ask::Program { snap, .. } => Some((*snap, Tab::Program)),
            Ask::All { snap, .. } => Some((*snap, Tab::All)),
            Ask::Undo | Ask::Restart => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Ask::Path { .. } => "path",
            Ask::Program { .. } => "program",
            Ask::All { .. } => "all",
            Ask::Undo => "undo",
            Ask::Restart => "restart",
        }
    }

    /// The request line (bitssnap local.rs); `dry`: the preview, which changes nothing.
    pub fn line(&self, dry: bool) -> Option<String> {
        let mut v = match self {
            Ask::Path { snap, path } => vec![("op", Json::str("restore-path")), ("snapshot", Json::Num(*snap as f64)), ("path", Json::str(path))],
            Ask::Program { snap, name } => vec![("op", Json::str("restore-program")), ("snapshot", Json::Num(*snap as f64)), ("program", Json::str(name))],
            Ask::All { at, .. } => vec![("op", Json::str("restore-system")), ("at", Json::Num(*at as f64))],
            Ask::Undo if !dry => vec![("op", Json::str("undo"))],
            Ask::Undo | Ask::Restart => return None,
        };
        v.push(if dry { ("dry_run", Json::Bool(true)) } else { ("confirmed", Json::Bool(true)) });
        Some(Json::obj(v).to_string())
    }
}

/// What an open request is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    List,
    Browse { snap: u64, path: String },
    Programs { snap: u64 },
    Preview(Ask),
    Do(Ask),
    Power,
}

/// The parts of the view a pointer can hit (`ctl hot` names them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hot {
    Back,
    Now,
    Older,
    Newer,
    Chip(usize),
    Way(Tab),
    Up,
    Row(usize),
    Do,
    Restart,
    Undo,
    DialogCancel,
    DialogOk,
}

#[derive(Default)]
pub struct Rewind {
    pub open: bool,
    pub avail: Avail,
    /// Newest first.
    pub snaps: Vec<Snap>,
    pub off_min: i64,
    pub now_s: u64,
    /// A rollback of /data waits for the next start (an "Alles" of this start, or MCP's).
    pub pending: Option<u64>,
    pub pick: usize,
    /// The first chip shown on the time line.
    pub first: usize,
    pub tab: Tab,
    pub browse: Browse,
    pub progs: Progs,
    pub dialog: Option<(Dialog, Ask)>,
    /// When the dialog appeared or injected input last touched it (`pacer` time): a yes counts `ARM` later.
    pub since: Duration,
    /// An "Alles" was set for the next start from this view: the restart and its undo are offered.
    pub set_for_start: bool,
    /// The snapshot (and the generation pinned, if one was) this compositor's "Alles" set for the next start:
    /// tells a pending rollback of its own from one set another way.
    pub ours: Option<(u64, Option<u64>)>,
    pub busy: Option<String>,
    pub msg: Option<(String, bool)>,
    /// The reason last shown as a notice for the grey button: the view drops it when it opens.
    pub said: Option<String>,
    pub hover: Option<Hot>,
    pub waiting: HashMap<u64, Want>,
    pub next_id: u64,
    pub images: crate::rewind_draw::Images,
    /// Tests: the request lines, kept here instead of sent (their answers are fed to `rewind_msg`).
    #[cfg(test)]
    pub sent: Option<Vec<(u64, String)>>,
}

impl Rewind {
    pub fn chosen(&self) -> Option<&Snap> {
        self.snaps.get(self.pick)
    }

    /// A list answer: the snapshots (newest first) and the availability; the pick stays on the same snapshot when
    /// it is still there.
    pub fn take_list(&mut self, j: &Json) -> bool {
        let before = (self.avail.clone(), self.snaps.clone(), self.pending);
        if j.get("ok").and_then(Json::as_bool) != Some(true) {
            self.avail = Avail::Failed;
            return before != (self.avail.clone(), self.snaps.clone(), self.pending);
        }
        let keep = self.chosen().map(|s| s.id);
        let mut snaps: Vec<Snap> = j
            .get("snapshots")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|s| {
                Some(Snap {
                    id: s.get("id")?.as_u64()?,
                    created_s: s.get("created_s").and_then(Json::as_u64).unwrap_or(0),
                    kind: SnapKind::of(s.get("kind").and_then(Json::as_str).unwrap_or("")),
                    label: s.get("label").and_then(Json::as_str).unwrap_or("").to_string(),
                })
            })
            .collect();
        snaps.sort_by_key(|s| std::cmp::Reverse((s.created_s, s.id)));
        self.off_min = j.get("utc_offset_min").and_then(Json::as_i64).unwrap_or(self.off_min);
        self.now_s = j.get("now_s").and_then(Json::as_u64).unwrap_or(self.now_s);
        self.pending = j.get("rollback_pending").and_then(Json::as_u64);
        self.avail = match j.get("data").and_then(Json::as_str) {
            Some("pending") => Avail::Pending,
            Some("never") => Avail::Never,
            _ if snaps.is_empty() => Avail::Empty,
            _ => Avail::Ready(snaps.len()),
        };
        self.pick = keep.and_then(|id| snaps.iter().position(|s| s.id == id)).unwrap_or(0);
        self.snaps = snaps;
        before != (self.avail.clone(), self.snaps.clone(), self.pending)
    }
}

/// A browse answer's entries.
pub fn entries(j: &Json) -> Vec<Entry> {
    j.get("entries")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| {
            let kind = match e.get("kind").and_then(Json::as_str)? {
                "folder" => EntryKind::Folder,
                "file" => EntryKind::File,
                "link" => EntryKind::Link,
                _ => EntryKind::Other,
            };
            Some(Entry { name: e.get("name")?.as_str()?.to_string(), kind, open: e.get("open").and_then(Json::as_bool) == Some(true), bytes: e.get("bytes").and_then(Json::as_u64) })
        })
        .collect()
}

pub fn programs(j: &Json) -> Vec<Prog> {
    let s = |p: &Json, k: &str| p.get(k).and_then(Json::as_str).map(str::to_string);
    j.get("programs")
        .and_then(Json::as_array)
        .unwrap_or(&[])
        .iter()
        .filter_map(|p| Some(Prog { name: s(p, "name")?, version: s(p, "version"), now: s(p, "now"), data: p.get("data").and_then(Json::as_bool) == Some(true) }))
        .collect()
}

/// The folder above `path`, never above /data (None at /data).
pub fn parent(path: &str) -> Option<String> {
    let p = path.trim_end_matches('/');
    if p == "/data" || !p.starts_with("/data/") {
        return None;
    }
    p.rsplit_once('/').map(|(a, _)| a.to_string()).filter(|a| mine(a))
}

/// Whether `path` is the home folder or below it: the only place bitssnap restores a path for the desktop user
/// (local.rs `yours`), so the list never goes above it.
pub fn mine(path: &str) -> bool {
    let (p, h) = (path.trim_end_matches('/'), home());
    p == h || p.strip_prefix(h.as_str()).is_some_and(|r| r.starts_with('/'))
}

/// Where the folder list starts: the desktop user's home on /data.
pub fn home() -> String {
    match std::env::var("HOME") {
        Ok(h) if h.starts_with("/data/") => h.trim_end_matches('/').to_string(),
        _ => "/data/home/1000".into(),
    }
}

// --- Times and sizes in German ---

/// (days since 1970, seconds of the day) of UTC seconds `t` shifted by `off_min`.
fn local(t: u64, off_min: i64) -> (i64, i64) {
    let s = t as i64 + off_min * 60;
    (s.div_euclid(86_400), s.rem_euclid(86_400))
}

/// (year, month, day) of days since 1970 (Howard Hinnant's civil_from_days, as bitssnap's local_time).
fn civil(days: i64) -> (i64, i64, i64) {
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

const WEEKDAY: [&str; 7] = ["Do", "Fr", "Sa", "So", "Mo", "Di", "Mi"];

/// "13:58".
pub fn clock(t: u64, off_min: i64) -> String {
    let (_, s) = local(t, off_min);
    format!("{:02}:{:02}", s / 3600, s % 3600 / 60)
}

/// "Heute", "Gestern" or "Mi 28.10." (`now`: the kernel's now).
pub fn day(t: u64, off_min: i64, now: u64) -> String {
    let (d, _) = local(t, off_min);
    let (today, _) = local(now, off_min);
    match today - d {
        0 => "Heute".into(),
        1 => "Gestern".into(),
        _ => {
            let (_, m, dd) = civil(d);
            format!("{} {dd:02}.{m:02}.", WEEKDAY[d.rem_euclid(7) as usize])
        }
    }
}

/// "heute um 13:58", "gestern um 13:58", "am Mi 28.10. um 13:58".
pub fn when(t: u64, off_min: i64, now: u64) -> String {
    let d = day(t, off_min, now);
    let d = match d.as_str() {
        "Heute" => "heute".into(),
        "Gestern" => "gestern".into(),
        _ => format!("am {d}"),
    };
    format!("{d} um {}", clock(t, off_min))
}

/// "Mi 28.10.2026".
pub fn date_long(t: u64, off_min: i64) -> String {
    let (d, _) = local(t, off_min);
    let (y, m, dd) = civil(d);
    format!("{} {dd:02}.{m:02}.{y}", WEEKDAY[d.rem_euclid(7) as usize])
}

/// "vor 12 Min", "vor 3 Std", "vor 2 Tagen" ("gerade eben" under a minute).
pub fn ago(t: u64, now: u64) -> String {
    let s = now.saturating_sub(t);
    match s {
        0..60 => "gerade eben".into(),
        60..3600 => format!("vor {} Min", s / 60),
        3600..86_400 => format!("vor {} Std", s / 3600),
        _ if s / 86_400 == 1 => "vor 1 Tag".into(),
        _ => format!("vor {} Tagen", s / 86_400),
    }
}

/// "12,3 MB" (decimal units, a German comma).
pub fn size(bytes: u64) -> String {
    const U: [&str; 5] = ["Byte", "kB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} Byte") } else { format!("{v:.1} {}", U[i]).replace('.', ",") }
}

fn count(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// bitssnap's tally of a tree in words: "eine Datei, 12,3 kB" or "3 Dateien in 2 Ordnern, 1,2 MB". A big tree is
/// counted only so far (bitssnap's bound): then what was counted, as a lower bound, "mehr als 20000 Einträge,
/// mindestens 1,2 GB".
pub fn tally(j: &Json) -> String {
    let g = |k: &str| j.get(k).and_then(Json::as_u64).unwrap_or(0);
    match j.get("kind").and_then(Json::as_str) {
        Some("file") => format!("eine Datei, {}", size(g("bytes"))),
        Some("link") => "eine Verknüpfung".into(),
        _ if j.get("more").and_then(Json::as_bool) == Some(true) => match g("bytes") {
            0 => format!("mehr als {} Einträge", g("files") + g("dirs") + g("links")),
            b => format!("mehr als {} Einträge, mindestens {}", g("files") + g("dirs") + g("links"), size(b)),
        },
        _ => format!("{} in {}, {}", count(g("files"), "Datei", "Dateien"), count(g("dirs"), "Ordner", "Ordnern"), size(g("bytes"))),
    }
}

/// The dialog's lines for a file or folder (bitssnap's dry run `j`).
pub fn path_lines(s: &Snap, j: &Json, off_min: i64, now: u64) -> Vec<String> {
    let path = j.get("path").and_then(Json::as_str).unwrap_or("?");
    let then = j.get("then").filter(|t| **t != Json::Null).map_or("nichts".into(), tally);
    let now_t = match j.get("now") {
        Some(Json::Null) | None => "nichts (es kommt neu dazu)".into(),
        Some(t) => tally(t),
    };
    vec![
        path.to_string(),
        format!("Stand von {} ({})", when(s.created_s, off_min, now), s.kind.word()),
        format!("Damals: {then}"),
        format!("Jetzt: {now_t}"),
        "Was jetzt dort liegt, wird vorher als Stand gesichert.".into(),
        "Das Zurückholen lässt sich so rückgängig machen.".into(),
    ]
}

/// The dialog's lines for a program.
pub fn program_lines(s: &Snap, j: &Json, off_min: i64, now: u64) -> Vec<String> {
    let name = j.get("program").and_then(Json::as_str).unwrap_or("?");
    let v = |k: &str| j.get(k).and_then(Json::as_str).map_or("keine".to_string(), |v| format!("Version {v}"));
    let data = |k: &str| match j.get(k) {
        Some(Json::Null) | None => "keiner".to_string(),
        Some(t) => tally(t),
    };
    let mut l = vec![
        format!("Programm „{name}“ wie {}", when(s.created_s, off_min, now)),
        format!("Damals: {} · jetzt: {}", v("version"), v("now")),
        format!("Datenordner damals: {}", data("data_then")),
        format!("Datenordner jetzt: {}", data("data_now")),
    ];
    if let Some(p) = j.get("data_path").and_then(Json::as_str) {
        l.push(p.to_string());
    }
    l.push("Vorher wird alles als Stand gesichert: rückgängig machbar.".into());
    l.push("Läuft das Programm gerade, starte es danach neu.".into());
    l
}

/// The dialog's lines for "Alles" (bitssnap's dry run of restore-system).
pub fn all_lines(j: &Json, off_min: i64) -> Vec<String> {
    let s = j.get("snapshot_s").and_then(Json::as_u64).unwrap_or(0);
    let num = |k: &str| j.get(k).and_then(Json::as_u64);
    let system = match (num("pin"), num("running")) {
        (Some(p), Some(r)) if p != r => format!("System: Generation {p} (läuft jetzt: Generation {r})"),
        (Some(p), _) => format!("System: Generation {p}, die schon läuft"),
        (None, Some(r)) => format!("System: bleibt Generation {r} (keine ältere passt zu der Zeit)"),
        _ => "System: wie jetzt".into(),
    };
    vec![
        format!("Alles wie am {} um {}", date_long(s, off_min), clock(s, off_min)),
        format!("Daten: Stand Nr. {} von {} {}", num("snapshot").unwrap_or(0), date_long(s, off_min), clock(s, off_min)),
        system,
        "Beim nächsten Start sind Daten und System wie damals.".into(),
        "Was neuer ist, bleibt als Stand „vor dem Zurücksetzen“ erhalten.".into(),
        "Rückgängig: hier vor dem Neustart, danach mit diesem Stand.".into(),
    ]
}

/// The pending rollback: ("Stand Nr. N", "vom <date> um <time>" when the list has it), and whether this view set it.
fn pending_words(r: &Rewind) -> (String, Option<String>, bool) {
    let Some(id) = r.pending.or(r.ours.map(|o| o.0)) else { return ("dem eingestellten Stand".into(), None, false) };
    let when = r.snaps.iter().find(|s| s.id == id).map(|s| format!("vom {} um {}", date_long(s.created_s, r.off_min), clock(s.created_s, r.off_min)));
    (format!("Stand Nr. {id}"), when, r.ours.is_some_and(|o| o.0 == id))
}

/// The restart's question: what the start will bring back, named, not "the chosen time".
pub fn restart_lines(r: &Rewind) -> Vec<String> {
    let (stand, when, ours) = pending_words(r);
    let mut l = vec!["Alle Programme werden beendet.".to_string(), format!("Danach sind deine Daten wie in {stand}")];
    l.extend(when);
    l.push(match r.ours.filter(|_| ours) {
        Some((_, Some(g))) => format!("Das System startet als Generation {g}."),
        Some((_, None)) => "Das System bleibt, wie es ist.".into(),
        None => "Nicht hier eingestellt: das System wie dort festgelegt.".into(),
    });
    l
}

/// The undo's question.
pub fn undo_lines(r: &Rewind) -> Vec<String> {
    let (stand, when, ours) = pending_words(r);
    let mut l = Vec::new();
    if ours {
        l.push(format!("Das Zurücksetzen auf {stand}"));
        l.extend(when);
        l.push("wird zurückgenommen: Daten und System bleiben,".into());
        l.push("wie sie jetzt sind.".into());
    } else {
        l.push(format!("Eingestellt ist {stand}"));
        l.extend(when);
        l.push("Zurücknehmen geht hier nur, was hier eingestellt wurde.".into());
        l.push("Sonst gilt es beim nächsten Start weiter.".into());
    }
    l
}

/// What the "Alles" side explains before anything is asked.
pub const ALL_TEXT: [&str; 4] = [
    "Daten und System gehen beim nächsten Start auf diesen Zeitpunkt zurück:",
    "deine Dateien, die Programme und ihre Einstellungen wie damals, und die Systemversion von damals.",
    "Was neuer ist, geht nicht verloren: es bleibt als Stand „vor dem Zurücksetzen“ erhalten.",
    "Bis zum Neustart lässt sich das hier rückgängig machen, danach mit diesem Stand.",
];

/// The German sentence for a refusal or failure of bitssnap (`error` of its answer, or the client's reason).
pub fn failed(code: &str) -> &'static str {
    match code {
        "needs_confirm" | "not_confirmer" => "Der Dienst hat die Bestätigung nicht angenommen: nichts geändert.",
        "not_owner" => "Der Dienst nimmt von diesem Benutzer nichts an.",
        "not_yours" => "Zurückholen geht nur in deinem eigenen Ordner: nichts geändert.",
        "bad_request" => "Der Dienst hat die Anfrage nicht verstanden: nichts geändert.",
        "absent" => "Der Dienst für gespeicherte Stände läuft nicht.",
        "late" => "Der Dienst für gespeicherte Stände antwortet nicht.",
        "busy" => "Der Dienst ist gerade beschäftigt: versuch es gleich noch einmal.",
        _ => "Das hat nicht geklappt (Einzelheiten im Protokoll).",
    }
}

#[cfg(test)]
#[path = "rewind_tests.rs"]
mod tests;
