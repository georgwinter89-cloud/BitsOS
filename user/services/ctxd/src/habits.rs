//! DB4: habits, the second half of the one local memory (the first is `learn.rs`, files and searches). The
//! desktop tells ctxd what Georg does (C4 `habit_event`: programs started, windows closed in which state, sound,
//! picks, the sofa mode); ctxd keeps only what a learned habit needs, as records `<data>/habits/events-NNNNNN.jsonl`
//! (append-only, one record per line, same store rules as `learn.rs`: local, 0700 folder of ctxd, decay).
//! A record belongs to exactly one learned item (`part:key`), so forgetting an item deletes its records from
//! disk (the files are rewritten without them) and nothing of it stays behind. What the desktop did a moment ago
//! (C4 `history`) is kept in memory only, never on disk.
//! The rules that turn records into habits are in `habits_rules.rs`; AI never decides (DECISIONS 24.10.).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

/// The parts of the memory, each with its own switch `gewohnheiten.<part>` (German name for the page).
pub const PARTS: [(&str, &str); 10] = [
    ("fenster", "Fenstergröße je Programm"),
    ("breite", "Kachelbreite je Programm"),
    ("zusammen", "Programme, die du zusammen startest"),
    ("zeiten", "Uhrzeiten, zu denen du Programme startest"),
    ("ton", "Ton je Programm"),
    ("auswahl", "Was du oft auswählst"),
    ("sofa", "Sofa-Modus"),
    ("vorrang", "Vorrang je Programm"),
    ("dateien", "Dateien, die zusammengehören"),
    ("suche", "Suchgedächtnis"),
];
/// Two programs started this close together count as started together.
pub const TOGETHER_S: i64 = 120;
/// Records older than this go at the next rewrite: a habit of half a year ago is no habit any more.
pub const KEEP_DAYS: i64 = 180;
/// Bound on what is kept (a desktop that floods ctxd cannot fill the disk).
pub const MAX_RECORDS: usize = 50_000;
/// An overflow trims to this many, so the next records do not each cost a full rewrite.
pub const TRIM_TO: usize = MAX_RECORDS * 9 / 10;
/// Records past KEEP_DAYS are pruned at the start and at most this often after it.
const PRUNE_EVERY_S: i64 = 86_400;
/// A desktop event's kind: short, lower case (`program_start`, DB1's ui kinds); anything else is refused.
const KIND_MAX: usize = 40;
const HISTORY_MAX: usize = 500;
const HISTORY_S: i64 = 3600;
const ROTATE: u64 = 4 << 20;
/// Longest text kept from the desktop (a program id, a window title, an address).
const TEXT_MAX: usize = 200;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Rec {
    pub v: u32,
    pub ts: i64,
    pub part: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub val: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num: Option<f64>,
    /// Local hour (0-23) as the desktop saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<u8>,
    /// The program's or target's German name as the desktop shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Rec {
    pub fn id(&self) -> String {
        format!("{}:{}", self.part, self.key)
    }

    /// The item this record belongs to: its own, or for a hint, an undo or a yes the item it was about.
    pub fn item(&self) -> String {
        match self.part.as_str() {
            "hint" | "undo" | "ja" => self.key.clone(),
            _ => self.id(),
        }
    }
}

/// One line of C4 `history`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Recent {
    pub t: i64,
    pub kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub app: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub element: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Default)]
struct Mem {
    recs: Vec<Rec>,
    /// Program starts of the last minutes (pairing for "together"; also while `zeiten` is off).
    starts: Vec<(String, i64)>,
    history: VecDeque<Recent>,
}

pub struct Habits {
    dir: PathBuf,
    mem: RwLock<Mem>,
    file: Mutex<Option<(File, u64, u32)>>,
    /// gewohnheiten.enabled and one switch per part (missing: on).
    switches: RwLock<HashMap<String, bool>>,
    offset: std::sync::atomic::AtomicI64,
    /// When records past KEEP_DAYS were last pruned (`now()` time).
    pruned: std::sync::atomic::AtomicI64,
}

fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn cut(s: &str) -> String {
    crate::util::cut_bytes(s, TEXT_MAX).to_string()
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("events-") && n.ends_with(".jsonl")))
        .collect();
    v.sort();
    v
}

fn number(p: &Path) -> u32 {
    p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.strip_prefix("events-")).and_then(|n| n.parse().ok()).unwrap_or(0)
}

impl Habits {
    pub fn open(data: &Path) -> Habits {
        let dir = data.join("habits");
        let _ = crate::util::private_dir(&dir);
        let mut recs = Vec::new();
        let names = files(&dir);
        for p in &names {
            let Ok(f) = File::open(p) else { continue };
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                match serde_json::from_str::<Rec>(&line) {
                    Ok(r) => recs.push(r),
                    Err(_) => crate::event!("warn", "recover.torn_tail", file = "habits"),
                }
            }
        }
        let last = names.last().map(|p| number(p)).unwrap_or(0).max(1);
        let h = Habits {
            dir,
            mem: RwLock::new(Mem { recs, ..Default::default() }),
            file: Mutex::new(None),
            switches: RwLock::new(HashMap::new()),
            offset: std::sync::atomic::AtomicI64::new(0),
            pruned: std::sync::atomic::AtomicI64::new(i64::MIN),
        };
        if let Ok(mut f) = h.file.lock() {
            *f = open_file(&h.dir, last);
        }
        h.prune_due();
        h
    }

    /// Drops records older than KEEP_DAYS from memory and disk, at most once a day (a habit unused for half a
    /// year leaves nothing behind); true when it rewrote.
    pub fn prune_due(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let now = self.now();
        let last = self.pruned.load(Relaxed);
        if last != i64::MIN && now - last < PRUNE_EVERY_S {
            return false;
        }
        self.pruned.store(now, Relaxed);
        let old = self.mem.read().map(|m| m.recs.iter().any(|r| now - r.ts > KEEP_DAYS * 86_400)).unwrap_or(false);
        old && self.rewrite(|_| true, MAX_RECORDS).is_ok()
    }

    pub fn now(&self) -> i64 {
        crate::util::now_secs() + self.offset.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Moves the clock forward (tests of the decay), never backward.
    pub fn advance(&self, secs: i64) {
        self.offset.fetch_add(secs.max(0), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether `part` learns (the master switch and its own).
    pub fn on(&self, part: &str) -> bool {
        let Ok(s) = self.switches.read() else { return true };
        s.get("enabled").copied().unwrap_or(true) && s.get(part).copied().unwrap_or(true)
    }

    pub fn switch(&self, part: &str) -> bool {
        self.switches.read().ok().and_then(|s| s.get(part).copied()).unwrap_or(true)
    }

    pub fn set_switch(&self, part: &str, on: bool) {
        if let Ok(mut s) = self.switches.write() {
            s.insert(part.to_string(), on);
        }
    }

    /// A copy of every record (the rules read it).
    pub fn records(&self) -> Vec<Rec> {
        self.mem.read().map(|m| m.recs.clone()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.mem.read().map(|m| m.recs.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends one record (its time `now()` when 0); a part that is switched off keeps nothing.
    pub fn record(&self, mut r: Rec) {
        let learns = matches!(r.part.as_str(), "hint" | "undo" | "ja") || self.on(&r.part);
        if !learns {
            return;
        }
        r.v = 1;
        if r.ts == 0 {
            r.ts = self.now();
        }
        let Ok(line) = serde_json::to_string(&r) else { return };
        if let Ok(mut f) = self.file.lock() {
            if let Some((_, size, n)) = f.as_ref()
                && *size > ROTATE
            {
                *f = open_file(&self.dir, n + 1);
            }
            if let Some((file, size, _)) = f.as_mut()
                && file.write_all(format!("{line}\n").as_bytes()).is_ok()
            {
                *size += line.len() as u64 + 1;
            }
        }
        let full = if let Ok(mut m) = self.mem.write() {
            m.recs.push(r);
            m.recs.len() > MAX_RECORDS
        } else {
            false
        };
        if full && self.rewrite(|_| true, TRIM_TO).is_err() {
            crate::event!("warn", "habits.trim_failed", records = self.len());
        }
    }

    /// C4 `habit_event`: one thing the desktop saw Georg do, into history and the records it teaches.
    pub fn event(&self, a: &Value) -> Result<(), String> {
        let kind = s(a, "kind").ok_or("kind is required")?.to_string();
        if kind.len() > KIND_MAX || !kind.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-' || b == b'.') {
            return Err(format!("kind must be at most {KIND_MAX} bytes of a-z, 0-9, _ - ."));
        }
        let app = s(a, "app").map(cut).unwrap_or_default();
        let ts = self.now();
        let h = a.get("hour").and_then(Value::as_u64).filter(|h| *h < 24).map(|h| h as u8);
        let name = s(a, "name").map(cut);
        self.remember(Recent {
            t: ts,
            kind: kind.clone(),
            app: app.clone(),
            window: s(a, "window").map(cut),
            element: s(a, "element").map(cut),
            // DB1's ui actions may carry typed text: memory only, cut, never on disk.
            text: s(a, "text").map(cut),
        });
        let need_app = || if app.is_empty() { Err("app is required".to_string()) } else { Ok(()) };
        let rec = |part: &str, key: &str| Rec { ts, part: part.into(), key: key.into(), h, name: name.clone(), ..Default::default() };
        match kind.as_str() {
            "program_start" => {
                need_app()?;
                let partners: Vec<String> = match self.mem.write() {
                    Ok(mut m) => {
                        m.starts.retain(|(_, t)| ts - t <= TOGETHER_S);
                        let p = m.starts.iter().filter(|(x, _)| *x != app).map(|(x, _)| x.clone()).collect::<Vec<_>>();
                        m.starts.retain(|(x, _)| *x != app);
                        m.starts.push((app.clone(), ts));
                        p
                    }
                    Err(_) => Vec::new(),
                };
                self.record(rec("zeiten", &app));
                let mut done: Vec<String> = Vec::new();
                for p in partners {
                    if done.contains(&p) {
                        continue;
                    }
                    let mut pair = [p.clone(), app.clone()];
                    pair.sort();
                    self.record(Rec { name: None, ..rec("zusammen", &pair.join("+")) });
                    done.push(p);
                }
            }
            "window_close" => {
                need_app()?;
                // No state: a window Georg did not size himself (a dialog, the sofa mode's, a minimised one) teaches
                // nothing; the close is history only.
                let Some(state) = s(a, "state") else { return Ok(()) };
                if !matches!(state, "maximised" | "fullscreen" | "tiled" | "floating") {
                    return Err(format!("state {:?}: maximised, fullscreen, tiled or floating", crate::util::cut_bytes(state, 40)));
                }
                self.record(Rec { val: Some(state.into()), ..rec("fenster", &app) });
                if let Some(w) = a.get("tile").and_then(Value::as_f64).filter(|w| state == "tiled" && w.is_finite() && *w > 0.0) {
                    self.record(Rec { num: Some(w), ..rec("breite", &app) });
                }
            }
            "sound" => {
                need_app()?;
                let out = s(a, "output").map(cut);
                let vol = a.get("volume").and_then(Value::as_f64).filter(|v| (0.0..=150.0).contains(v));
                if out.is_none() && vol.is_none() {
                    return Err("output or volume is required".into());
                }
                self.record(Rec { val: out, num: vol, ..rec("ton", &app) });
            }
            "pick" => {
                let target = s(a, "target").ok_or("target is required")?;
                let via = s(a, "via").unwrap_or("address");
                self.record(Rec { val: Some(cut(via)), ..rec("auswahl", &cut(target)) });
            }
            "sofa" => {
                if a.get("on").and_then(Value::as_bool).unwrap_or(true) {
                    self.record(rec("sofa", "sofa"));
                }
            }
            "vorrang" => {
                if let Some((app, v)) = crate::habits_vorrang::teaches(&app, s(a, "value"), s(a, "by"))? {
                    self.record(Rec { val: Some(v), ..rec(crate::habits_vorrang::PART, &app) });
                }
            }
            // Georg's own "Ja" to a proposal (only bitscomp sends it, after his real input).
            "vorrang_ja" => {
                need_app()?;
                if crate::habits_vorrang::own_service(&app) {
                    return Err("a service of BitsOS gets no Vorrang".into());
                }
                self.record(Rec { part: "ja".into(), key: format!("{}:{app}", crate::habits_vorrang::PART), ts, ..Default::default() });
            }
            "hint_shown" | "undo" => {
                let id = s(a, "id").ok_or("id is required")?;
                let part = if kind == "undo" { "undo" } else { "hint" };
                // A proposal's "Nein" is kept as such, so the page says why it is not made.
                let val = (kind == "undo" && a.get("nein").and_then(Value::as_bool) == Some(true)).then(|| "nein".to_string());
                self.record(Rec { part: part.into(), key: cut(id), ts, val, ..Default::default() });
            }
            // History only: window open, program stop, ui actions (DB1), anything new from the desktop.
            _ => {}
        }
        Ok(())
    }

    fn remember(&self, r: Recent) {
        if let Ok(mut m) = self.mem.write() {
            let now = r.t;
            m.history.push_back(r);
            while m.history.len() > HISTORY_MAX || m.history.front().is_some_and(|f| now - f.t > HISTORY_S) {
                m.history.pop_front();
            }
        }
    }

    /// C4 `history`: the newest `max` actions of the last `since_s` seconds, oldest first.
    pub fn history(&self, since_s: i64, max: usize) -> Vec<Recent> {
        let now = self.now();
        let Ok(m) = self.mem.read() else { return Vec::new() };
        let v: Vec<Recent> = m.history.iter().filter(|r| now - r.t <= since_s).cloned().collect();
        v[v.len().saturating_sub(max)..].to_vec()
    }

    /// Deletes item `id`'s records (also its hints and undos) from memory and disk; how many went. An error when
    /// the disk could not be rewritten: then nothing is forgotten (it would come back at the next start).
    pub fn forget(&self, id: &str) -> Result<usize, String> {
        self.rewrite(|r| r.item() != id, MAX_RECORDS)
    }

    /// Everything learned goes: the files are deleted, the memory is empty (the history too). An error when a file
    /// stayed on disk; the memory is empty then too, so nothing more is learned from it until the next start.
    pub fn forget_all(&self) -> Result<usize, String> {
        let n = self.len();
        let mut failed = None;
        if let Ok(mut f) = self.file.lock() {
            *f = None;
            for p in files(&self.dir) {
                if let Err(e) = std::fs::remove_file(&p)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    failed = Some(e.to_string());
                }
            }
            *f = open_file(&self.dir, 1);
        }
        if let Ok(mut m) = self.mem.write() {
            *m = Mem::default();
        }
        failed.map_or(Ok(n), Err)
    }

    /// Keeps the records `keep` says yes to (and younger than KEEP_DAYS, at most `limit`, newest kept), as one new
    /// file written through a temp file and a rename; only when that worked the memory follows and the old files
    /// go. The number dropped, or why the disk said no (then memory and disk are as before).
    pub fn rewrite(&self, keep: impl Fn(&Rec) -> bool, limit: usize) -> Result<usize, String> {
        let now = self.now();
        let mut f = self.file.lock().map_err(|_| "lock".to_string())?;
        let mut m = self.mem.write().map_err(|_| "lock".to_string())?;
        let mut kept: Vec<Rec> = m.recs.iter().filter(|r| keep(r) && now - r.ts <= KEEP_DAYS * 86_400).cloned().collect();
        if kept.len() > limit {
            let extra = kept.len() - limit;
            kept.drain(..extra);
        }
        let old = files(&self.dir);
        let next = old.last().map(|p| number(p)).unwrap_or(0) + 1;
        let mut text = String::new();
        for r in &kept {
            if let Ok(l) = serde_json::to_string(r) {
                text.push_str(&l);
                text.push('\n');
            }
        }
        let new = self.dir.join(format!("events-{next:06}.jsonl"));
        crate::util::write_atomic(&new, text.as_bytes(), true).map_err(|e| e.to_string())?;
        *f = None;
        let mut stayed = None;
        for p in old {
            if let Err(e) = std::fs::remove_file(&p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                stayed = Some(format!("{}: {e}", p.display()));
            }
        }
        *f = open_file(&self.dir, next);
        let dropped = m.recs.len() - kept.len();
        m.recs = kept;
        // An old file that stayed would bring its records back at the next start: not forgotten.
        stayed.map_or(Ok(dropped), Err)
    }
}

fn open_file(dir: &Path, n: u32) -> Option<(File, u64, u32)> {
    let f = OpenOptions::new().create(true).append(true).open(dir.join(format!("events-{n:06}.jsonl"))).ok()?;
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    Some((f, size, n))
}
