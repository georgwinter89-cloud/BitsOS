//! What each kind of provider is for (one line beside its choice on the page "KI"), and what to do when an
//! address does not fit its kind. Georg once picked "Anthropic" for a subscription that runs as a helper program
//! on this machine: the page must say which kind is which, and a refusal
//! must name the next step instead of only "the address is wrong".

use crate::ki::{PROGRAM, UNSET, kind_name};
use crate::ki_key::host_of;

/// The line beside each kind's choice.
pub fn note(kind: &str) -> &'static str {
    match kind {
        UNSET => "Noch keine Art: dieser Anbieter wird nicht gefragt.",
        "anthropic" => "Claude mit Schlüssel aus der Anthropic-Konsole. Für ein Abo: Hilfsprogramm.",
        "openai" => "ChatGPT-Modelle mit einem Schlüssel von OpenAI.",
        "gemini" => "Googles Gemini mit einem Schlüssel aus Google AI Studio.",
        "openai_compatible" => "Ein KI-Dienst im Heimnetz, zum Beispiel ein eigener KI-Rechner.",
        PROGRAM => "Ein Programm auf diesem Rechner, etwa für ein Abo ohne Schlüssel.",
        _ => "Eine Art, die diese Seite noch nicht erklärt.",
    }
}

/// The models offered for a provider of kind `anthropic` (the sidebar's model list): the API's model names and
/// their labels. The one place they are written down; a name not here is still kept and shown as it stands.
pub const ANTHROPIC_MODELS: [(&str, &str); 4] =
    [("claude-fable-5-2", "Fable 5.2"), ("claude-opus-5-5", "Opus 5.5"), ("claude-sonnet-5-5", "Sonnet 5.5"), ("claude-haiku-5-5", "Haiku 5.5")];

/// An address that kind `kind` would take, as an example.
pub fn example(kind: &str) -> &'static str {
    match kind {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "https://api.openai.com/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta",
        _ => "http://192.168.100.20:8080/v1",
    }
}

/// Looks like a place on this machine rather than on the network.
fn path_like(a: &str) -> bool {
    a.starts_with(['/', '~', '.'])
}

/// What to do when `address` does not fit `kind` (German, one line); None: it fits, or nothing is typed yet.
/// `helpers`: helpers were found on this machine (the page lists them).
pub fn address_advice(kind: &str, address: &str, helpers: bool) -> Option<String> {
    let a = address.trim();
    if a.is_empty() || kind == UNSET {
        return None;
    }
    let pick = if helpers { " oder ein gefundenes übernehmen" } else { "" };
    if kind == PROGRAM {
        if a.starts_with("http://") || a.starts_with("https://") {
            return Some(format!("Das ist eine Adresse im Netz: dafür eine andere Art wählen, etwa „{}“.", kind_name("openai_compatible")));
        }
        return (!a.starts_with('/')).then(|| format!("Hier gehört der ganze Pfad des Programms hin, der mit / beginnt{pick}."));
    }
    let why = host_of(a).err()?;
    let program = kind_name(PROGRAM);
    Some(if path_like(a) {
        format!("Das ist ein Pfad, keine Adresse. Für ein Programm auf diesem Rechner: Art „{program}“ wählen.")
    } else if !a.contains("://") {
        format!("Keine Adresse (etwa {}). Für ein Programm auf diesem Rechner: Art „{program}“ wählen.", example(kind))
    } else {
        format!("Die Adresse passt nicht: {why}. Etwa: {}", example(kind))
    })
}

/// A change settingsd refused because a key is stored for provider `id` (its `key_bound`).
pub fn key_bound(id: &str) -> String {
    format!(
        "Für „{id}“ ist ein Schlüssel gespeichert, der nur zu Adresse, Art und Ort passt, wie sie jetzt sind. Erst bei „Schlüssel“ auf „Löschen“, dann das hier ändern, dann den Schlüssel neu eintippen."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_explained_and_a_wrong_address_names_the_next_step() {
        for k in [UNSET, "anthropic", "openai", "gemini", "openai_compatible", PROGRAM] {
            assert!(!note(k).is_empty() && note(k).chars().count() <= 80, "{k}: {}", note(k));
        }
        assert!(note("anthropic").contains("Abo"), "a subscription is findable from the kind Georg picks for Claude");
        assert_eq!(address_advice("anthropic", "", true), None);
        assert_eq!(address_advice("anthropic", "https://api.anthropic.com", true), None);
        assert_eq!(address_advice(PROGRAM, "/data/programme/helfer", true), None);
        let path = address_advice("anthropic", "/data/home/1000/helfer", true).expect("a path for an http kind");
        assert!(path.contains("„Hilfsprogramm“ wählen"), "{path}");
        let word = address_advice("anthropic", "abo", true).expect("a bare word");
        assert!(word.contains("https://api.anthropic.com") && word.contains("„Hilfsprogramm“ wählen"), "{word}");
        let bad = address_advice("openai_compatible", "http://", false).expect("no host");
        assert!(bad.starts_with("Die Adresse passt nicht: in ihr fehlt der Name des Rechners. Etwa: http"), "{bad}");
        assert!(address_advice(PROGRAM, "https://x.example", true).expect("an address for a helper").contains("„OpenAI-kompatibel“"));
        assert!(address_advice(PROGRAM, "abo", true).expect("no path").contains("beginnt oder ein gefundenes übernehmen"));
        for s in [path, word, bad] {
            assert!(!s.contains("ai.provider") && !s.contains("settings set"), "no code words: {s}");
        }
    }
}
