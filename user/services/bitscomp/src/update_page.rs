//! UPDATE: the page "Aktualisieren" (`einstellungen/aktualisieren`): which version is installed, the newest one
//! in the workshop repository with its "Was ist neu", the buttons Aktualisieren (download, check, install; then
//! Jetzt neu starten / Später) and Zurück zur vorherigen Version, the GitHub token's field and every failure in
//! one plain line. Pure: built from what bitspkgd's update ops answered (user/services/bitspkgd/src/update.rs).
//! The token is never part of it: the row knows only "gespeichert" or "fehlt".

use mcpreg::Json;

use crate::ki::Built;
use crate::page::{Kind, Row};
use crate::page_draw::Tone;

pub const ADDRESS: &str = "einstellungen/aktualisieren";
pub const TITLE: &str = "Aktualisieren";
pub const HINT: &str = "Neue Versionen von BitsOS aus deinem Werkstatt-Archiv, von deinem Desk unterschrieben. Nichts wird ohne dich installiert.";
/// The settings' way to this page (System, card Kern; a key settingsd never has).
pub const LINK_KEY: &str = "kernel.aktualisieren";
pub const TOKEN_KEY: &str = "update.token";
pub const BUTTONS_KEY: &str = "update.knoepfe";
pub const RESTART_KEY: &str = "update.neustart";
pub const BACK_KEY: &str = "update.zurueck";
pub const STORED: &str = "gespeichert";
pub const MISSING: &str = "fehlt";

pub fn link_row() -> Row {
    Row {
        key: LINK_KEY.into(),
        kind: Kind::Link("aktualisieren".into()),
        default: String::new(),
        label: "BitsOS aktualisieren".into(),
        short: "Aktualisieren".into(),
        note: String::new(),
        confirm: false,
        stack: false,
    }
}

/// What the page knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    /// bitspkgd answered `update_status` at least once.
    pub loaded: bool,
    /// The service does not answer (the German reason).
    pub error: Option<String>,
    pub installed: String,
    pub pending: bool,
    pub pin: Option<u64>,
    pub back: Option<u64>,
    pub token: bool,
    /// (version, date, notes) of a newer version.
    pub offer: Option<(u64, String, String)>,
    /// UPD-4: what it brings (Kern, Schreibtisch, Programme with their megabytes), as bitsupdate says it.
    pub comes: Option<String>,
    pub current: bool,
    /// A check, an install or a pin is on its way.
    pub busy: bool,
    /// The step of a running install.
    pub progress: Option<String>,
    /// The last answer's line and whether it was a failure.
    pub said: Option<(String, bool)>,
    /// A version was installed in this session: "Jetzt neu starten" / "Später".
    pub restart: bool,
}

/// `update_status`'s answer into `d`.
pub fn status(d: &mut Data, j: &Json) {
    d.loaded = true;
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        d.error = Some(j.get("message").and_then(Json::as_str).unwrap_or("Keine Antwort.").to_string());
        return;
    }
    d.error = None;
    let i = j.get("installed");
    d.installed = i.and_then(|i| i.get("text")).and_then(Json::as_str).unwrap_or("").to_string();
    d.pending = i.and_then(|i| i.get("pending")).and_then(Json::as_bool).unwrap_or(false);
    d.pin = i.and_then(|i| i.get("pin")).and_then(Json::as_u64);
    d.back = i.and_then(|i| i.get("back")).and_then(Json::as_u64);
    d.token = j.get("token").and_then(Json::as_str) == Some("***");
    if let Some(o) = j.get("offer").filter(|o| !matches!(o, Json::Null)) {
        d.offer = offer(o);
        d.comes = comes(o);
    }
}

fn comes(o: &Json) -> Option<String> {
    o.get("comes").and_then(Json::as_str).map(str::to_string)
}

fn offer(o: &Json) -> Option<(u64, String, String)> {
    Some((o.get("version")?.as_u64()?, o.get("date").and_then(Json::as_str).unwrap_or("").to_string(), o.get("notes").and_then(Json::as_str).unwrap_or("").to_string()))
}

/// `update_check`'s answer.
pub fn checked(d: &mut Data, j: &Json) {
    d.busy = false;
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        d.said = Some((j.get("message").and_then(Json::as_str).unwrap_or("Keine Antwort.").to_string(), true));
        return;
    }
    d.current = j.get("current").and_then(Json::as_bool) == Some(true);
    d.offer = j.get("offer").and_then(offer);
    d.comes = j.get("offer").and_then(comes);
    d.said = None;
}

/// `update_do`'s last line.
pub fn done(d: &mut Data, j: &Json) {
    d.busy = false;
    d.progress = None;
    let ok = j.get("ok").and_then(Json::as_bool) == Some(true);
    let msg = j.get("message").and_then(Json::as_str).unwrap_or(if ok { "Installiert." } else { "Nicht installiert." }).to_string();
    if ok && j.get("installed").is_some() {
        d.restart = true;
        d.offer = None;
    }
    d.said = Some((msg, !ok));
}

fn fixed() -> Kind {
    Kind::Fixed(String::new())
}

fn plain(t: &str) -> (String, Tone) {
    (t.to_string(), Tone::Plain)
}

pub fn build(d: &Data) -> Built {
    let mut out = Built::default();
    if let Some(e) = &d.error {
        out.row("update.fehlt", fixed(), "Keine Antwort", "Keine Antwort", "", (e.clone(), Tone::Bad));
        out.section("AKTUALISIEREN", 0);
        return out;
    }
    if !d.loaded {
        out.row("update.laedt", fixed(), "Wird geladen", "Wird geladen", "", plain(""));
        out.section("AKTUALISIEREN", 0);
        return out;
    }
    let line = if d.pending {
        "Diese Version wird gerade erprobt: benutz den Schreibtisch ein, zwei Minuten, dann bleibt sie.".to_string()
    } else {
        match d.pin {
            Some(p) => format!("Du hast „Zurück“ gewählt: beim nächsten Start kommt die Version {p}."),
            None => String::from("Beim nächsten Start kommt diese Version, oder eine neue, die du installiert hast."),
        }
    };
    out.row("update.installiert", fixed(), "Installiert", "Installiert", &d.installed, plain(&line));
    out.section("INSTALLIERT", 0);

    let from = out.model.rows.len();
    if let Some(p) = &d.progress {
        out.row("update.schritt", fixed(), "Läuft", "Läuft", "", plain(p));
    } else if d.busy {
        out.row("update.schritt", fixed(), "Läuft", "Läuft", "", plain("Einen Moment …"));
    }
    if let Some((t, bad)) = &d.said {
        out.row("update.meldung", fixed(), if *bad { "Nicht geklappt" } else { "Erledigt" }, "Meldung", "", (t.clone(), if *bad { Tone::Bad } else { Tone::Plain }));
    }
    if d.restart {
        out.row(RESTART_KEY, Kind::Buttons(vec![("neustart".into(), "Jetzt neu starten".into()), ("spaeter".into(), "Später".into())]), "Neu starten", "Neu starten", "", plain("Die neue Version startet beim nächsten Neustart."));
    } else if let Some((v, date, notes)) = &d.offer {
        let head = if date.is_empty() { format!("Version {v}") } else { format!("Version {v} vom {date}") };
        out.row("update.neu", fixed(), "Neu", "Neu", &head, plain(if notes.is_empty() { "Was ist neu: (keine Angaben)" } else { notes }));
        if let Some(c) = &d.comes {
            out.row("update.kommt", fixed(), "Es kommt", "Es kommt", "", plain(c));
        }
        if !d.busy {
            out.row(BUTTONS_KEY, Kind::Buttons(vec![("aktualisieren".into(), "Aktualisieren".into())]), "Aktualisieren", "Aktualisieren", "", plain("Laden, prüfen, installieren. Danach ein Neustart."));
        }
    } else if d.current {
        out.row("update.aktuell", fixed(), "Neueste", "Neueste", "", plain("Du hast die neueste Version."));
    }
    if !d.busy && !d.restart {
        out.row("update.pruefen", Kind::Buttons(vec![("pruefen".into(), "Nach neuer Version sehen".into())]), "Prüfen", "Prüfen", "", plain(""));
    }
    out.section("NEUESTE VERSION", from);

    let from = out.model.rows.len();
    match (d.pin, d.back) {
        (Some(_), _) => out.row(BACK_KEY, Kind::Buttons(vec![("vor".into(), "Wieder die neueste Version starten".into())]), "Neueste", "Neueste", "", plain("Hebt „Zurück“ auf.")),
        (None, Some(b)) => out.row(BACK_KEY, Kind::Buttons(vec![("zurueck".into(), "Zurück zur vorherigen Version".into())]), "Zurück", "Zurück", "", plain(&format!("Beim nächsten Start kommt die Version {b}. Die neue bleibt installiert."))),
        (None, None) => out.row(BACK_KEY, fixed(), "Zurück", "Zurück", "", plain("Keine ältere bestätigte Version auf dem Startdatenträger.")),
    }
    out.section("ZURÜCK", from);

    let from = out.model.rows.len();
    out.row(TOKEN_KEY, Kind::Secret { clear: true }, "GitHub-Schlüssel", "GitHub-Schlüssel", if d.token { STORED } else { MISSING }, plain(bitsupdate_help()));
    out.section("GITHUB-SCHLÜSSEL", from);
    out
}

/// Where Georg gets the token (bitsupdate's text::TOKEN_HELP, said here too: bitscomp does not link bitsupdate).
pub fn bitsupdate_help() -> &'static str {
    "Den Schlüssel machst du einmal auf github.com (die Knöpfe heißen dort englisch): Settings › Developer settings › Personal access tokens › Fine-grained tokens › Generate new token. Nur das Archiv bitsos-werkstatt, bei „Contents“ nur „Read-only“. Dann hier einfügen."
}

/// The rows as text for `ctl aktualisieren` (label | value | line).
pub fn rows_text(b: &Built) -> String {
    let mut t = String::new();
    for (i, r) in b.model.rows.iter().enumerate() {
        let sub = b.subs.get(i).and_then(|s| s.as_ref()).map_or("", |s| s.0.as_str());
        let buttons = match &r.kind {
            Kind::Buttons(v) => format!(" [{}]", v.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(",")),
            _ => String::new(),
        };
        t.push_str(&format!("{} | {}{buttons} | {}\n", r.label, b.values.get(i).map_or("", |v| v.as_str()), sub));
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(t: &str) -> Json {
        Json::parse(t).unwrap()
    }

    #[test]
    fn the_page_says_what_is_installed_new_and_wrong() {
        let mut d = Data::default();
        assert!(rows_text(&build(&d)).starts_with("Wird geladen"));
        status(&mut d, &j(r#"{"ok":true,"installed":{"version":165,"text":"Version 165 vom 16.11.2026","pending":false,"pin":null,"back":164},"token":null,"offer":null}"#));
        let t = rows_text(&build(&d));
        assert!(t.contains("Installiert | Version 165 vom 16.11.2026"), "{t}");
        assert!(t.contains("Zurück |  [zurueck] | Beim nächsten Start kommt die Version 164."), "{t}");
        assert!(t.contains("GitHub-Schlüssel | fehlt | Den Schlüssel machst du einmal auf github.com"), "{t}");
        checked(&mut d, &j(r#"{"ok":true,"offer":{"version":166,"date":"16.11.2026","notes":"Was ist neu: das Aktualisieren."}}"#));
        let t = rows_text(&build(&d));
        assert!(t.contains("Neu | Version 166 vom 16.11.2026 | Was ist neu: das Aktualisieren."), "{t}");
        assert!(t.contains("Aktualisieren |  [aktualisieren]"), "{t}");
        assert!(!t.contains("Es kommt"), "a version without parts says nothing about them");
        // UPD-4: what a version with parts brings.
        checked(&mut d, &j(r#"{"ok":true,"offer":{"version":169,"date":"17.11.2026","notes":"Neu.","comes":"Kern 180 MB, Schreibtisch 12 MB, Programme 410 MB (Brave, Steam)."}}"#));
        let t = rows_text(&build(&d));
        assert!(t.contains("Es kommt |  | Kern 180 MB, Schreibtisch 12 MB, Programme 410 MB (Brave, Steam)."), "{t}");
        // A failure says it in one line, plain.
        done(&mut d, &j(r#"{"ok":false,"error":"check","message":"Nicht installiert: die Version ist nicht von deinem Desk unterschrieben."}"#));
        let t = rows_text(&build(&d));
        assert!(t.contains("Nicht geklappt |  | Nicht installiert: die Version ist nicht von deinem Desk unterschrieben."), "{t}");
        done(&mut d, &j(r#"{"ok":true,"installed":166,"message":"Version 166 ist installiert."}"#));
        let t = rows_text(&build(&d));
        assert!(t.contains("[neustart,spaeter]") && !t.contains("[aktualisieren]"), "{t}");
        // After the restart: on trial, then "Zurück" pinned.
        let mut p = Data::default();
        status(&mut p, &j(r#"{"ok":true,"installed":{"text":"Version 166","pending":true,"pin":164,"back":165},"token":"***"}"#));
        let t = rows_text(&build(&p));
        assert!(t.contains("wird gerade erprobt") && t.contains("[vor]") && t.contains("GitHub-Schlüssel | gespeichert"), "{t}");
        // The service is down.
        let mut e = Data::default();
        status(&mut e, &j(r#"{"ok":false,"message":"Kein Netz: GitHub ist nicht erreichbar. Nichts installiert."}"#));
        assert!(rows_text(&build(&e)).contains("Kein Netz"));
        assert_eq!(link_row().kind, Kind::Link("aktualisieren".into()));
    }
}
