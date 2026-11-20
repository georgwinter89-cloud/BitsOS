//! What Georg reads (plain German, no code words): the update page's lines and the console's.

use crate::flow::{Fail, Installed, Step, Taken, What};
use crate::parts::Plan;
use crate::verify::Why;

pub const TITLE: &str = "Aktualisieren";
pub const NOTHING: &str = "Nichts installiert.";
pub const TOKEN_HELP: &str = "Den Schlüssel machst du einmal auf github.com (die Knöpfe heißen dort englisch): Settings › Developer settings › Personal access tokens › Fine-grained tokens › Generate new token. Nur das Archiv bitsos-werkstatt, bei „Contents“ nur „Read-only“. Dann hier einfügen.";
pub const RESTART: &str = "Jetzt neu starten";
pub const LATER: &str = "Später";
pub const BACK: &str = "Zurück zur vorherigen Version";
pub const FORWARD: &str = "Wieder die neueste Version starten";
pub const UPDATE: &str = "Aktualisieren";
pub const CURRENT: &str = "Du hast die neueste Version.";

pub fn version(n: u32) -> String {
    if n == 0 { String::from("Entwicklungsstand (ohne Nummer)") } else { format!("Version {n}") }
}

pub fn installed(i: &Installed) -> String {
    let date = if i.date.is_empty() { String::new() } else { format!(" vom {}", german_date(&i.date)) };
    format!("{}{date}", version(i.running))
}

/// "2026-11-16" -> "16.11.2026".
pub fn german_date(d: &str) -> String {
    let p: Vec<&str> = d.split('-').collect();
    if p.len() == 3 { format!("{}.{}.{}", p[2], p[1], p[0]) } else { d.to_string() }
}

/// Megabytes as Georg reads them (at least 1 for anything not empty).
pub fn mb(bytes: u64) -> u64 {
    bytes.div_ceil(1_000_000)
}

/// A program tree's name as Georg knows the program (the folder's name otherwise).
pub fn program(name: &str) -> String {
    let known = [
        ("brave", "Brave"),
        ("steam", "Steam"),
        ("libreoffice", "LibreOffice"),
        ("mesa", "Grafiktreiber"),
        ("vkquake", "vkQuake"),
        ("cobalt", "YouTube"),
        ("xwayland", "Fenster älterer Programme"),
        ("deqp", "Grafikprüfungen"),
        ("firmware", "Gerätedateien"),
    ];
    known.iter().find(|(k, _)| *k == name).map_or_else(|| name.to_string(), |(_, v)| v.to_string())
}

fn what(w: &What) -> String {
    match w {
        What::Kernel => String::from("den Kern"),
        What::Initrd => String::from("das Startpaket"),
        What::Desktop => String::from("den Schreibtisch"),
        What::Program(p) => format!("„{}“", program(p)),
    }
}

pub fn step(s: Step) -> String {
    match s {
        Step::Checking => String::from("Prüfe die Unterschrift …"),
        Step::Downloading { what: w, bytes } => format!("Lade {} ({} MB) …", what(&w), mb(bytes)),
        Step::Verifying => String::from("Prüfe, ob die Datei unverändert ist …"),
        Step::Writing { what: w, bytes } => format!("Schreibe {} auf den Startdatenträger ({} MB) …", what(&w), mb(bytes)),
        Step::Installing => String::from("Installiere auf dem Startdatenträger …"),
    }
}

/// What a version brings (the page's and the console's line before "Aktualisieren"): Kern, Schreibtisch and
/// Programme with the megabytes that come over the network; a part the medium holds comes not again.
pub fn comes(p: &Plan) -> String {
    let mut t = format!("Kern {} MB", mb(p.kernel.saturating_add(p.initrd)));
    let mut programs = (0u64, Vec::new(), 0usize);
    for (part, present) in p.parts.iter().zip(&p.present) {
        if part.name == crate::parts_rules::BIN {
            if *present {
                t.push_str(", Schreibtisch unverändert");
            } else {
                t.push_str(&format!(", Schreibtisch {} MB", mb(part.download_bytes())));
            }
        } else if *present {
            programs.2 += 1;
        } else {
            programs.0 = programs.0.saturating_add(part.download_bytes());
            programs.1.push(program(&part.name));
        }
    }
    if !programs.1.is_empty() {
        t.push_str(&format!(", Programme {} MB ({})", mb(programs.0), programs.1.join(", ")));
    }
    if programs.2 > 0 {
        t.push_str(&format!(", {} Programm{} unverändert", programs.2, if programs.2 == 1 { "" } else { "e" }));
    }
    t.push('.');
    t
}

pub fn done(slot: u32) -> String {
    format!("Version {slot} ist installiert. Sie startet beim nächsten Neustart; klappt der Start nicht, kommt von selbst die bisherige Version.")
}

/// After the restart, until the new version confirmed itself.
pub fn pending() -> &'static str {
    "Diese Version wird gerade erprobt: benutz den Schreibtisch ein, zwei Minuten, dann bleibt sie."
}

/// Georg's line for a failure: fixed German sentences; the technical detail (`Fail::detail`) goes to the log and
/// to the desk's line only.
pub fn fail(f: &Fail) -> String {
    match f {
        Fail::NoToken => String::from("Kein GitHub-Schlüssel eingetragen. Trag ihn unten ein."),
        Fail::Network(_) => format!("Kein Netz: GitHub ist nicht erreichbar. {NOTHING}"),
        Fail::TokenWrong => format!("Der GitHub-Schlüssel stimmt nicht (abgelaufen oder vertippt). {NOTHING}"),
        Fail::TokenNoAccess => format!("Der GitHub-Schlüssel darf das Werkstatt-Archiv nicht lesen. {NOTHING}"),
        Fail::Clock => format!("Die Uhr dieses Rechners geht falsch, so klappt die sichere Verbindung nicht. {NOTHING}"),
        Fail::Tls(_) => format!("Die sichere Verbindung zu GitHub klappte nicht. {NOTHING}"),
        Fail::Server(_) => format!("GitHub antwortete nicht wie erwartet. {NOTHING}"),
        Fail::NoRelease => String::from("Im Werkstatt-Archiv liegt noch keine Version."),
        Fail::Check(w) => why(w),
        Fail::Kernel(_) => String::from("Nicht installiert: BitsOS hat die Version beim Prüfen vor dem Schreiben abgelehnt."),
        Fail::NoSpace => String::from("Nicht installiert: auf dem Startdatenträger ist nicht genug Platz."),
        Fail::NoRoom { need, free, missing } => format!(
            "Nicht installiert: auf dem Startdatenträger fehlen {} MB. Die Version braucht bis zu {} MB, frei sind {} MB. Nichts wurde heruntergeladen.",
            mb(*missing),
            mb(*need),
            free / 1_000_000
        ),
        Fail::NoRam { need, free } => format!(
            "Nicht installiert: zum Herunterladen fehlt freier Arbeitsspeicher ({} MB nötig, {} MB frei). Schließ ein paar Programme und versuch es noch einmal.",
            mb(*need),
            free / 1_000_000
        ),
        Fail::Local(_) => format!("Auf diesem Rechner ging etwas schief. {NOTHING}"),
        Fail::Refused(_) => String::from("Das geht nur als root an der Tastatur dieses Rechners (nicht aus der Ferne, nicht als normaler Benutzer)."),
        Fail::Pin(_) => String::from("Das ging nicht: die Wahl für den nächsten Start blieb, wie sie war."),
        Fail::Taken(t) => taken(t),
    }
}

/// A release not above what the medium holds, while an older or unnumbered build runs.
pub fn taken(t: &Taken) -> String {
    match t {
        Taken::Waiting(v) => format!("Version {v} ist schon installiert und startet beim nächsten Neustart."),
        Taken::Back { version, running } => {
            format!("Version {version} ist installiert, es läuft aber {} (Zurück gewählt). „{FORWARD}“ holt sie wieder.", self::version(*running))
        }
        Taken::Failed(v) => format!("Version {v} ist installiert, ihr Start klappte aber nicht; es läuft wieder die vorherige. Der Desk muss eine neue Version machen."),
        Taken::Occupied { version, newest } => format!(
            "Version {version} kann hier nicht installiert werden: auf dem Startdatenträger liegt schon Generation {newest} (ein Entwicklungsstand oder eine andere Fassung). Der Desk muss eine höhere Nummer vergeben."
        ),
    }
}

pub fn why(w: &Why) -> String {
    match w {
        Why::NoKey => String::from("Nicht installiert: diese BitsOS-Fassung kennt den Schlüssel deines Desks noch nicht."),
        Why::Signature => String::from("Nicht installiert: die Version ist nicht von deinem Desk unterschrieben."),
        Why::Manifest(_) => String::from("Nicht installiert: die Versionsdatei ist fehlerhaft (ein Fehler beim Verpacken). Der Desk muss die Version neu machen."),
        Why::Tag { .. } => String::from("Nicht installiert: die Version heißt im Archiv anders als in ihrer Versionsdatei. Der Desk muss sie neu machen."),
        Why::NotNewer { version, .. } => format!("Version {version} ist nicht neuer als deine: {NOTHING}"),
        Why::TooOld { need, .. } => format!("Diese Version braucht zuerst Version {need}. {NOTHING}"),
        Why::File { name, .. } => format!("Nicht installiert: {} kam beschädigt an. Versuch es noch einmal.", if name == "KERNEL" { "der Kern" } else if name == "INITRD" { "das Startpaket" } else { "eine Datei" }),
        Why::PartsWithheld => {
            String::from("Nicht installiert: der Version fehlen der Schreibtisch und die Programme, die dieser Rechner schon hat. Der Desk muss sie neu machen. Nichts wurde heruntergeladen.")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_plain_german() {
        assert_eq!(why(&Why::Signature), "Nicht installiert: die Version ist nicht von deinem Desk unterschrieben.");
        assert!(fail(&Fail::Network("x".into())).starts_with("Kein Netz"));
        assert!(fail(&Fail::TokenWrong).contains("Schlüssel stimmt nicht"));
        assert!(fail(&Fail::NoSpace).contains("nicht genug Platz"));
        assert!(why(&Why::PartsWithheld).starts_with("Nicht installiert: der Version fehlen der Schreibtisch"));
        assert_eq!(german_date("2026-11-16"), "16.11.2026");
        assert_eq!(installed(&Installed { running: 165, date: "2026-11-16".into(), ..Installed::default() }), "Version 165 vom 16.11.2026");
        assert_eq!(installed(&Installed::default()), "Entwicklungsstand (ohne Nummer)");
        // No code words in what Georg reads, whatever the detail behind a failure says.
        let detail = String::from("HTTP status 502 KERNEL: EROFS (the stick is read-only) API null Err(x)");
        let all = [
            fail(&Fail::NoToken), fail(&Fail::Clock), fail(&Fail::Tls(detail.clone())), fail(&Fail::NoRelease), why(&Why::NoKey),
            fail(&Fail::Server(detail.clone())), fail(&Fail::Local(detail.clone())), fail(&Fail::Kernel(detail.clone())), fail(&Fail::Pin(detail.clone())),
            fail(&Fail::Refused(detail.clone())),
            fail(&Fail::Network(detail.clone())), why(&Why::Manifest(detail.clone())), why(&Why::Tag { tag: 166, manifest: 165 }),
            taken(&Taken::Waiting(166)), taken(&Taken::Back { version: 166, running: 165 }), taken(&Taken::Failed(166)),
            taken(&Taken::Occupied { version: 166, newest: 166 }),
            why(&Why::File { name: "KERNEL".into(), what: String::new() }), step(Step::Installing), done(165),
            fail(&Fail::NoRoom { need: 3 << 30, free: 1 << 30, missing: 2 << 30 }), fail(&Fail::NoRam { need: 2 << 30, free: 1 << 29 }),
            step(Step::Writing { what: What::Program("brave".into()), bytes: 500_000_000 }), step(Step::Downloading { what: What::Desktop, bytes: 1 }),
            step(Step::Downloading { what: What::Initrd, bytes: 1 }),
        ];
        // The token's help names GitHub's own (English) buttons on purpose, and nothing else.
        assert!(TOKEN_HELP.contains("Fine-grained tokens") && TOKEN_HELP.contains("bitsos-werkstatt"));
        for t in all {
            for w in ["KERNEL", "INITRD", "sha256", "SHA", "manifest", "token", "API", "null", "Err(", "HTTP", "EROFS", "status", "stick"] {
                assert!(!t.contains(w), "{t:?} has {w}");
            }
        }
        assert_eq!(Fail::Server(detail.clone()).detail(), Some(detail.as_str()));
        assert_eq!(Fail::TokenWrong.detail(), None);
    }

    #[test]
    fn a_broken_manifest_or_name_is_no_signature_problem() {
        let sig = why(&Why::Signature);
        for w in [Why::Manifest(String::from("no version line")), Why::Tag { tag: 166, manifest: 165 }] {
            let t = why(&w);
            assert_ne!(t, sig);
            assert!(!t.contains("unterschrieben"), "{t}");
            assert!(t.contains("Der Desk muss"), "{t}");
        }
        assert!(why(&Why::Manifest(String::new())).contains("Versionsdatei ist fehlerhaft"));
        assert!(why(&Why::Tag { tag: 166, manifest: 165 }).contains("heißt im Archiv anders"));
    }

    #[test]
    fn a_taken_number_is_never_the_newest_version() {
        for t in [Taken::Waiting(166), Taken::Back { version: 166, running: 165 }, Taken::Failed(166), Taken::Occupied { version: 166, newest: 166 }] {
            assert_ne!(taken(&t), CURRENT);
            assert!(taken(&t).contains("Version 166"), "{t:?}");
        }
        assert!(taken(&Taken::Back { version: 166, running: 0 }).contains("Entwicklungsstand"));
        assert!(taken(&Taken::Occupied { version: 165, newest: 166 }).contains("höhere Nummer"));
    }
}
