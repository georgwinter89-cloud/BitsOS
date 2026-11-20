//! `bits papierkorb liste|zurueck|leeren` over PLAN K6's trash (the kernel's entries, read in `trash.rs`).
//! Emptying is asked on the desktop (only Georg's hand says yes) and never possible from the assistant's tree; the
//! kernel refuses the tree's `empty` too.

use serde_json::{Value, json};

use crate::out::{Done, Fail, R};
use crate::trash::{self, Entry};
use crate::{einst, frage};

pub fn run(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("liste") | None => liste(),
        Some("zurueck") => zurueck(&args[1..]),
        Some("leeren") => leeren(args.get(1).map(String::as_str)),
        Some(v) => Err(Fail::usage(format!("„papierkorb {v}“ gibt es nicht; möglich: liste, zurueck, leeren."))),
    }
}

fn json_of(e: &Entry) -> Value {
    json!({"id": e.id, "mount": e.mount, "original": e.original, "time": e.time, "command": e.command, "chat": e.chat, "pid": e.pid, "kind": e.kind, "bytes": e.bytes})
}

fn liste() -> R {
    let es = trash::entries()?;
    let total: u64 = es.iter().map(|e| e.bytes).sum();
    let mut text = es.iter().map(|e| format!("{}  {}  {}", e.id, e.original, trash::size_text(e.bytes))).collect::<Vec<_>>().join("\n");
    text.push_str(&format!("{}{} Einträge, {}", if es.is_empty() { "" } else { "\n" }, es.len(), trash::size_text(total)));
    Ok(Done::new(text).with("entries", es.iter().map(json_of).collect::<Vec<_>>()).with("bytes", total))
}

fn zurueck(a: &[String]) -> R {
    let id = a.first().ok_or_else(|| Fail::usage("papierkorb zurueck <id> [--nach <pfad>]"))?;
    let to = match a.iter().position(|x| x == "--nach") {
        Some(i) => Some(a.get(i + 1).ok_or_else(|| Fail::usage("--nach braucht einen Pfad"))?.clone()),
        None => None,
    };
    let e = trash::find(id)?;
    let target = to.unwrap_or_else(|| e.original.clone());
    if bitstree::of_self().inside() {
        match einst::level()?.as_str() {
            "auto" => {}
            "answer" => return Err(Fail::refused(einst::ANSWER_ONLY)),
            _ => frage::ask(&format!("bits-frage zurueck {id} {target}"))?,
        }
    }
    trash::restore(&e, &target)?;
    Ok(Done::new(format!("Zurück: {target}")).with("id", id.as_str()).with("restored", target))
}

pub const NOT_FROM_TREE: &str = "Den Papierkorb leert nur Georg selbst, nie der Assistent. Es wurde nichts gelöscht.";

fn leeren(id: Option<&str>) -> R {
    if bitstree::of_self().inside() {
        return Err(Fail::refused(NOT_FROM_TREE));
    }
    let all = trash::entries()?;
    let chosen: Vec<&Entry> = match id {
        Some(i) => vec![all.iter().find(|e| e.id == i).ok_or_else(|| Fail::new("not_found", format!("Im Papierkorb gibt es „{i}“ nicht.")))?],
        None => all.iter().collect(),
    };
    if chosen.is_empty() {
        return Ok(Done::new("Der Papierkorb ist leer.").with("removed", 0).with("bytes", 0));
    }
    let bytes: u64 = chosen.iter().map(|e| e.bytes).sum();
    frage::ask(&format!("bits-frage leeren {} {bytes}", chosen.len()))?;
    for e in &chosen {
        trash::remove(e)?;
    }
    Ok(Done::new(format!("Endgültig gelöscht: {} Einträge, {}.", chosen.len(), trash::size_text(bytes))).with("removed", chosen.len()).with("bytes", bytes))
}
