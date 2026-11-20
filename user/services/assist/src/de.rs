//! The German sentences Georg reads: one per kind of failure, and the help. Plain words, the setting to look at
//! named where there is one. The provider's own words never appear here; they go to the log.

use crate::provider::{ApiError, ErrKind, Kind};

/// What went wrong with provider `id` (model `model`), in one or two sentences.
pub fn sentence(e: &ApiError, id: &str, model: &str) -> String {
    match e.kind {
        ErrKind::Key => format!("Der Anbieter „{id}“ lehnt den Schlüssel ab. Trag ihn neu ein: assist --key {id}"),
        ErrKind::Forbidden => format!("Der Anbieter „{id}“ erlaubt diese Anfrage mit deinem Zugang nicht."),
        ErrKind::Billing => format!("Beim Anbieter „{id}“ ist das Guthaben aufgebraucht oder die Abrechnung gesperrt."),
        ErrKind::RateLimit => match e.retry_after {
            Some(d) => format!("Der Anbieter „{id}“ bittet um eine Pause (zu viele Anfragen). Versuch es in {} Sekunden noch einmal.", d.as_secs().max(1)),
            None => format!("Der Anbieter „{id}“ bittet um eine Pause (zu viele Anfragen). Versuch es gleich noch einmal."),
        },
        ErrKind::Overloaded => format!("Der Anbieter „{id}“ ist gerade überlastet. Versuch es gleich noch einmal."),
        ErrKind::ModelNotFound => format!("Der Anbieter „{id}“ kennt das Modell „{model}“ nicht. Prüfe ai.provider.{id}.model und die Adresse."),
        ErrKind::BadRequest => format!("Der Anbieter „{id}“ hat die Anfrage zurückgewiesen. Passen Art (ai.provider.{id}.kind) und Adresse zusammen?"),
        ErrKind::TooLarge => "Das Gespräch ist für das Modell zu lang geworden. Beginne ein neues Gespräch.".into(),
        ErrKind::Server => format!("Beim Anbieter „{id}“ ist ein Fehler aufgetreten (nicht bei dir). Versuch es gleich noch einmal."),
        ErrKind::Dns => format!("Die Adresse des Anbieters „{id}“ lässt sich nicht finden. Ist das Netz da, stimmt ai.provider.{id}.address?"),
        ErrKind::Network => format!("Der Anbieter „{id}“ ist nicht erreichbar. Ist das Netz da, läuft der Rechner dort?"),
        ErrKind::Tls => format!("Die verschlüsselte Verbindung zu „{id}“ kam nicht zustande."),
        ErrKind::Clock => format!("Die Uhr dieses Rechners geht falsch, deshalb gilt das Zertifikat von „{id}“ als ungültig. Stell die Uhrzeit richtig."),
        ErrKind::Timeout => format!("Der Anbieter „{id}“ hat nicht rechtzeitig geantwortet (die Grenze steht in ai.timeout_s)."),
        ErrKind::Cut => format!("Die Antwort von „{id}“ ist mittendrin abgebrochen."),
        ErrKind::Protocol => format!("Die Antwort von „{id}“ war nicht zu lesen. Passen Art (ai.provider.{id}.kind) und Adresse zusammen?"),
        ErrKind::TooBig => format!("Die Antwort von „{id}“ ist größer, als dieser Rechner gerade im Speicher halten kann."),
        ErrKind::Cancelled => "Abgebrochen.".into(),
    }
}

/// An address as each kind wants it (the adapter appends the rest of the path).
pub fn address_hint(kind: Kind) -> &'static str {
    match kind {
        Kind::Anthropic => "zum Beispiel https://api.anthropic.com",
        Kind::OpenAi => "mit /v1 am Ende, zum Beispiel https://api.openai.com/v1",
        Kind::OpenAiCompatible => "mit /v1 am Ende, zum Beispiel http://192.168.100.20:8080/v1",
        Kind::Gemini => "mit /v1beta am Ende, zum Beispiel https://generativelanguage.googleapis.com/v1beta",
        Kind::Program => "der volle Pfad des Hilfsprogramms, zum Beispiel /data/programme/helfer",
    }
}

/// The next step after an address that is no network address: one that looks like a path or a bare word is
/// most likely a helper program on this machine, set up with the wrong kind (seen on the Mini-PC with generation 140,
/// 12.11.).
pub fn not_an_address(address: &str) -> &'static str {
    let a = address.trim();
    if a.starts_with(['/', '~', '.']) || !a.contains("://") {
        " Läuft der Anbieter als Programm auf diesem Rechner (etwa für ein Abo), wähle auf der Einstellungsseite „KI“ die Art „Hilfsprogramm“ und dort das Programm."
    } else {
        ""
    }
}

/// Shown when no provider is set up: BitsOS brings none (DECISIONS 21.10.), so this is the first thing Georg
/// reads. Lines of at most 76 columns: it must read well on an 80-column console too.
pub fn howto() -> String {
    "Es ist noch kein KI-Anbieter eingerichtet. BitsOS bringt keinen mit:\n\
du bestimmst, wer antwortet.\n\
\n\
So richtest du einen ein. Der Name ist frei, zum Beispiel claude,\n\
chatgpt, gemini oder strata:\n\
\n\
  assist --add NAME\n\
  settings set ai.provider.NAME.kind ART\n\
  settings set ai.provider.NAME.address ADRESSE\n\
  settings set ai.provider.NAME.model MODELL\n\
  settings set ai.provider.NAME.place ORT\n\
  assist --key NAME\n\
  settings set ai.role.chat NAME\n\
\n\
ART ist anthropic, openai, gemini oder openai_compatible.\n\
Art program: ein Hilfsprogramm antwortet, ADRESSE ist sein voller Pfad.\n\
ORT ist cloud oder local; das legst du fest, BitsOS rät es nicht.\n\
assist --key fragt nach dem Schlüssel, ohne ihn anzuzeigen. Er gilt nur\n\
für Art, Adresse und Ort, wie sie dann eingetragen sind: wer die später\n\
ändern will, löscht den Schlüssel vorher und trägt ihn danach neu ein:\n\
in den Einstellungen auf der Seite „KI“, oder als root an der Konsole mit\n\
settings reset --confirm ai.provider.NAME.key (als anderer Benutzer lehnt\n\
BitsOS das ab; ebenso das Entfernen eines Anbieters aus ai.providers).\n\
ai.role.chat bestimmt, wer im Gespräch antwortet.\n\
\n\
Die ADRESSE je Art:\n\
  anthropic          https://api.anthropic.com\n\
  openai             https://api.openai.com/v1\n\
  gemini             https://generativelanguage.googleapis.com/v1beta\n\
  openai_compatible  http://192.168.100.20:8080/v1 (Beispiel im Heimnetz)\n\
\n\
Lokale Programme (llama-server, Ollama, Strata) sind openai_compatible,\n\
brauchen meist keinen Schlüssel und dürfen im Heimnetz http:// benutzen.\n\
„settings set ai.local_only true“: kein Anbieter in der Cloud wird mehr\n\
angesprochen. „assist --providers“ zeigt, was eingerichtet ist.\n"
        .to_string()
}

pub const USAGE: &str = "assist                      Gespräch im ganzen Bildschirm (Esc oder Strg-D beendet)\n\
assist --once \"TEXT\"        eine Frage, die Antwort auf die Standardausgabe\n\
assist --providers          zeigt die eingerichteten Anbieter (ohne Schlüssel)\n\
assist --add NAME           legt die Einstellungen für einen neuen Anbieter an\n\
assist --key NAME           trägt den Schlüssel eines Anbieters ein (wird nicht angezeigt)\n\
assist --test NAME          prüft einen Anbieter mit einer einzigen kleinen Anfrage (eine Zeile Antwort)\n\
assist --mcp                BitsOS-Werkzeuge für ein anderes Programm (MCP über Ein- und Ausgabe)\n\
assist --ask --bell PFAD    Fragen eines Helfers an den Assistenten (eine Zeile je Frage, HELPER.md)\n\
assist --call --tool-socket PFAD   Aufrufe eines Helfers für die Werkzeuge des Assistenten (HELPER.md)\n\
assist serve                der Assistent als Dienst für Seitenleiste und Durchblick (startet die Oberfläche selbst)\n\
assist --help               diese Hilfe\n\
\n\
Anbieter gibt es in fünf Arten; bei der Art „program“ antwortet ein Hilfsprogramm auf diesem Rechner.\n\
\n\
Zusätze: --socket PFAD (settingsd), --log DATEI (technische Meldungen im Gespräch),\n\
--read-only (mit --mcp: nur Werkzeuge, die lesen), --listen PFAD (mit serve: ein anderer Socket),\n\
--bell PFAD (mit --mcp unter einem Helfer: der Assistent darüber fragt dich, bevor ein Werkzeug etwas ändert)\n";

#[cfg(test)]
mod tests {
    #[test]
    fn a_path_or_a_word_as_an_address_points_to_the_helper_kind() {
        for a in ["/data/home/1000/helfer", "abo", "~/helfer"] {
            assert!(super::not_an_address(a).contains("„Hilfsprogramm“"), "{a}");
        }
        for a in ["https://api.anthropic.com", "http://[::1:30500/v1"] {
            assert_eq!(super::not_an_address(a), "", "{a}");
        }
    }
}
