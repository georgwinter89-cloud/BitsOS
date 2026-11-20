//! CX-3: learning (DESIGN §7.2-7.4). What Georg did is kept in `<data>/usage/events-NNNNNN.jsonl` (the part
//! that is not derived and must never be lost: append-only, one event per line, ids and numbers only; a
//! query is kept as a salted hash and, when the model answered, its vector). From it, in memory:
//! - co-open edges: two documents opened in one session get `exp(-dt / 300 s)`; a manual add counts 3 with
//!   each item already in the workspace, an accepted suggestion 1;
//! - query memory: an open within 10 minutes after a search ties the document to that query;
//! - dismissals: 0.5 for similar queries (scope workspace or query), 2 everywhere;
//! - the last open of each document (recency uses the later of it and the file's time).
//!
//! Everything decays with a half-life of 90 days. A sum of `w * 2^(t/H)` per edge gives its value at any time
//! as `sum * 2^(-now/H)` without rewriting anything. The clock can be moved forward (`advance`) for the
//! replay of recorded sessions (`ctxd replay`, §7.3), never backward.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, RwLock};

pub const HALF_LIFE_DAYS: f64 = 90.0;
const SESSION_GAP_S: i64 = 20 * 60;
const CO_OPEN_S: f64 = 300.0;
const TIE_WINDOW_S: i64 = 600;
const SIMILAR: f32 = 0.85;
/// A program that opens more files than this within `SCAN_WINDOW_S` is a scanner, not Georg.
const SCAN_OPENS: usize = 20;
const SCAN_WINDOW_S: i64 = 10;
const ROTATE: u64 = 8 << 20;
/// Days are counted from 2020 so the growth factors stay small numbers.
const EPOCH: i64 = 1_577_836_800;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub v: u32,
    pub ts: i64,
    pub ev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// Salted hash of the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qh: Option<String>,
    /// The query's vector, base64 of little-endian f32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qv: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// A query as learning knows it: never its text.
#[derive(Debug, Clone, PartialEq)]
pub struct QKey {
    pub hash: String,
    pub vec: Option<Vec<f32>>,
}

impl QKey {
    pub fn similar(&self, o: &QKey) -> bool {
        match (&self.vec, &o.vec) {
            (Some(a), Some(b)) if a.len() == b.len() => crate::embed::dot(a, b) >= SIMILAR,
            _ => self.hash == o.hash,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Edge {
    grown: f64,
    count: u32,
}

#[derive(Debug, Default)]
struct State {
    edges: HashMap<String, HashMap<String, Edge>>,
    ties: Vec<(String, QKey, i64)>,
    /// (doc, query, weight, ts, workspace, everywhere)
    dismissals: Vec<(String, Option<QKey>, f64, i64, Option<String>, bool)>,
    last_open: HashMap<String, i64>,
    session: Vec<(String, i64)>,
    session_ws: Option<String>,
    searches: Vec<(QKey, i64)>,
    /// Recent opens per program (scanner detection).
    by_program: HashMap<String, Vec<i64>>,
    events: u64,
}

fn grow(ts: i64) -> f64 {
    2f64.powf((ts - EPOCH) as f64 / 86_400.0 / HALF_LIFE_DAYS)
}

fn decay_at(ts: i64, now: i64) -> f64 {
    2f64.powf(-((now - ts).max(0) as f64) / 86_400.0 / HALF_LIFE_DAYS)
}

pub fn encode_vec(v: &[f32]) -> String {
    let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
    b64::encode(&bytes)
}

pub fn decode_vec(s: &str) -> Option<Vec<f32>> {
    let b = b64::decode(s)?;
    (b.len() % 4 == 0).then(|| b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

fn qkey_of(e: &Event) -> Option<QKey> {
    Some(QKey { hash: e.qh.clone()?, vec: e.qv.as_deref().and_then(decode_vec) })
}

impl State {
    fn add_edge(&mut self, a: &str, b: &str, w: f64, ts: i64) {
        if a == b {
            return;
        }
        for (x, y) in [(a, b), (b, a)] {
            let e = self.edges.entry(x.to_string()).or_default().entry(y.to_string()).or_default();
            e.grown += w * grow(ts);
            e.count += 1;
        }
    }

    fn apply(&mut self, e: &Event) {
        self.events += 1;
        match e.ev.as_str() {
            "search" => {
                if let Some(q) = qkey_of(e) {
                    self.searches.retain(|(_, t)| e.ts - t <= TIE_WINDOW_S);
                    self.searches.push((q, e.ts));
                }
            }
            "open" => {
                let Some(doc) = e.doc.clone() else { return };
                if let Some(p) = &e.program {
                    let opens = self.by_program.entry(p.clone()).or_default();
                    opens.retain(|t| e.ts - t <= SCAN_WINDOW_S);
                    opens.push(e.ts);
                    if opens.len() > SCAN_OPENS {
                        return;
                    }
                }
                self.last_open.insert(doc.clone(), e.ts);
                let gap = self.session.last().is_some_and(|(_, t)| e.ts - t > SESSION_GAP_S);
                if gap || self.session_ws != e.workspace {
                    self.session.clear();
                    self.session_ws = e.workspace.clone();
                }
                let before: Vec<(String, i64)> = self.session.clone();
                for (other, t) in &before {
                    self.add_edge(&doc, other, (-((e.ts - t).abs() as f64) / CO_OPEN_S).exp(), e.ts);
                }
                self.session.push((doc.clone(), e.ts));
                for (q, t) in &self.searches {
                    if e.ts - t <= TIE_WINDOW_S && e.ts >= *t {
                        self.ties.push((doc.clone(), q.clone(), e.ts));
                    }
                }
            }
            "add" => {
                let (Some(doc), Some(with)) = (e.doc.clone(), e.with.clone()) else { return };
                let w = if e.how.as_deref() == Some("manual") { 3.0 } else { 1.0 };
                for o in &with {
                    self.add_edge(&doc, o, w, e.ts);
                }
            }
            "dismiss" => {
                let Some(doc) = e.doc.clone() else { return };
                let everywhere = e.scope.as_deref() == Some("everywhere");
                let w = if everywhere { 2.0 } else { 0.5 };
                self.dismissals.push((doc, qkey_of(e), w, e.ts, e.workspace.clone(), everywhere));
            }
            "undo" => {
                let Some(doc) = &e.doc else { return };
                if let Some(i) = self.dismissals.iter().rposition(|d| &d.0 == doc && d.4 == e.workspace) {
                    self.dismissals.remove(i);
                }
            }
            _ => {}
        }
    }
}

/// What a search gets from learning for one document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoUse {
    /// The sum `A` of DESIGN §7.1 (edges to the anchors, plus the document's own tie to the query).
    pub a: f32,
    /// The strongest anchor and how often the two were opened together.
    pub best: Option<(String, u32)>,
    /// Opened after a similar query.
    pub tied: f32,
}

pub struct Learn {
    dir: PathBuf,
    salt: [u8; 16],
    state: RwLock<State>,
    file: Mutex<Option<(File, u64, u32)>>,
    offset: AtomicI64,
    /// DB4: the switches gewohnheiten.dateien (opens and adds) and gewohnheiten.suche (searches).
    files_on: std::sync::atomic::AtomicBool,
    searches_on: std::sync::atomic::AtomicBool,
}

fn read_events(dir: &Path) -> (Vec<Event>, u32) {
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl")))
        .collect();
    names.sort();
    let last = names.last().and_then(|p| p.file_stem()?.to_str()?.strip_prefix("events-")?.parse().ok()).unwrap_or(0);
    let mut out = Vec::new();
    for p in &names {
        let Ok(f) = File::open(p) else { continue };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            match serde_json::from_str::<Event>(&line) {
                Ok(e) => out.push(e),
                // A torn last line after a crash is dropped (DESIGN §4.3), never fatal.
                Err(_) => crate::event!("warn", "recover.torn_tail", file = "usage"),
            }
        }
    }
    (out, last)
}

impl Learn {
    pub fn open(data: &Path) -> Learn {
        let dir = data.join("usage");
        let _ = std::fs::create_dir_all(&dir);
        let salt = Self::salt(&dir);
        let (events, last) = read_events(&dir);
        let mut st = State::default();
        for e in &events {
            st.apply(e);
        }
        let on = || std::sync::atomic::AtomicBool::new(true);
        let l = Learn { dir, salt, state: RwLock::new(st), file: Mutex::new(None), offset: AtomicI64::new(0), files_on: on(), searches_on: on() };
        if let Ok(mut f) = l.file.lock() {
            *f = Self::open_file(&l.dir, last.max(1));
        }
        l
    }

    fn salt(dir: &Path) -> [u8; 16] {
        let p = dir.join("salt");
        if let Ok(b) = std::fs::read(&p)
            && let Ok(s) = <[u8; 16]>::try_from(b.as_slice())
        {
            return s;
        }
        let id = crate::util::new_id("s");
        let s: [u8; 16] = blake3::hash(id.as_bytes()).as_bytes()[..16].try_into().unwrap_or([0; 16]);
        let _ = crate::util::write_atomic(&p, &s, true);
        s
    }

    fn open_file(dir: &Path, n: u32) -> Option<(File, u64, u32)> {
        let f = OpenOptions::new().create(true).append(true).open(dir.join(format!("events-{n:06}.jsonl"))).ok()?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        Some((f, size, n))
    }

    /// Seconds since 1970, plus what `advance` moved the clock.
    pub fn now(&self) -> i64 {
        crate::util::now_secs() + self.offset.load(Ordering::Relaxed)
    }

    /// The replay's clock: forward only.
    pub fn advance(&self, secs: i64) {
        self.offset.fetch_add(secs.max(0), Ordering::Relaxed);
    }

    pub fn qkey(&self, query: &str, vec: Option<&[f32]>) -> QKey {
        let norm: String = crate::tokenize::words(query).join(" ");
        let mut h = blake3::Hasher::new();
        h.update(&self.salt);
        h.update(norm.as_bytes());
        QKey { hash: h.finalize().to_hex()[..32].to_string(), vec: vec.map(<[f32]>::to_vec) }
    }

    /// Appends and applies one event (its time is `now()` when 0).
    pub fn record(&self, mut e: Event) {
        let off = match e.ev.as_str() {
            "open" | "add" => !self.files_on.load(Ordering::Relaxed),
            "search" => !self.searches_on.load(Ordering::Relaxed),
            _ => false,
        };
        if off {
            return;
        }
        e.v = 1;
        if e.ts == 0 {
            e.ts = self.now();
        }
        if let Ok(line) = serde_json::to_string(&e)
            && let Ok(mut f) = self.file.lock()
        {
            if let Some((_, size, n)) = f.as_ref()
                && *size > ROTATE
            {
                *f = Self::open_file(&self.dir, n + 1);
            }
            if let Some((file, size, _)) = f.as_mut()
                && file.write_all(format!("{line}\n").as_bytes()).is_ok()
            {
                *size += line.len() as u64 + 1;
            }
        }
        if let Ok(mut st) = self.state.write() {
            st.apply(&e);
        }
    }

    pub fn search_event(&self, q: &QKey, results: Vec<String>, workspace: Option<String>) {
        self.record(Event { ev: "search".into(), qh: Some(q.hash.clone()), qv: q.vec.as_deref().map(encode_vec), results: Some(results), workspace, ..Default::default() });
    }

    pub fn events(&self) -> u64 {
        self.state.read().map(|s| s.events).unwrap_or(0)
    }

    /// The anchors of a plain search: documents opened after similar earlier queries, with their weight.
    pub fn anchors(&self, q: &QKey) -> Vec<(String, f64)> {
        let now = self.now();
        let Ok(st) = self.state.read() else { return Vec::new() };
        let mut m: HashMap<String, f64> = HashMap::new();
        for (doc, k, ts) in &st.ties {
            if k.similar(q) {
                *m.entry(doc.clone()).or_default() += decay_at(*ts, now);
            }
        }
        m.into_iter().collect()
    }

    /// The edge between two documents now (0 without one), and how often they were opened together.
    pub fn edge(&self, a: &str, b: &str) -> (f64, u32) {
        let now = self.now();
        let Ok(st) = self.state.read() else { return (0.0, 0) };
        st.edges.get(a).and_then(|m| m.get(b)).map_or((0.0, 0), |e| (e.grown * grow(now).recip(), e.count))
    }

    /// `A` for `doc` against `anchors` (DESIGN §7.1 `C`).
    pub fn co_use(&self, doc: &str, anchors: &[(String, f64)]) -> CoUse {
        let mut out = CoUse::default();
        let mut best = 0.0;
        for (a, w) in anchors {
            if a == doc {
                out.tied += *w as f32;
                continue;
            }
            let (e, n) = self.edge(doc, a);
            let part = e * w;
            if part > best {
                best = part;
                out.best = Some((a.clone(), n));
            }
            out.a += part as f32;
        }
        out.a += out.tied;
        out
    }

    /// `D` of DESIGN §7.1 `X`: decayed dismissals of `doc` for queries like `q` (or everywhere).
    pub fn dismissed(&self, doc: &str, q: &QKey) -> f32 {
        let now = self.now();
        let Ok(st) = self.state.read() else { return 0.0 };
        st.dismissals
            .iter()
            .filter(|d| d.0 == doc && (d.5 || d.1.as_ref().is_some_and(|k| k.similar(q))))
            .map(|d| d.2 * decay_at(d.3, now))
            .sum::<f64>() as f32
    }

    pub fn last_open(&self, doc: &str) -> Option<i64> {
        self.state.read().ok()?.last_open.get(doc).copied()
    }

    /// DB4: the switches of the one memory's page (off: nothing new is learned, what is there stays).
    pub fn set_switches(&self, files: bool, searches: bool) {
        self.files_on.store(files, Ordering::Relaxed);
        self.searches_on.store(searches, Ordering::Relaxed);
    }

    /// DB4: every document with co-use, its strongest partners (doc, times together) first; the strongest
    /// documents first, at most `max`.
    pub fn documents(&self, max: usize) -> Vec<(String, Vec<(String, u32)>)> {
        let now = self.now();
        let Ok(st) = self.state.read() else { return Vec::new() };
        type Ranked = (f64, String, Vec<(String, u32)>);
        let mut v: Vec<Ranked> = st
            .edges
            .iter()
            .map(|(doc, m)| {
                let mut p: Vec<(f64, String, u32)> = m.iter().map(|(o, e)| (e.grown * grow(now).recip(), o.clone(), e.count)).collect();
                p.sort_by(|a, b| b.0.total_cmp(&a.0));
                let sum = p.iter().map(|x| x.0).sum::<f64>();
                (sum, doc.clone(), p.into_iter().map(|(_, o, n)| (o, n)).collect())
            })
            .collect();
        v.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().take(max).map(|(_, d, p)| (d, p)).collect()
    }

    /// DB4: how many searches are remembered (as salted hashes).
    pub fn searches(&self) -> usize {
        self.state.read().map(|s| s.ties.len() + s.searches.len()).unwrap_or(0)
    }

    /// DB4: the usage log without what `keep` drops (or changes): one new file through temp + rename; only when
    /// that worked the old files go and the state is rebuilt from what is left. The number of events dropped, or
    /// why the disk said no (then nothing changed: a forget that is not on disk would come back at the next start).
    pub fn rewrite(&self, keep: impl Fn(Event) -> Option<Event>) -> Result<u64, String> {
        let mut f = self.file.lock().map_err(|_| "lock".to_string())?;
        let (events, last) = read_events(&self.dir);
        let before = events.len();
        let kept: Vec<Event> = events.into_iter().filter_map(keep).collect();
        let mut text = String::new();
        for e in &kept {
            if let Ok(l) = serde_json::to_string(e) {
                text.push_str(&l);
                text.push('\n');
            }
        }
        let next = last + 1;
        let old: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-")))
            .collect();
        crate::util::write_atomic(&self.dir.join(format!("events-{next:06}.jsonl")), text.as_bytes(), true).map_err(|e| e.to_string())?;
        *f = None;
        let mut stayed = None;
        for p in old {
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                stayed = Some(format!("{}: {e}", p.display()));
            }
        }
        *f = Self::open_file(&self.dir, next);
        let mut st = State::default();
        for e in &kept {
            st.apply(e);
        }
        if let Ok(mut s) = self.state.write() {
            *s = st;
        }
        stayed.map_or(Ok((before - kept.len()) as u64), Err)
    }

    /// DB4: everything learned about one document goes (its opens, adds and dismissals; out of search results).
    pub fn forget_doc(&self, doc: &str) -> Result<u64, String> {
        self.rewrite(|mut e| {
            if e.doc.as_deref() == Some(doc) {
                return None;
            }
            if let Some(w) = e.with.as_mut() {
                w.retain(|x| x != doc);
            }
            if let Some(r) = e.results.as_mut() {
                r.retain(|x| x != doc);
            }
            Some(e)
        })
    }

    /// DB4: the search memory goes (the searches and what they tied to opened documents).
    pub fn forget_searches(&self) -> Result<u64, String> {
        self.rewrite(|e| (e.ev != "search").then_some(e))
    }

    /// Forget everything learned: the usage files go, the state is empty (cold start, DESIGN §9.6).
    pub fn forget(&self) -> u64 {
        let n = self.events();
        if let Ok(mut f) = self.file.lock() {
            *f = None;
            for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
                let _ = std::fs::remove_file(e.path());
            }
            *f = Self::open_file(&self.dir, 1);
        }
        if let Ok(mut st) = self.state.write() {
            *st = State::default();
        }
        n
    }
}

/// Base64 (RFC 4648, with padding) for the query vectors; no crate needed for 40 lines.
mod b64 {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(b: &[u8]) -> String {
        let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
        for c in b.chunks(3) {
            let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                if i <= c.len() {
                    s.push(A[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    pub fn decode(s: &str) -> Option<Vec<u8>> {
        let val = |c: u8| A.iter().position(|a| *a == c).map(|p| p as u32);
        let b = s.as_bytes();
        if b.len() % 4 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(b.len() / 4 * 3);
        for q in b.chunks(4) {
            let pad = q.iter().rev().take_while(|c| **c == b'=').count();
            let mut n = 0u32;
            for (i, c) in q.iter().enumerate() {
                n |= if *c == b'=' { 0 } else { val(*c)? } << (18 - 6 * i);
            }
            out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..3 - pad]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: i64, kind: &str, doc: &str) -> Event {
        Event { ts, ev: kind.into(), doc: Some(doc.into()), ..Default::default() }
    }

    #[test]
    fn vectors_survive_base64() {
        let v = vec![0.25f32, -1.5, 3.0e-7, 42.0];
        assert_eq!(decode_vec(&encode_vec(&v)), Some(v));
        assert_eq!(b64::encode(b"ab"), "YWI=");
        assert_eq!(b64::decode("YWI="), Some(b"ab".to_vec()));
    }

    #[test]
    fn co_opens_make_edges_that_decay_by_half_in_ninety_days() {
        let d = std::env::temp_dir().join(format!("ctxd-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let l = Learn::open(&d);
        let t = l.now();
        l.record(ev(t, "open", "a"));
        l.record(ev(t + 60, "open", "b"));
        l.record(ev(t + 60 + SESSION_GAP_S + 1, "open", "c"));
        let (e, n) = l.edge("a", "b");
        assert!((e - (-60.0f64 / 300.0).exp()).abs() < 1e-3 && n == 1, "{e} {n}");
        assert_eq!(l.edge("b", "c").1, 0, "a new session after the gap");
        l.advance(90 * 86_400);
        assert!((l.edge("a", "b").0 / e - 0.5).abs() < 0.01);
        // What was recorded comes back after a restart.
        let again = Learn::open(&d);
        assert_eq!(again.edge("a", "b").1, 1);
        assert_eq!(again.forget(), 3);
        assert_eq!(Learn::open(&d).events(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_scanner_teaches_nothing() {
        let d = std::env::temp_dir().join(format!("ctxd-learn-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let l = Learn::open(&d);
        let t = l.now();
        for i in 0..40 {
            l.record(Event { program: Some("thumbnailer".into()), ..ev(t, "open", &format!("d{i}")) });
        }
        assert!(l.edge("d30", "d31").1 == 0, "opens past the 20th within 10 s are ignored");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_document_and_the_search_memory_can_be_forgotten_alone() {
        let d = std::env::temp_dir().join(format!("ctxd-learn-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let l = Learn::open(&d);
        let t = l.now();
        let q = l.qkey("steuer", None);
        l.search_event(&q, vec!["a".into(), "b".into()], None);
        l.record(ev(t, "open", "a"));
        l.record(ev(t + 10, "open", "b"));
        l.record(ev(t + 20, "open", "c"));
        assert_eq!(l.documents(10).len(), 3);
        assert!(l.searches() > 0);
        assert_eq!(l.forget_doc("b"), Ok(1));
        assert_eq!(l.edge("a", "b").1, 0);
        assert_eq!(l.edge("a", "c").1, 1, "the others stay");
        let disk: String = std::fs::read_dir(d.join("usage"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("events-"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(!disk.contains("\"b\""), "b is gone from disk: {disk}");
        // A disk that refuses the new file: nothing is forgotten (it would come back at the next start).
        let usage = d.join("usage");
        let moved = d.join("usage-away");
        std::fs::rename(&usage, &moved).unwrap();
        std::fs::write(&usage, b"").unwrap();
        assert!(l.forget_doc("c").is_err());
        assert_eq!(l.edge("a", "c").1, 1, "still known, as still on disk");
        std::fs::remove_file(&usage).unwrap();
        std::fs::rename(&moved, &usage).unwrap();
        assert_eq!(l.forget_searches(), Ok(1));
        assert_eq!(Learn::open(&d).searches(), 0);
        l.set_switches(false, true);
        l.record(ev(t + 30, "open", "d"));
        assert_eq!(l.last_open("d"), None, "switched off: no new opens");
        let _ = std::fs::remove_dir_all(&d);
    }
}
