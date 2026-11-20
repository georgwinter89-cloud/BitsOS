//! The access log on /data: one merged line per day, program and thing (`days/YYYY-MM-DD.jsonl`, UTC days),
//! the last hour's entries as they came (memory only, for "what did it just do"), and the first-time marks
//! (`seen`: program + kind of thing + the last day it was touched). Retention by days and by size (the marks by
//! days too); files are written whole and renamed. The very first batch after the log began (no `seen` file yet)
//! only learns: what was there before is not "new". A program's hosts and listening ports count as new only while
//! it has few of them: a browser or a game talks to hundreds, and "new" would then say nothing.

use crate::model::{Entry, device_class, private_root};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Hosts (and listening ports) per program that are still marked when new; beyond, they are neither marked nor
/// remembered.
pub const NEW_HOSTS_MAX: usize = 10;
/// Kinds of thing whose marks are capped per program.
const CAPPED: [&str; 2] = ["netz:", "lauscht:"];
/// Separates a mark's parts in `seen`.
const SEP: char = '\u{1f}';
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub type Key = (String, String, String, String, u16);

/// The raw entries kept for short questions.
pub const RECENT_MS: u64 = 3_600_000;
pub const RECENT_MAX: usize = 20_000;
const DAY_MS: u64 = 86_400_000;

#[derive(Default)]
pub struct Store {
    pub dir: PathBuf,
    pub days: BTreeMap<String, BTreeMap<Key, Entry>>,
    pub recent: VecDeque<Entry>,
    /// Mark (program + kind of thing) -> the last day it was touched.
    seen: BTreeMap<String, String>,
    learning: bool,
    dirty: BTreeSet<String>,
    seen_dirty: bool,
    /// The kernel's state as the last batch said it.
    pub on: bool,
    pub lost: u64,
}

/// The UTC day of unix ms `t`.
pub fn day_of(t: u64) -> String {
    bitsjournal::local_time(t.max(1), 0)[..10].to_string()
}

/// What "new" is judged by: the kind of thing an entry touches (None: never marked).
pub fn category(e: &Entry, roots: &[String]) -> Option<String> {
    let c = if e.is_file() {
        format!("privat:{}", private_root(&e.what, roots)?)
    } else if e.kind == "device" {
        format!("gerät:{}", device_class(&e.what))
    } else if e.kind == "connect" || e.kind == "accept" {
        if crate::model::host_words(&e.what) == "dieser Rechner" {
            return None;
        }
        format!("netz:{}", e.what)
    } else if e.kind == "listen" {
        format!("lauscht:{}:{}", e.proto, e.port)
    } else if e.is_right() {
        format!("recht:{}", e.kind)
    } else {
        return None;
    };
    Some(format!("{}{SEP}{c}", e.exe))
}

/// A `seen` line: the mark and its last day (lines without a day are from before days were kept: today).
fn seen_line(l: &str, today: &str) -> (String, String) {
    match l.rsplit_once(SEP) {
        Some((mark, day)) if mark.contains(SEP) => (mark.to_string(), day.to_string()),
        _ => (l.to_string(), today.to_string()),
    }
}

impl Store {
    /// An empty log for `dir`, not read yet (`load_disk` once /data can be written); its first batch only learns.
    pub fn in_memory(dir: &Path) -> Store {
        Store { dir: dir.to_path_buf(), learning: true, ..Store::default() }
    }

    /// Loads what `dir` holds (missing files are an empty log).
    pub fn open(dir: &Path) -> Store {
        let mut s = Store { dir: dir.to_path_buf(), ..Store::default() };
        if let Ok(rd) = fs::read_dir(dir.join("days")) {
            for f in rd.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                let Some(day) = name.strip_suffix(".jsonl") else { continue };
                let text = fs::read_to_string(f.path()).unwrap_or_default();
                let m = s.days.entry(day.to_string()).or_default();
                for e in text.lines().filter_map(|l| mcpreg::Json::parse(l).ok()).filter_map(|j| Entry::from_json(&j)) {
                    m.entry(e.key()).and_modify(|x| x.absorb(&e)).or_insert(e);
                }
            }
        }
        let today = day_of(crate::now_ms());
        match fs::read_to_string(dir.join("seen")) {
            Ok(t) => s.seen = t.lines().filter(|l| !l.is_empty()).map(|l| seen_line(l, &today)).collect(),
            Err(_) => s.learning = true,
        }
        s
    }

    /// /data came later than the log: what the disk holds joins what was kept in memory meanwhile (marks too;
    /// the batches before only learned when the disk had no marks yet).
    pub fn load_disk(&mut self) {
        let disk = Store::open(&self.dir);
        for (day, m) in disk.days {
            let mine = self.days.entry(day.clone()).or_default();
            for (k, e) in m {
                mine.entry(k).and_modify(|x| x.absorb(&e)).or_insert(e);
            }
            self.dirty.insert(day);
        }
        self.dirty.extend(self.days.keys().cloned());
        self.seen_dirty |= !self.seen.is_empty();
        for (mark, day) in disk.seen {
            let d = self.seen.entry(mark).or_insert_with(|| day.clone());
            *d = (*d).clone().max(day);
        }
        self.learning &= disk.learning;
    }

    /// Takes a batch: marks first times, keeps the raw entries of the last hour, merges into the days.
    pub fn add(&mut self, entries: Vec<Entry>, roots: &[String], now: u64) {
        let learning = self.learning && !entries.is_empty();
        for mut e in entries {
            let day = day_of(e.t1);
            if let Some(c) = category(&e, roots) {
                e.new = self.mark(c, &day) && !learning;
            }
            self.dirty.insert(day.clone());
            self.days.entry(day).or_default().entry(e.key()).and_modify(|x| x.absorb(&e)).or_insert_with(|| e.clone());
            self.recent.push_back(e);
        }
        if learning {
            self.learning = false;
            self.seen_dirty = true;
        }
        while self.recent.len() > RECENT_MAX || self.recent.front().is_some_and(|e| e.t1 + RECENT_MS < now) {
            self.recent.pop_front();
        }
    }

    /// Notes that `mark` was touched on `day`; true when it was not known (and may be remembered).
    fn mark(&mut self, mark: String, day: &str) -> bool {
        if let Some(d) = self.seen.get_mut(&mark) {
            if d.as_str() < day {
                *d = day.to_string();
                self.seen_dirty = true;
            }
            return false;
        }
        if let Some((prog, kind)) = mark.split_once(SEP)
            && let Some(cap) = CAPPED.iter().find(|c| kind.starts_with(**c))
        {
            let prefix = format!("{prog}{SEP}{cap}");
            if self.seen.range(prefix.clone()..).take_while(|(m, _)| m.starts_with(&prefix)).count() >= NEW_HOSTS_MAX {
                return false;
            }
        }
        self.seen.insert(mark, day.to_string());
        self.seen_dirty = true;
        true
    }

    /// The marks remembered (for tests and the notes' checks).
    pub fn marks(&self) -> usize {
        self.seen.len()
    }

    fn write_whole(path: &Path, text: &str) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, path)
    }

    /// Writes the days that changed and the marks.
    pub fn flush(&mut self) -> io::Result<()> {
        let days = self.dir.join("days");
        fs::create_dir_all(&days)?;
        for d in std::mem::take(&mut self.dirty) {
            let Some(m) = self.days.get(&d) else { continue };
            let mut text = String::new();
            for e in m.values() {
                text.push_str(&e.to_json().to_string());
                text.push('\n');
            }
            if let Err(e) = Self::write_whole(&days.join(format!("{d}.jsonl")), &text) {
                self.dirty.insert(d);
                return Err(e);
            }
        }
        if self.seen_dirty {
            let text: String = self.seen.iter().map(|(m, d)| format!("{m}{SEP}{d}\n")).collect();
            Self::write_whole(&self.dir.join("seen"), &text)?;
            self.seen_dirty = false;
        }
        Ok(())
    }

    /// Without /data: only the last day stays in memory.
    pub fn forget_before(&mut self, t: u64) {
        let cut = day_of(t);
        self.days.retain(|d, _| *d >= cut);
    }

    /// Bytes the day files take.
    pub fn bytes(&self) -> u64 {
        fs::read_dir(self.dir.join("days")).map(|rd| rd.flatten().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum()).unwrap_or(0)
    }

    /// Drops days older than `keep_days`, then the oldest days while the files take more than `max_bytes` (today
    /// stays). Answers the days removed.
    pub fn retain(&mut self, now: u64, keep_days: u64, max_bytes: u64) -> Vec<String> {
        let cut = day_of(now.saturating_sub(keep_days * DAY_MS));
        let today = day_of(now);
        let mut gone: Vec<String> = self.days.keys().filter(|d| **d < cut).cloned().collect();
        let size = |d: &str| fs::metadata(self.dir.join("days").join(format!("{d}.jsonl"))).map_or(0, |m| m.len());
        let mut total: u64 = self.days.keys().filter(|d| !gone.contains(d)).map(|d| size(d)).sum();
        for d in self.days.keys() {
            if total <= max_bytes || *d == today {
                break;
            }
            if !gone.contains(d) {
                total = total.saturating_sub(size(d));
                gone.push(d.clone());
            }
        }
        for d in &gone {
            self.days.remove(d);
            self.dirty.remove(d);
            let _ = fs::remove_file(self.dir.join("days").join(format!("{d}.jsonl")));
        }
        // A mark not touched for `keep_days` goes too: the log no longer knows that time.
        let before = self.seen.len();
        self.seen.retain(|_, d| *d >= cut);
        self.seen_dirty |= self.seen.len() != before;
        gone
    }

    /// The day entries whose last touch is at or after `since` (unix ms).
    pub fn since(&self, since: u64) -> impl Iterator<Item = &Entry> {
        let first = day_of(since);
        self.days.range(first..).flat_map(|(_, m)| m.values()).filter(move |e| e.t1 >= since)
    }

    /// The raw entries of the last hour at or after `since`.
    pub fn recent_since(&self, since: u64) -> impl Iterator<Item = &Entry> {
        self.recent.iter().filter(move |e| e.t1 >= since)
    }

    /// The oldest kept time (unix ms), for answers that say how far back the log reaches.
    pub fn oldest(&self) -> Option<u64> {
        self.days.values().next().and_then(|m| m.values().map(|e| e.t0).min())
    }
}
