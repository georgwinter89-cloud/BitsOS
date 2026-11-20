//! The fixed nightly set (plan/spec/self-optimise.md, level 1): what each benchmark is called on the page, its
//! unit, and which way is better. The ids are the JSON's and the page's (bitscomp leistung.rs keeps the same list).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Better {
    Lower,
    Higher,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bench {
    pub id: &'static str,
    /// The German name on the page and in the report.
    pub name: &'static str,
    pub unit: &'static str,
    pub better: Better,
    /// Entered by hand (`bitsbench enter`), never run by the set.
    pub by_hand: bool,
}

const fn b(id: &'static str, name: &'static str, unit: &'static str, better: Better) -> Bench {
    Bench { id, name, unit, better, by_hand: false }
}

pub const SET: [Bench; 11] = [
    b("boot", "Hochfahren", "ms", Better::Lower),
    b("brave_start", "Brave starten", "ms", Better::Lower),
    b("brave_load", "Seite laden", "ms", Better::Lower),
    b("brave_scroll", "Scrollen in Brave", "ms je Bild", Better::Lower),
    b("desktop_frame", "Bild der Arbeitsfläche", "ms je Bild", Better::Lower),
    b("game_frame", "Spiel (vkQuake)", "ms je Bild", Better::Lower),
    Bench { id: "farthest_frontier", name: "Farthest Frontier", unit: "ms je Bild", better: Better::Lower, by_hand: true },
    b("steam_start", "Steam starten", "ms", Better::Lower),
    b("file_read", "Dateien lesen", "MB/s", Better::Higher),
    b("file_mix", "Dateien schreiben und lesen", "ms", Better::Lower),
    b("memory_load", "Speicher unter Last", "ms", Better::Lower),
];

pub fn get(id: &str) -> Option<&'static Bench> {
    SET.iter().find(|b| b.id == id)
}

/// Runs per benchmark and night (Georg 13.11.: every benchmark 5 runs).
pub const RUNS: usize = 5;
