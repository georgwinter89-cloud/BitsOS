//! PK-7a: the "Programme" page's German words: numbers of bytes, the reasons of a refusal (Signatur, Platz,
//! Spiegel), the progress of a run, and the plan card's lines as the layout counts them and the look draws them.

use mcpreg::Json;

use crate::programme::{Action, Card, Phase};

/// Bytes the German way: `1,5 GB`, `340 MB`, `12 KB`.
pub fn bytes(b: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("KB", 1e3)];
    for (u, f) in UNITS {
        let v = b as f64 / f;
        if v >= 1.0 {
            let t = if v < 10.0 { format!("{v:.1}") } else { format!("{v:.0}") };
            return format!("{} {u}", t.replace('.', ","));
        }
    }
    format!("{b} Bytes")
}

/// Words of the system or a library (English) that come through bitspkgd's message: a fixed German sentence
/// with its kind instead (the raw text goes to the log, B6).
const FOREIGN: [(&[&str], &str, &str); 9] = [
    (&["no space left", "os error 28", "enospc", "disk quota"], "Platz", "die Platte ist voll"),
    (&["read-only file system", "os error 30"], "Platz", "das Dateisystem ist nur lesbar"),
    (&["certificate", "tls handshake", "tls:", "x509"], "Spiegel", "die gesicherte Verbindung zum Spiegel ging nicht (Zertifikat)"),
    (&["timed out", "os error 110"], "Spiegel", "der Spiegel antwortete nicht rechtzeitig"),
    (&["connection refused", "connection reset", "network is unreachable", "os error 111", "os error 101", "failed to lookup", "name resolution", "dns"], "Spiegel", "keine Verbindung zum Spiegel (Netz?)"),
    (&["checksum", "sha256", "hash mismatch", "digest"], "Signatur", "die Prüfsumme stimmt nicht: das Paket ist beschädigt"),
    (&["bad signature", "invalid signature", "signature mismatch"], "Signatur", "die Signatur stimmt nicht"),
    (&["unknown key", "untrusted", "no public key", "key expired", "expired key"], "Signatur", "der Schlüssel ist unbekannt oder abgelaufen"),
    (&["permission denied", "os error 13"], "", "kein Zugriff (Rechte)"),
];

/// bitspkgd's `error`/`message` as a German reason that names its kind.
pub fn reason(error: &str, message: &str) -> String {
    let m = message.trim();
    let low = m.to_lowercase();
    if let Some((_, kind, de)) = FOREIGN.iter().find(|(pats, _, _)| pats.iter().any(|p| low.contains(p))) {
        return if kind.is_empty() { de.to_string() } else { format!("{kind}: {de}") };
    }
    let kind = if low.contains("signatur") || low.contains("pgp") || low.contains("signature") || low.contains("schlüssel") || low.contains("prüfsumme") {
        "Signatur"
    } else if low.contains("platz") || low.contains("space") || low.contains("max_gb") || low.contains("voll") {
        "Platz"
    } else if low.contains("spiegel") || low.contains("mirror") || low.contains("herunterladen") || low.contains("download") || low.contains("fetch") || low.contains("verbindung") {
        "Spiegel"
    } else {
        ""
    };
    let text = match error {
        "busy" => "die Programmverwaltung arbeitet gerade an etwas anderem; gleich noch einmal".to_string(),
        "read_only" => "nur lesen: ändern dürfen nur root und der Compositor".to_string(),
        "unknown_plan" => "der Plan ist abgelaufen; bitte neu planen".to_string(),
        "timeout" => format!("dauerte zu lange ({m})"),
        _ if m.is_empty() => if error.is_empty() { "ohne Grund".to_string() } else { error.to_string() },
        _ => m.to_string(),
    };
    if kind.is_empty() || text.starts_with(kind) { text } else { format!("{kind}: {text}") }
}

/// The line for sources not loaded or old (`sources` op: `packages` null when not loaded, `age_days`); None when
/// every source is loaded and fresh.
pub fn sources_line(list: &[Json]) -> Option<String> {
    if list.is_empty() {
        return Some("Keine Paketquelle eingerichtet: unter „Quellen und Platz“ einstellen.".into());
    }
    let name = |j: &Json| j.get("name").and_then(Json::as_str).unwrap_or("?").to_string();
    let missing: Vec<String> = list.iter().filter(|j| j.get("packages").and_then(Json::as_u64).is_none()).map(name).collect();
    if !missing.is_empty() {
        return Some(format!("Paketquellen noch nicht geladen: {}. Ohne sie findet die Suche nichts; „Jetzt prüfen“ unter Aktualisierungen lädt sie (braucht Netz).", missing.join(", ")));
    }
    let age = list.iter().filter_map(|j| j.get("age_days").and_then(Json::as_i64)).max().unwrap_or(0);
    (age > crate::programme::STALE_DAYS).then(|| format!("Paketquellen zuletzt vor {age} Tagen geladen: ohne Netz findet die Suche nichts Neues; „Jetzt prüfen“ lädt sie."))
}

pub fn running(a: Action, name: &str) -> String {
    match a {
        Action::Install => format!("{name} wird installiert …"),
        Action::Remove => format!("{name} wird entfernt …"),
        Action::Update => format!("{name} wird aktualisiert …"),
    }
}

/// The German names of the run's phases (bitspkgd progress.rs `rank` order).
pub const PHASES: [&str; 5] = ["Herunterladen", "Signatur prüfen", "Auspacken", "Einrichten", "Umschalten"];

/// One `progress` line into the card.
pub fn progress(c: &mut Card, j: &Json) {
    let phase = j.get("progress").and_then(Json::as_str).unwrap_or("");
    if phase == "report" {
        if let Some(t) = j.get("text").and_then(Json::as_str) {
            c.report.push(t.to_string());
        }
        return;
    }
    let Some(r) = ["download", "verify", "unpack", "triggers", "switch"].iter().position(|p| *p == phase) else { return };
    // Never backwards (a late download line after verify started).
    if c.rank.is_some_and(|now| r < now) {
        return;
    }
    c.step_done = j.get("step").and_then(Json::as_str) == Some("done");
    c.rank = Some(r);
    if let (Some(d), Some(t)) = (j.get("done_bytes").and_then(Json::as_u64), j.get("total_bytes").and_then(Json::as_u64)) {
        c.bytes = (d, t.max(d));
    }
    c.line = match r {
        0 if c.bytes.1 > 0 => format!("Herunterladen: {} von {}", bytes(c.bytes.0), bytes(c.bytes.1)),
        _ => format!("{}{}", PHASES[r], if c.step_done { ": fertig" } else { " …" }),
    };
}

/// The line of a plan bitspkgd refused (space with its numbers, a part, a signature).
pub fn refusal(c: &Card) -> String {
    if let Some((free, limit, used, false)) = c.space {
        return format!(
            "Platz: zu wenig. {} braucht {} (Download {}); frei sind {}, für Programme höchstens {} (belegt {}). Nichts wurde heruntergeladen.",
            c.name,
            bytes(c.installed),
            bytes(c.download),
            bytes(free),
            bytes(limit),
            bytes(used)
        );
    }
    match c.refused.first() {
        Some(r) => format!("Nicht installierbar: {}", reason("", r)),
        None if !c.summary.is_empty() => c.summary.clone(),
        None => "Nicht installierbar.".into(),
    }
}

pub fn finished(a: Action, name: &str, j: &Json) -> String {
    let v = j.get("version").and_then(Json::as_str).unwrap_or("");
    match a {
        Action::Install => format!("{name} {v} ist installiert. Starten in der Bibliothek."),
        Action::Update => format!("{name} ist aktualisiert. Die alte Fassung bleibt zum Zurückgehen."),
        Action::Remove => match j.get("freed").and_then(Json::as_u64) {
            Some(f) => format!("{name} ist entfernt ({} frei).", bytes(f)),
            None => format!("{name} ist entfernt."),
        },
    }
}

pub fn failed(a: Action, name: &str, why: &str) -> String {
    let what = match a {
        Action::Install => "nicht installiert",
        Action::Remove => "nicht entfernt",
        Action::Update => "nicht aktualisiert",
    };
    format!("{name} {what}: {why}")
}

/// How a card line looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Head,
    Text,
    Muted,
    Good,
    Bad,
    /// A list entry (mono, indented).
    Item,
}

/// The card's text lines, top to bottom (the layout gives each one row, the buttons come after them).
pub fn card_lines(c: &Card) -> Vec<(String, Tone)> {
    let mut v = Vec::new();
    let title = match c.action {
        Action::Install => format!("{} installieren", c.name),
        Action::Remove => format!("{} entfernen", c.name),
        Action::Update => format!("{} aktualisieren", c.name),
    };
    v.push((title, Tone::Head));
    if c.phase == Phase::Asking {
        v.push(("Plan wird erstellt …".into(), Tone::Muted));
        return v;
    }
    if !c.summary.is_empty() {
        v.push((c.summary.clone(), Tone::Text));
    }
    if c.action != Action::Remove && !c.packages.is_empty() {
        v.push((format!("Kommt mit: {} Teile, {} auf der Platte, Download {}", c.packages.len(), bytes(c.installed), bytes(c.download)), Tone::Text));
        if c.show_list {
            v.extend(c.packages.iter().map(|p| (p.clone(), Tone::Item)));
        }
    }
    if let Some((free, limit, used, fits)) = c.space {
        let t = format!("Platz: frei {}, für Programme höchstens {}, belegt {}", bytes(free), bytes(limit), bytes(used));
        v.push((t, if fits { Tone::Muted } else { Tone::Bad }));
    }
    if c.action == Action::Install && !c.permissions.is_empty() {
        v.push(("Wird fragen dürfen:".into(), Tone::Text));
        v.extend(c.permissions.iter().map(|p| (p.clone(), Tone::Item)));
    }
    if c.action == Action::Remove {
        let data = if c.keep_data {
            "Deine Daten bleiben (Meine Daten behalten).".to_string()
        } else if c.data_removed {
            format!("Der Datenordner ({}) wird mit gelöscht.", bytes(c.data))
        } else {
            "Es gibt keinen Datenordner.".into()
        };
        v.push((data, Tone::Text));
        v.push(("Andere Programme bleiben unberührt (jedes hat seine eigenen Teile).".into(), Tone::Muted));
    }
    v.extend(c.refused.iter().filter(|r| !r.starts_with("Platz:")).map(|r| (format!("Abgelehnt: {r}"), Tone::Bad)));
    if !c.extras.is_empty() {
        v.push(("Zusätze:".into(), Tone::Text));
    }
    v
}

/// The lines under the card's boxes and buttons: the run, its result and the last report lines.
pub fn card_foot(c: &Card) -> Vec<(String, Tone)> {
    let mut v = Vec::new();
    if !c.line.is_empty() {
        let tone = match c.phase {
            Phase::Refused | Phase::Failed => Tone::Bad,
            Phase::Done => Tone::Good,
            _ => Tone::Text,
        };
        v.push((c.line.clone(), tone));
    }
    if matches!(c.phase, Phase::Done | Phase::Failed) {
        let n = c.report.len();
        v.extend(c.report[n.saturating_sub(4)..].iter().map(|r| (r.clone(), Tone::Item)));
    }
    v
}

/// The date and time of `secs` in local time, German (`12.11.2026, 03:00`).
pub fn when(secs: i64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r writes into our own tm; t is a plain value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "?".into();
    }
    format!("{:02}.{:02}.{}, {:02}:{:02}", tm.tm_mday, tm.tm_mon + 1, tm.tm_year + 1900, tm.tm_hour, tm.tm_min)
}

/// The date alone (`12.11.2026`).
pub fn day(secs: i64) -> String {
    when(secs).split(',').next().unwrap_or("?").to_string()
}

/// "Cache leeren" while bitspkgd has no `clean` op.
pub const NO_CLEAN: &str = "Cache leeren kann die Programmverwaltung noch nicht.";
