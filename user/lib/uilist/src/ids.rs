//! Stable element ids `<window>/<n>`: n is a short hash of the element's key (what it is, not where it is drawn),
//! so an id stays the same while the element exists, across lists and across restarts; a second element with the
//! same key in one list gets `-2`, `-3`.

use std::collections::HashMap;

pub struct Ids {
    window: String,
    seen: HashMap<String, u32>,
}

/// FNV-1a, 32 bits: small, fixed across builds and platforms (std's hasher is seeded per process).
fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

fn base36(mut v: u32) -> String {
    const D: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(D[(v % 36) as usize]);
        v /= 36;
        if v == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

impl Ids {
    pub fn new(window: &str) -> Ids {
        Ids { window: window.to_string(), seen: HashMap::new() }
    }

    pub fn id(&mut self, key: &str) -> String {
        let n = base36(fnv(key));
        let count = self.seen.entry(n.clone()).or_insert(0);
        *count += 1;
        if *count == 1 { format!("{}/{n}", self.window) } else { format!("{}/{n}-{count}", self.window) }
    }
}

/// An element id as a model writes it, back to the id: the prompts show ids as `[#w3/1a2b]` (BitsOS frames those),
/// and a model may pass that form, or `#w3/1a2b`, to a tool (Claude over the abo did, 14.11.).
pub fn clean_id(id: &str) -> &str {
    let t = id.trim();
    let t = t.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(t).trim();
    t.strip_prefix('#').unwrap_or(t).trim()
}

/// The window part of an element id (`w12/3f9k` -> `w12`).
pub fn window_of(id: &str) -> Option<&str> {
    id.rsplit_once('/').map(|(w, _)| w).filter(|w| !w.is_empty())
}
