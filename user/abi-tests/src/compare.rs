//! Comparing a transcript with Linux's (`expected/wave-*.txt`) and the known-difference list
//! (`expected/wave-*.known`). Uses only `alloc`, so the kernel's `ul.abi.*` ktests include this same file
//! (`crates/userland`, `#[path]`) and judge exactly as the host test does.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The lines of a transcript as (id, result) pairs; `wave X end (n checks)` becomes ("wave X end", "n checks"),
/// other lines without ": " keep the whole line as id.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            if l.starts_with("wave ")
                && let Some((a, b)) = l.split_once(" (")
            {
                return (a.to_string(), b.trim_end_matches(')').to_string());
            }
            match l.split_once(": ") {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (l.to_string(), String::new()),
            }
        })
        .collect()
}

/// A known difference: `<id>: <owner and reason>` lines (`#` comments).
pub fn parse_known(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once(": ") {
            Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
            None => (l.trim_end_matches(':').to_string(), String::new()),
        })
        .collect()
}

/// The result of comparing a transcript with Linux's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Lines equal to Linux's.
    pub same: usize,
    /// Differences listed as known: (id, linux, got).
    pub known: Vec<(String, String, String)>,
    /// Known differences that now match Linux (remove them from the list).
    pub fixed: Vec<String>,
    /// Differences not listed: (id, linux, got). A missing line has got = "<missing>".
    pub new: Vec<(String, String, String)>,
    /// Lines Linux does not have.
    pub extra: Vec<String>,
}

impl Diff {
    pub fn ok(&self) -> bool {
        self.new.is_empty() && self.extra.is_empty()
    }
    pub fn summary(&self) -> String {
        let mut s = format!(
            "same={} known={} fixed={} new={} extra={}",
            self.same,
            self.known.len(),
            self.fixed.len(),
            self.new.len(),
            self.extra.len()
        );
        for (id, e, g) in &self.new {
            s.push_str(&format!("\n  NEW {id}: linux '{e}' got '{g}'"));
        }
        for id in &self.extra {
            s.push_str(&format!("\n  EXTRA {id}"));
        }
        for id in &self.fixed {
            s.push_str(&format!("\n  FIXED {id} (now equal to Linux: remove it from the known list)"));
        }
        s
    }
    /// The known differences as text (for the notes and the ktest log).
    pub fn known_text(&self) -> String {
        let mut s = String::new();
        for (id, e, g) in &self.known {
            s.push_str(&format!("  known {id}: linux '{e}' got '{g}'\n"));
        }
        s
    }
}

/// Compares `got` with Linux's `expected`, line by id. The `wave X end (n checks)` line is compared like any
/// other, so a transcript cut short by a crash shows as missing lines.
pub fn compare(expected: &str, got: &str, known: &str) -> Diff {
    let exp = parse(expected);
    let got = parse(got);
    let known = parse_known(known);
    let mut d = Diff::default();
    for (id, e) in &exp {
        let g = got.iter().find(|(gi, _)| gi == id).map(|(_, g)| g.clone());
        let is_known = known.iter().any(|(k, _)| k == id);
        match g {
            Some(g) if &g == e => {
                d.same += 1;
                if is_known {
                    d.fixed.push(id.clone());
                }
            }
            other => {
                let g = other.unwrap_or_else(|| "<missing>".to_string());
                if is_known {
                    d.known.push((id.clone(), e.clone(), g));
                } else {
                    d.new.push((id.clone(), e.clone(), g));
                }
            }
        }
    }
    for (id, _) in &got {
        if !exp.iter().any(|(e, _)| e == id) && !known.iter().any(|(k, _)| k == id) {
            d.extra.push(id.clone());
        }
    }
    d
}
