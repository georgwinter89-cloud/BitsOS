//! The change journal (B1: every change can be undone). One JSON object per line in `journal.log` beside the
//! group files: sequence number, time, key, old and new value (`null` = the default), who asked, and for an
//! undo the entry it reverts. Secret keys are journaled without values (and cannot be undone).

use crate::json::{self, Json};
use crate::value::Value;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub t_ms: u64,
    pub key: String,
    pub old: Option<Value>,
    pub new: Option<Value>,
    pub source: String,
    /// `set`, `reset`, `undo`, `edit` (found changed in a file at start) or `resync`.
    pub op: String,
    pub undoes: Option<u64>,
    pub secret: bool,
}

impl Entry {
    pub fn to_json(&self) -> Json {
        let v = |x: &Option<Value>| {
            if self.secret { Json::from("***") } else { x.as_ref().map_or(Json::Null, Value::to_json) }
        };
        let mut j = Json::obj()
            .with("seq", self.seq)
            .with("t_ms", self.t_ms)
            .with("key", self.key.as_str())
            .with("old", v(&self.old))
            .with("new", v(&self.new))
            .with("source", self.source.as_str())
            .with("op", self.op.as_str());
        if let Some(u) = self.undoes {
            j = j.with("undoes", u);
        }
        if self.secret {
            j = j.with("secret", true);
        }
        j
    }

    pub fn from_json(j: &Json) -> Option<Entry> {
        let secret = j.get("secret").and_then(Json::as_bool).unwrap_or(false);
        let val = |k: &str| if secret { None } else { j.get(k).and_then(Value::from_json) };
        Some(Entry {
            seq: u64::try_from(j.get("seq")?.as_i64()?).ok()?,
            t_ms: j.get("t_ms").and_then(Json::as_i64).and_then(|t| u64::try_from(t).ok()).unwrap_or(0),
            key: j.str("key")?.to_string(),
            old: val("old"),
            new: val("new"),
            source: j.str("source").unwrap_or("?").to_string(),
            op: j.str("op").unwrap_or("set").to_string(),
            undoes: j.get("undoes").and_then(Json::as_i64).and_then(|u| u64::try_from(u).ok()),
            secret,
        })
    }
}

#[derive(Default, Debug, Clone)]
pub struct Journal {
    pub entries: Vec<Entry>,
    next: u64,
}

impl Journal {
    /// Reads a journal file; lines that do not parse are counted, not fatal (a cut write leaves half a line).
    pub fn load(text: &str) -> (Journal, usize) {
        let mut j = Journal::default();
        let mut bad = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match json::parse(line).ok().as_ref().and_then(Entry::from_json) {
                Some(e) => j.push_loaded(e),
                None => bad += 1,
            }
        }
        (j, bad)
    }

    pub fn push_loaded(&mut self, mut e: Entry) {
        // Sequence numbers only grow, also across a journal merged from the fallback.
        if e.seq < self.next {
            e.seq = self.next;
        }
        self.next = e.seq + 1;
        self.entries.push(e);
    }

    pub fn next_seq(&self) -> u64 {
        self.next.max(1)
    }

    pub fn add(&mut self, mut e: Entry) -> Entry {
        e.seq = self.next_seq();
        self.next = e.seq + 1;
        self.entries.push(e.clone());
        e
    }

    fn undone(&self) -> HashSet<u64> {
        self.entries.iter().filter_map(|e| e.undoes).collect()
    }

    /// The newest `n` entries an undo reverts, newest first: not an undo itself and not undone yet.
    pub fn undo_candidates(&self, n: usize) -> Vec<Entry> {
        let undone = self.undone();
        self.entries.iter().rev().filter(|e| e.op != "undo" && !undone.contains(&e.seq)).take(n).cloned().collect()
    }

    /// Newest first, optionally for one key or a prefix; each with whether it was undone.
    pub fn history(&self, n: usize, prefix: Option<&str>) -> Vec<Json> {
        let undone = self.undone();
        self.entries
            .iter()
            .rev()
            .filter(|e| {
                prefix.is_none_or(|p| e.key == p || e.key.starts_with(&format!("{}.", p.trim_end_matches('.'))))
            })
            .take(n)
            .map(|e| e.to_json().with("undone", undone.contains(&e.seq)))
            .collect()
    }

    pub fn line(e: &Entry) -> String {
        let mut s = e.to_json().to_text();
        s.push('\n');
        s
    }

    pub fn text(&self) -> String {
        self.entries.iter().map(Journal::line).collect()
    }

    /// Keeps the newest `keep` entries. Returns whether anything was dropped (then the file is rewritten).
    pub fn trim(&mut self, keep: usize) -> bool {
        if self.entries.len() <= keep {
            return false;
        }
        let cut = self.entries.len() - keep;
        self.entries.drain(..cut);
        true
    }

    /// Drops every entry of keys under `prefix` (a package removed without leftovers, PK-2).
    pub fn forget(&mut self, keys: &HashSet<String>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !keys.contains(&e.key));
        before != self.entries.len()
    }
}
