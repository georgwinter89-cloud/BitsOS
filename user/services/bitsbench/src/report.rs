//! The morning's one line (plain German, no code words): whether the night's medians stayed within their usual
//! range, or which benchmarks moved and by how much. The usual range is the median of up to the last 7 nights
//! before of the same generation (the first night of a new one is held against the one before and says so); a
//! benchmark with fewer than 3 runs tonight is not compared; a benchmark moved when it lies 5 % or more away from
//! it (five runs on real hardware scatter by a few per cent, the night's own spread is in the JSON).

use crate::catalog::{self, Better};
use crate::store::Entry;
use std::collections::{BTreeMap, BTreeSet};

pub const BASE_NIGHTS: usize = 7;
pub const MOVED: f64 = 0.05;
/// A benchmark with fewer runs tonight than this (or than the set asked for, if that is less) is kept but not
/// compared: one boot moving by 5 % says nothing about the generation.
pub const MIN_RUNS: usize = 3;
/// Named in the line at most; the rest is counted.
const NAMED: usize = 3;

/// Tonight as the line sees it.
pub struct Tonight<'a> {
    pub medians: &'a BTreeMap<String, f64>,
    /// Benchmarks with too few runs tonight to be compared.
    pub few: &'a BTreeSet<String>,
    pub generation: Option<u64>,
    pub origin: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Move {
    pub id: String,
    /// Relative change against the usual range (+0.12 = 12 % more).
    pub change: f64,
    pub worse: bool,
}

/// The earlier nights tonight is held against (oldest first): those of the same generation and origin; for the
/// first night of a new generation (or origin), those of the one measured last before it, and the line names the
/// change. Mixing two generations in one usual range would hide a real change or report it as a wobble.
pub fn baseline<'a>(t: &Tonight, before: &'a [Entry]) -> (Vec<&'a Entry>, bool) {
    let same = |g: Option<u64>, o: &str| -> Vec<&'a Entry> { before.iter().filter(|e| e.generation == g && e.origin == o).collect() };
    let mine = same(t.generation, t.origin);
    if !mine.is_empty() {
        return (mine, false);
    }
    match before.iter().max_by(|a, b| a.night.cmp(&b.night)) {
        Some(last) => (same(last.generation, &last.origin), true),
        None => (Vec::new(), false),
    }
}

/// The benchmarks that moved tonight, largest change first. `base`: earlier nights, oldest first.
pub fn moves(t: &Tonight, base: &[&Entry]) -> Vec<Move> {
    let mut out = Vec::new();
    for (id, now) in t.medians.iter().filter(|(id, _)| !t.few.contains(*id)) {
        let past: Vec<f64> = base.iter().rev().filter_map(|e| e.medians.get(id).copied()).take(BASE_NIGHTS).collect();
        let Some(base) = crate::stats::median(&past) else { continue };
        if base.abs() < f64::EPSILON {
            continue;
        }
        let change = (now - base) / base;
        if change.abs() < MOVED {
            continue;
        }
        let better = catalog::get(id).map_or(Better::Lower, |b| b.better);
        out.push(Move { id: id.clone(), change, worse: (change > 0.0) == (better == Better::Lower) });
    }
    out.sort_by(|a, b| b.change.abs().total_cmp(&a.change.abs()));
    out
}

pub fn line(t: &Tonight, before: &[Entry]) -> String {
    if t.medians.is_empty() {
        return "Nacht-Messung: nichts gemessen".into();
    }
    let (base, changed) = baseline(t, before);
    let compared = t.medians.keys().any(|id| !t.few.contains(id) && base.iter().any(|e| e.medians.contains_key(id)));
    if !compared {
        return "Nacht-Messung: erste Messung, noch nichts zum Vergleichen".into();
    }
    let head = if !changed {
        "Nacht-Messung".to_string()
    } else if base.first().is_some_and(|e| e.generation != t.generation) {
        match t.generation {
            Some(g) => format!("Nacht-Messung nach dem Wechsel auf Generation {g}"),
            None => "Nacht-Messung nach dem Wechsel der Generation".into(),
        }
    } else {
        format!("Nacht-Messung nach dem Wechsel auf „{}“", t.origin)
    };
    let m = moves(t, &base);
    if m.is_empty() {
        return format!("{head}: alles im Rahmen");
    }
    let named: Vec<String> = m
        .iter()
        .take(NAMED)
        .map(|x| {
            let name = catalog::get(&x.id).map_or(x.id.as_str(), |b| b.name);
            format!("{name} {} % {}", (x.change.abs() * 100.0).round() as i64, if x.worse { "langsamer" } else { "schneller" })
        })
        .collect();
    let more = m.len().saturating_sub(NAMED);
    let tail = match more {
        0 => String::new(),
        1 => " und 1 weitere Messung".into(),
        n => format!(" und {n} weitere Messungen"),
    };
    format!("{head}: {}{tail}", named.join(", "))
}
