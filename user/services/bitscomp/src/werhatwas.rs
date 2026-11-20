//! DB2: the page "Wer hat was gemacht" (`werhatwas`, `einstellungen/werhatwas`): what bitsaccess answered for
//! `summary` (user/services/bitsaccess/src/query.rs), as rows of the settings page's drawing. The time filter
//! (Heute, 7 Tage, 30 Tage) and the switch first; then each program, newest first, with its folders, hosts,
//! devices and rights; BitsOS's own programs together at the end, one row each. Plain German, no paths in the
//! labels (the folder's name; the whole path in the line under it), and a "Neu" where a program touched a kind of
//! thing for the first time.

use mcpreg::Json;

use crate::ki::Built;
use crate::page::{Kind, Section};
use crate::page_draw::Tone;

pub const ADDRESS: &str = "einstellungen/werhatwas";
pub const TITLE: &str = "Wer hat was gemacht";
pub const HINT: &str = "Was jedes Programm angefasst hat: Ordner, Netz, Geräte und Rechte. Alles bleibt auf diesem Rechner.";
pub const RANGE_KEY: &str = "werhatwas.zeitraum";
pub const SWITCH_KEY: &str = "access.enabled";
pub const RANGES: [(&str, &str); 3] = [("heute", "Heute"), ("7", "7 Tage"), ("30", "30 Tage")];
/// The settings group's last row: the way to this page (a key settingsd never has).
pub const LINK_KEY: &str = "access.ansehen";

pub fn link_row() -> crate::page::Row {
    crate::page::Row {
        key: LINK_KEY.into(),
        kind: Kind::Link("werhatwas".into()),
        default: String::new(),
        label: "Ansehen, wer was gemacht hat".into(),
        short: "Wer hat was gemacht".into(),
        note: String::new(),
        confirm: false,
        stack: false,
    }
}

/// Rows one program shows at most per part; the rest is said in one row.
pub const FOLDERS: usize = 6;
pub const HOSTS: usize = 5;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Folder {
    pub folder: String,
    pub private: bool,
    pub read: u64,
    pub write: u64,
    pub create: u64,
    pub delete: u64,
    pub rename: u64,
    pub last: u64,
    pub new: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Host {
    pub words: String,
    pub port_words: String,
    pub proto: String,
    pub connect: u64,
    pub accept: u64,
    pub listen: u64,
    pub tx: u64,
    pub rx: u64,
    pub last: u64,
    pub new: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Device {
    pub label: String,
    pub paths: Vec<String>,
    pub count: u64,
    pub last: u64,
    pub new: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prog {
    pub app: String,
    pub name: String,
    pub system: bool,
    pub new: bool,
    /// How often it was started in the range.
    pub started: u64,
    pub last: u64,
    pub files: Vec<Folder>,
    pub files_more: u64,
    pub net: Vec<Host>,
    pub net_more: u64,
    pub devices: Vec<Device>,
    /// (kind, what, count)
    pub rights: Vec<(String, String, u64)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    pub loaded: bool,
    /// Why there is nothing to show (the service does not answer).
    pub error: Option<String>,
    pub on: bool,
    pub lost: u64,
    pub programs: Vec<Prog>,
}

fn u(j: &Json, k: &str) -> u64 {
    j.get(k).and_then(Json::as_u64).unwrap_or(0)
}
fn s(j: &Json, k: &str) -> String {
    j.get(k).and_then(Json::as_str).unwrap_or("").to_string()
}
fn b(j: &Json, k: &str) -> bool {
    j.get(k).and_then(Json::as_bool).unwrap_or(false)
}
fn arr<'a>(j: &'a Json, k: &str) -> &'a [Json] {
    j.get(k).and_then(Json::as_array).unwrap_or(&[])
}

/// The `result` of a `summary` answer.
pub fn parse(r: &Json) -> Data {
    let programs = arr(r, "programs")
        .iter()
        .map(|p| Prog {
            app: s(p, "app"),
            name: s(p, "name"),
            system: b(p, "system"),
            new: b(p, "new"),
            started: u(p, "started"),
            last: u(p, "last"),
            files: arr(p, "files")
                .iter()
                .map(|f| Folder { folder: s(f, "folder"), private: b(f, "private"), read: u(f, "read"), write: u(f, "write"), create: u(f, "create"), delete: u(f, "delete"), rename: u(f, "rename"), last: u(f, "last"), new: b(f, "new") })
                .collect(),
            files_more: u(p, "files_more"),
            net: arr(p, "net")
                .iter()
                .map(|n| Host { words: s(n, "words"), port_words: s(n, "port_words"), proto: s(n, "proto"), connect: u(n, "connect"), accept: u(n, "accept"), listen: u(n, "listen"), tx: u(n, "tx"), rx: u(n, "rx"), last: u(n, "last"), new: b(n, "new") })
                .collect(),
            net_more: u(p, "net_more"),
            devices: arr(p, "devices")
                .iter()
                .map(|d| Device { label: s(d, "label"), paths: arr(d, "paths").iter().filter_map(|x| x.as_str().map(str::to_string)).collect(), count: u(d, "count"), last: u(d, "last"), new: b(d, "new") })
                .collect(),
            rights: arr(p, "rights").iter().map(|x| (s(x, "kind"), s(x, "what"), u(x, "count"))).collect(),
        })
        .collect();
    Data { loaded: true, error: None, on: b(r, "on"), lost: u(r, "lost"), programs }
}

/// Seconds back for a range word: since local midnight for "heute".
pub fn since_s(range: &str, now_ms: u64, off_min: i32) -> u64 {
    match range {
        "7" => 7 * 86_400,
        "30" => 30 * 86_400,
        _ => ((now_ms / 1000) as i64 + off_min as i64 * 60).rem_euclid(86_400) as u64 + 1,
    }
}

/// "14:32" today, "gestern 14:32", else "am 12.11. 14:32" (local time).
pub fn when(t: u64, now: u64, off_min: i32) -> String {
    let local = |ms: u64| (ms / 1000) as i64 + off_min as i64 * 60;
    let (d, n) = (local(t).div_euclid(86_400), local(now).div_euclid(86_400));
    let sec = local(t).rem_euclid(86_400);
    let hm = format!("{}:{:02}", sec / 3600, sec % 3600 / 60);
    match n - d {
        0 => hm,
        1 => format!("gestern {hm}"),
        _ => {
            let (_, m, day) = civil(d);
            format!("am {day}.{m}. {hm}")
        }
    }
}

/// (year, month, day) of days since 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// 1.234.567 bytes as "1,2 MB" (German comma).
pub fn bytes(n: u64) -> String {
    let f = |v: f64, unit: &str| format!("{v:.1} {unit}").replace('.', ",");
    match n {
        0..1000 => format!("{n} Byte"),
        1000..1_000_000 => f(n as f64 / 1e3, "kB"),
        1_000_000..1_000_000_000 => f(n as f64 / 1e6, "MB"),
        _ => f(n as f64 / 1e9, "GB"),
    }
}

fn times(n: u64, what: &str) -> Option<String> {
    (n > 0).then(|| format!("{n}× {what}"))
}

fn folder_name(path: &str) -> &str {
    match path.trim_end_matches('/').rsplit('/').next() {
        Some(n) if !n.is_empty() => n,
        _ => "Hauptordner",
    }
}

/// Only what is new stands out; a private folder says so in words.
fn new_tone(new: bool) -> Tone {
    if new { Tone::Bad } else { Tone::Plain }
}

fn right_words(kind: &str, what: &str) -> String {
    match kind {
        "setid" => "mit Sonderrechten gestartet".into(),
        "caps" => "hat sich mehr Rechte gegeben".into(),
        "ptrace" => format!("wollte ein anderes Programm beobachten (Nummer {what})"),
        _ => kind.into(),
    }
}

/// The page's rows for `d` in `range`; `enabled`: access.enabled as settingsd says.
pub fn build(d: &Data, range: &str, enabled: bool, now: u64, off: i32) -> Built {
    let mut out = Built::default();
    let fixed = |why: &str| Kind::Fixed(why.to_string());
    let opts: Vec<(String, String)> = RANGES.iter().map(|(v, n)| (v.to_string(), n.to_string())).collect();
    let w = crate::page::pill_w(opts.iter().map(|(_, n)| n.as_str()));
    out.row(RANGE_KEY, Kind::Pills { opts, multi: false, w, notes: Vec::new() }, "Zeitraum", "Zeitraum", range, crate::ki::plain("Welche Zeit die Liste zeigt"));
    out.row(SWITCH_KEY, Kind::Switch, "Aufschreiben", "Aufschreiben", "", crate::ki::plain(""));
    switch_now(&mut out, enabled);
    if d.lost > 0 {
        out.row("werhatwas.verloren", fixed(""), "Lücken", "Lücken", &format!("{} Einträge", d.lost), (String::from("So viele Zugriffe kamen so schnell, dass BitsOS sie nicht alle aufschreiben konnte."), Tone::Bad));
    }
    out.section("ANZEIGE", 0);
    let from = out.model.rows.len();
    if let Some(e) = &d.error {
        out.row("werhatwas.fehlt", fixed(""), "Keine Antwort", "Keine Antwort", "", (e.clone(), Tone::Bad));
        out.section("PROGRAMME", from);
        return out;
    }
    if !d.loaded {
        out.row("werhatwas.laedt", fixed(""), "Wird geladen", "Wird geladen", "", crate::ki::plain(""));
        out.section("PROGRAMME", from);
        return out;
    }
    let (own, system): (Vec<&Prog>, Vec<&Prog>) = d.programs.iter().partition(|p| !p.system);
    if own.is_empty() && system.is_empty() {
        out.row("werhatwas.leer", fixed(""), "Noch nichts", "Noch nichts", "", crate::ki::plain("In diesem Zeitraum hat kein Programm etwas angefasst, das BitsOS aufschreibt."));
        out.section("PROGRAMME", from);
    }
    for (i, p) in own.iter().enumerate() {
        let from = out.model.rows.len();
        program_rows(&mut out, &format!("werhatwas.p{i}"), p, now, off);
        let head = if p.new { format!("{} · NEU", p.name.to_uppercase()) } else { p.name.to_uppercase() };
        out.section(&head, from);
    }
    if !system.is_empty() {
        let from = out.model.rows.len();
        for (i, p) in system.iter().enumerate().take(40) {
            let parts = [
                (!p.files.is_empty()).then(|| format!("{} Ordner", p.files.len() as u64 + p.files_more)),
                (!p.net.is_empty()).then(|| match p.net.len() as u64 + p.net_more {
                    1 => "1 Adresse".to_string(),
                    n => format!("{n} Adressen"),
                }),
                (!p.devices.is_empty()).then(|| format!("Geräte: {}", p.devices.iter().map(|d| d.label.as_str()).collect::<Vec<_>>().join(", "))),
                (!p.rights.is_empty()).then(|| "Rechte".to_string()),
            ];
            let text: Vec<String> = parts.into_iter().flatten().collect();
            let tone = if p.new { Tone::Bad } else { Tone::Plain };
            let line = format!("{}{}", text.join(" · "), if p.new { " · Neu" } else { "" });
            out.row(&format!("werhatwas.s{i}"), fixed(&when(p.last, now, off)), &p.name, &p.name, "", (line, tone));
        }
        out.section("BITSOS SELBST", from);
    }
    out
}

/// The switch's value and line as settingsd says now (the rest of the rows changes only with the data).
pub fn switch_now(b: &mut Built, enabled: bool) {
    let Some(i) = b.row_of(SWITCH_KEY) else { return };
    let sub = if enabled { "BitsOS schreibt auf, wer was anfasst. Es bleibt auf diesem Rechner." } else { "Ausgeschaltet: BitsOS schreibt nichts auf." };
    b.values[i] = enabled.to_string();
    b.subs[i] = Some(crate::ki::plain(sub));
}

fn program_rows(out: &mut Built, key: &str, p: &Prog, now: u64, off: i32) {
    let fixed = |why: String| Kind::Fixed(why);
    for (j, f) in p.files.iter().take(FOLDERS).enumerate() {
        let what: Vec<String> = [times(f.read, "gelesen"), times(f.write, "geschrieben"), times(f.create, "angelegt"), times(f.delete, "gelöscht"), times(f.rename, "umbenannt")].into_iter().flatten().collect();
        let label = format!("Dateien: {}", folder_name(&f.folder));
        let mut sub = f.folder.clone();
        if f.private {
            sub.push_str(" · privater Ordner");
        }
        if f.new {
            sub.push_str(" · Neu: zum ersten Mal");
        }
        out.row(&format!("{key}.f{j}"), fixed(when(f.last, now, off)), &label, &label, &what.join(", "), (sub, new_tone(f.new)));
    }
    let more = p.files.len().saturating_sub(FOLDERS) as u64 + p.files_more;
    if more > 0 {
        out.row(&format!("{key}.fmehr"), fixed(String::new()), "Weitere Ordner", "Weitere Ordner", &format!("{more}"), crate::ki::plain("Nicht einzeln gezeigt"));
    }
    for (j, h) in p.net.iter().take(HOSTS).enumerate() {
        let what: Vec<String> = [times(h.connect, "verbunden"), times(h.accept, "angenommen"), times(h.listen, "wartet auf Verbindungen")].into_iter().flatten().collect();
        let volume = if h.tx > 0 || h.rx > 0 { format!(" · {} gesendet, {} empfangen", bytes(h.tx), bytes(h.rx)) } else { String::new() };
        let label = format!("Netz: {}", h.words);
        let sub = format!("{}{volume}{}", h.port_words, if h.new { " · Neu: zum ersten Mal" } else { "" });
        out.row(&format!("{key}.n{j}"), fixed(when(h.last, now, off)), &label, &label, &what.join(", "), (sub, new_tone(h.new)));
    }
    let more = p.net.len().saturating_sub(HOSTS) as u64 + p.net_more;
    if more > 0 {
        out.row(&format!("{key}.nmehr"), fixed(String::new()), "Weitere Adressen", "Weitere Adressen", &format!("{more}"), crate::ki::plain("Nicht einzeln gezeigt"));
    }
    for (j, d) in p.devices.iter().enumerate() {
        let label = format!("Gerät: {}", d.label);
        // The device nodes' names are code words: the page says how many (`ctl werhatwas` and events name them).
        let parts = [(d.paths.len() > 1).then(|| format!("{} Geräte", d.paths.len())), d.new.then(|| "Neu: zum ersten Mal".to_string())];
        let sub = parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
        out.row(&format!("{key}.g{j}"), fixed(when(d.last, now, off)), &label, &label, &format!("{}× geöffnet", d.count), (sub, new_tone(d.new)));
    }
    for (j, (kind, what, n)) in p.rights.iter().enumerate() {
        out.row(&format!("{key}.r{j}"), fixed(String::new()), "Rechte", "Rechte", &format!("{n}×"), (right_words(kind, what), Tone::Bad));
    }
    if p.files.is_empty() && p.net.is_empty() && p.devices.is_empty() && p.rights.is_empty() {
        let value = if p.started > 0 { format!("{}×", p.started) } else { String::new() };
        out.row(&format!("{key}.nur"), fixed(when(p.last, now, off)), "Gestartet", "Gestartet", &value, crate::ki::plain("Sonst hat es nichts angefasst, das BitsOS aufschreibt."));
    }
}

/// The rows as text for `ctl werhatwas` (scenarios read the page this way): one line each.
pub fn rows_text(b: &Built) -> String {
    let mut out = String::new();
    for sec in &b.model.sections {
        let Section::Rows(head, range) = sec else { continue };
        out.push_str(&format!("[{head}]\n"));
        for r in range.clone() {
            let row = &b.model.rows[r];
            let why = match &row.kind {
                Kind::Fixed(w) => w.as_str(),
                _ => "",
            };
            let sub = b.subs.get(r).and_then(|s| s.as_ref()).map_or("", |s| s.0.as_str());
            out.push_str(&format!("{} | {} | {} | {}\n", row.label, b.values.get(r).map_or("", |v| v.as_str()), why, sub));
        }
    }
    out
}

#[cfg(test)]
#[path = "werhatwas_tests.rs"]
mod tests;
