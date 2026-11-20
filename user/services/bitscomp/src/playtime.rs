//! GM-6b: BitsOS's own play time (plan/spec/sofa-mode.md 3.4): from the moment a window of a program maps (on any
//! workspace) until its last window goes, per program id, in
//! `/data/home/1000/.local/share/bitsos/playtime.txt` (one line per program: id, last start, seconds today, seconds
//! total; `BITSCOMP_PLAYTIME` for tests). No Steam file is read for it. The file is read and written on a thread of
//! its own (B4), written whole to a temporary file and renamed; a bad line is left out with a note, never a panic.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

pub const PATH: &str = "/data/home/1000/.local/share/bitsos/playtime.txt";
/// Bounds of the file (R1: per input): a line and the whole file.
pub const MAX_LINE: usize = 512;
pub const MAX_FILE: u64 = 1 << 20;
const HEAD: &str = "# BitsOS play time (sofa-mode.md 3.4): id, last start (Unix s), seconds today, seconds total\n";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rec {
    /// The last start, Unix seconds.
    pub last: i64,
    /// Seconds on the day of `last`.
    pub today: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Book {
    pub recs: BTreeMap<String, Rec>,
}

/// A program id as the file keeps it: an address word (no blanks, no tabs).
fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Book {
    /// The file's text: the records and the lines left out (with why).
    pub fn parse(text: &str) -> (Book, Vec<String>) {
        let mut b = Book::default();
        let mut bad = Vec::new();
        for (n, l) in text.lines().enumerate() {
            let l = l.trim_end_matches('\r');
            if l.trim().is_empty() || l.starts_with('#') {
                continue;
            }
            if l.len() > MAX_LINE {
                bad.push(format!("line {}: longer than {MAX_LINE} bytes", n + 1));
                continue;
            }
            let f: Vec<&str> = l.split('\t').collect();
            let rec = match f.as_slice() {
                [id, last, today, total] if id_ok(id) => match (last.parse(), today.parse(), total.parse()) {
                    (Ok(last), Ok(today), Ok(total)) => Some((id.to_string(), Rec { last, today, total })),
                    _ => None,
                },
                _ => None,
            };
            match rec {
                Some((id, r)) => {
                    b.recs.insert(id, r);
                }
                None => bad.push(format!("line {}: not `id<TAB>last<TAB>today<TAB>total`", n + 1)),
            }
        }
        (b, bad)
    }

    pub fn encode(&self) -> String {
        let mut s = String::from(HEAD);
        for (id, r) in &self.recs {
            s.push_str(&format!("{id}\t{}\t{}\t{}\n", r.last, r.today, r.total));
        }
        s
    }

    /// A run of `id` from `start` to `end`; `day` gives a time's day number (today counts again on a new day).
    pub fn add(&mut self, id: &str, start: i64, end: i64, day: impl Fn(i64) -> i64) {
        if !id_ok(id) {
            return;
        }
        let secs = (end - start).max(0) as u64;
        let r = self.recs.entry(id.to_string()).or_default();
        let same_day = r.last != 0 && day(r.last) == day(start);
        r.today = if same_day { r.today.saturating_add(secs) } else { secs };
        r.total = r.total.saturating_add(secs);
        r.last = r.last.max(start);
    }
}

/// The windows counted now: (window id, program id, mapped at) and, per program, the start of a run whose first
/// windows went while others stay.
#[derive(Default, Debug)]
pub struct Clock {
    open: Vec<(u64, String, i64)>,
    run: HashMap<String, i64>,
}

impl Clock {
    pub fn mapped(&mut self, win: u64, prog: &str, now: i64) {
        if !self.open.iter().any(|o| o.0 == win) {
            self.open.push((win, prog.to_string(), now));
        }
    }

    pub fn counts(&self, win: u64) -> bool {
        self.open.iter().any(|o| o.0 == win)
    }

    /// Window `win` went: the run (program, start, end) when it was its program's last window.
    pub fn gone(&mut self, win: u64, now: i64) -> Option<(String, i64, i64)> {
        let i = self.open.iter().position(|o| o.0 == win)?;
        let (_, prog, mapped) = self.open.remove(i);
        let start = self.run.get(&prog).map_or(mapped, |s| (*s).min(mapped));
        if self.open.iter().any(|o| o.1 == prog) {
            self.run.insert(prog, start);
            return None;
        }
        self.run.remove(&prog);
        Some((prog, start, now))
    }
}

/// The file's path.
pub fn path() -> PathBuf {
    PathBuf::from(PATH)
}

fn load(p: &Path) -> Result<String, String> {
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut v = Vec::new();
    f.take(MAX_FILE + 1).read_to_end(&mut v).map_err(|e| e.to_string())?;
    if v.len() as u64 > MAX_FILE {
        return Err(format!("larger than {} KiB", MAX_FILE / 1024));
    }
    Ok(String::from_utf8_lossy(&v).into_owned())
}

fn save(p: &Path, text: &str) -> Result<(), String> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tmp = p.with_extension("txt.new");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, p)).map_err(|e| e.to_string())
}

/// The file's thread: it reads the file once (`loaded` gets the book on the main loop) and writes every text sent.
pub fn start<D: 'static>(handle: &LoopHandle<'static, D>, p: PathBuf, loaded: impl Fn(&mut D, Book) + 'static) -> Result<mpsc::Sender<String>, String> {
    let (tx, rx) = channel::channel::<Book>();
    let (jtx, jrx) = mpsc::channel::<String>();
    handle
        .insert_source(rx, move |ev, _, d| {
            if let channel::Event::Msg(b) = ev {
                loaded(d, b);
            }
        })
        .map_err(|e| format!("play time channel: {e}"))?;
    std::thread::Builder::new().name("bitscomp-playtime".into()).spawn(move || worker(p, tx, jrx)).map_err(|e| format!("play time thread: {e}"))?;
    Ok(jtx)
}

/// The book from the file, and whether writing it is safe: a file that exists but cannot be read is moved to
/// `playtime.txt.bad` first, so the next run never overwrites a history it could not read; if even that fails,
/// nothing is written (the old file stays as it is).
fn open_book(p: &Path) -> (Book, bool) {
    match load(p) {
        Ok(t) => {
            let (b, bad) = Book::parse(&t);
            for l in bad {
                eprintln!("bitscomp: playtime: {}: {l} (left out)", p.display());
            }
            (b, true)
        }
        Err(e) => {
            let aside = p.with_extension("txt.bad");
            match std::fs::rename(p, &aside) {
                Ok(()) => {
                    eprintln!("bitscomp: playtime: {} not read ({e}): kept as {}, counting from zero", p.display(), aside.display());
                    (Book::default(), true)
                }
                Err(r) => {
                    eprintln!("bitscomp: playtime: {} not read ({e}) and not moved aside ({r}): play time is not written", p.display());
                    (Book::default(), false)
                }
            }
        }
    }
}

fn worker(p: PathBuf, tx: Sender<Book>, jobs: mpsc::Receiver<String>) {
    let (book, writable) = open_book(&p);
    if tx.send(book).is_err() {
        return;
    }
    while let Ok(mut text) = jobs.recv() {
        // Only the newest text counts.
        while let Ok(t) = jobs.try_recv() {
            text = t;
        }
        if !writable {
            continue;
        }
        if let Err(e) = save(&p, &text) {
            eprintln!("bitscomp: playtime: {} not written: {e}", p.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    fn day(t: i64) -> i64 {
        t.div_euclid(DAY)
    }

    #[test]
    fn the_file_round_trips_and_bad_lines_are_left_out() {
        let text = "# head\nsteam-32360\t1730500000\t60\t7200\nkaputt\nvkquake\tx\t1\t2\nmit leer\t1\t2\t3\n\nbrave\t5\t0\t9\n";
        let (b, bad) = Book::parse(text);
        assert_eq!(b.recs.len(), 2, "{b:?}");
        assert_eq!(b.recs["steam-32360"], Rec { last: 1730500000, today: 60, total: 7200 });
        assert_eq!(bad.len(), 3, "{bad:?}");
        let (again, none) = Book::parse(&b.encode());
        assert_eq!(again, b);
        assert!(none.is_empty());
        let long = format!("{}\t1\t2\t3\n", "a".repeat(600));
        assert!(Book::parse(&long).0.recs.is_empty(), "a line over the bound is left out");
        assert!(Book::parse("\u{0}\u{ffff}\t\t\t\n").0.recs.is_empty());
    }

    #[test]
    fn runs_add_up_today_and_in_total() {
        let mut b = Book::default();
        let t = 100 * DAY + 3600;
        b.add("steam-1", t, t + 600, day);
        b.add("steam-1", t + 1000, t + 1300, day);
        assert_eq!(b.recs["steam-1"], Rec { last: t + 1000, today: 900, total: 900 });
        b.add("steam-1", t + DAY, t + DAY + 60, day);
        assert_eq!(b.recs["steam-1"], Rec { last: t + DAY, today: 60, total: 960 }, "a new day starts today again");
        b.add("bad id", t, t + 5, day);
        b.add("steam-2", t + 50, t, day);
        assert!(!b.recs.contains_key("bad id"));
        assert_eq!(b.recs["steam-2"].total, 0, "a run backwards counts nothing");
    }

    #[test]
    fn a_program_counts_from_its_first_window_to_its_last() {
        let mut c = Clock::default();
        c.mapped(1, "steam-440", 100);
        c.mapped(1, "steam-440", 150);
        c.mapped(2, "steam-440", 200);
        c.mapped(3, "vkquake", 250);
        assert_eq!(c.gone(1, 300), None, "window 2 still runs");
        assert_eq!(c.gone(3, 400), Some(("vkquake".into(), 250, 400)));
        assert_eq!(c.gone(2, 500), Some(("steam-440".into(), 100, 500)));
        assert_eq!(c.gone(2, 600), None, "gone once");
        assert_eq!(c.gone(9, 600), None, "a window that was never counted");
    }

    #[test]
    fn the_file_is_written_whole_and_read_back() {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("playtime-test");
        let _ = std::fs::remove_dir_all(&dir);
        let p = dir.join("sub/playtime.txt");
        assert_eq!(load(&p).unwrap(), "", "no file: nothing yet");
        let mut b = Book::default();
        b.add("steam-32360", 10, 70, day);
        save(&p, &b.encode()).unwrap();
        assert_eq!(Book::parse(&load(&p).unwrap()).0, b);
        assert!(!p.with_extension("txt.new").exists());
    }

    #[test]
    fn an_unreadable_file_is_kept_aside_not_overwritten() {
        let dir = std::path::PathBuf::from(std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into())).join("playtime-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("playtime.txt");
        let big = "x".repeat(MAX_FILE as usize + 1);
        std::fs::write(&p, &big).unwrap();
        let (b, writable) = open_book(&p);
        assert!(b.recs.is_empty() && writable);
        assert!(!p.exists(), "moved aside");
        assert_eq!(std::fs::read_to_string(p.with_extension("txt.bad")).unwrap(), big, "the history is kept");
        let (_, ok) = open_book(&p);
        assert!(ok, "no file now: a fresh start writes");
    }
}
