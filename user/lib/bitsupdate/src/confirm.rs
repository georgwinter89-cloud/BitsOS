//! VB-1 (Georg 17.11.): "Version N läuft. Bestätigen?" after the start of an unconfirmed version, for the desktop's
//! notice (bitscomp through bitspkgd's `update_confirm`), and the console (`bitsupdate frage`). "Ja" hands the
//! kernel Georg's answer (the node's `confirm <n>`, written by the boot confirmation as its third proof);
//! "Zurück zur vorherigen" is the page's "Zurück" (a pin to the newest confirmed version below), then a restart.
//! The kernel decides whether the question stands (`confirm.asks`: a numbered version in its own slot, still
//! waiting, a writable medium); here only what that means for the screens. A wrong or missing answer changes
//! nothing: the version stays unconfirmed and the fallback rules stay.

use crate::flow::{self, Installed};
use std::time::{Duration, Instant};

pub const YES: &str = "Ja";
pub const BACK: &str = "Zurück zur vorherigen";

/// The question, as the screens show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub version: u32,
    pub generation: u32,
    /// Where "Zurück zur vorherigen" goes (None: there is no older confirmed version; only "Ja" is offered).
    pub back: Option<u32>,
}

/// What a screen does with the kernel's status (pure, `decide`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    Now(Question),
    /// Not now, but it may come again in this boot ("Wieder die neueste Version starten" after "Zurück").
    Later(&'static str),
    /// Never in this boot: the screens stop looking.
    Never(&'static str),
}

pub fn decide(i: &Installed) -> Ask {
    if i.running == 0 {
        return Ask::Never("ein Entwicklungsstand: den bestätigt nur der Desk");
    }
    if i.running != i.generation {
        return Ask::Never("die Version läuft nicht aus ihrem eigenen Platz");
    }
    if !i.asks && i.answered && matches!(i.state.as_str(), "pending" | "writing") {
        // The kernel writes the answer; if that fails the question stands again, so the screens keep looking.
        return Ask::Later("die Antwort wird gerade geschrieben");
    }
    if !i.asks {
        return Ask::Never(match i.state.as_str() {
            "confirmed" | "writing" => "die Version ist schon bestätigt",
            "pending" if !i.writable => "der Startdatenträger kann nicht beschrieben werden",
            "pending" => "die Frage ist schon beantwortet",
            _ => "diese Version muss nicht bestätigt werden",
        });
    }
    if i.pin.is_some_and(|p| p != i.generation) {
        return Ask::Later("Zurück zur vorherigen ist gewählt");
    }
    Ask::Now(Question { version: i.running, generation: i.generation, back: i.back.filter(|&b| b < i.generation) })
}

pub fn question_text(q: &Question) -> String {
    format!("Version {} läuft. Bestätigen?", q.version)
}

pub fn no_back() -> &'static str {
    "Eine ältere bestätigte Version gibt es nicht."
}

pub fn confirmed(version: u32) -> String {
    format!("Version {version} ist bestätigt. Sie startet ab jetzt immer.")
}

pub fn still_writing(version: u32) -> String {
    format!("Version {version} wird gerade bestätigt; das dauert noch einen Moment.")
}

/// A refused or failed "Ja" (the kernel's reason goes to the log only).
pub fn not_confirmed() -> &'static str {
    "Die Bestätigung hat nicht geklappt; es bleibt alles, wie es war. Ohne Bestätigung startet beim nächsten Mal wieder die vorherige Version."
}

pub fn going_back(to: &str) -> String {
    format!("Zurück zu Version {to}: BitsOS startet jetzt neu.")
}

pub fn back_failed() -> &'static str {
    "Zurück hat nicht geklappt; es bleibt bei dieser Version."
}

/// How "Ja" ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done {
    /// OK is written (`by`: "georg", or the proof that came first).
    Confirmed { by: String },
    /// The kernel still writes (the medium was busy); it goes on by itself.
    Writing,
}

/// The status' end of a "Ja" (pure): Some when the kernel is done with it.
pub fn outcome(i: &Installed) -> Option<Result<Done, String>> {
    match i.state.as_str() {
        "confirmed" => Some(Ok(Done::Confirmed { by: i.by.clone() })),
        "failed" | "replaced" | "none" => Some(Err(i.confirm.clone())),
        _ => None,
    }
}

/// "Ja" for generation `g`: the kernel's `confirm`, then its status until OK is written or `wait` is over.
/// Err: the kernel's reason (refused, or the write failed).
pub fn yes(g: u32, wait: Duration) -> Result<Done, String> {
    let mut n = flow::node().map_err(|f| f.detail().unwrap_or("").to_string())?;
    let a = n.command(&format!("confirm {g}"))?;
    if a.get("ok").and_then(mcp_wire::json::Json::as_bool) != Some(true) {
        return Err(["refused", "bad_request"].iter().find_map(|k| a.get(k).and_then(mcp_wire::json::Json::as_str)).unwrap_or("no reason given").to_string());
    }
    let end = Instant::now() + wait;
    loop {
        let j = n.command("status")?;
        if let Some(r) = outcome(&flow::parse_status(&j)) {
            return r;
        }
        if Instant::now() >= end {
            return Ok(Done::Writing);
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// The restart after "Zurück": init's orderly restart (SIGTERM, UL-7). Err: why not.
pub fn restart() -> Result<(), String> {
    usys::kill(1, 15).map(|_| ()).map_err(|e| format!("SIGTERM to init: error {e}"))
}
