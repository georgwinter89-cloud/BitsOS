//! Where settings live and how they are written. On the stick: `\BITSOS2\settings\*.toml` + `journal.log`, and
//! kernel keys as lines of `\BITSOS2\CONFIG.TXT` (FS-7 allows writes below `\BITSOS2\` only). Without a writable
//! stick: the fallback directory (`/var/lib/bitsos/settings`, RAM), where `CONFIG.TXT` holds only the kernel
//! lines still to be carried to the stick, and a `PENDING` mark says a resync is due.
//!
//! Everything that touches the stick (writes, the probe, reads) goes through one worker thread so a hung stick
//! cannot hang the service: the caller waits up to a deadline, then moves to the fallback (the stuck worker is
//! abandoned; its later jobs are skipped by generation). Files are replaced by writing `<name>.new` and renaming it over the old one (FS-7 renames onto
//! an existing file; a cut leaves the old or the new file, never half of one).

use crate::guard;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Paths {
    /// `/boot/BITSOS2` on V2.
    pub stick: PathBuf,
    pub fallback: PathBuf,
    /// `/sys/bitsos`.
    pub sys: PathBuf,
    pub socket: PathBuf,
}

impl Default for Paths {
    fn default() -> Paths {
        Paths {
            stick: "/boot/BITSOS2".into(),
            fallback: "/var/lib/bitsos/settings".into(),
            sys: "/sys/bitsos".into(),
            socket: "/run/bitsos/settings.sock".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Stick,
    Fallback(String),
}

impl Paths {
    pub fn dir(&self, p: &Place) -> PathBuf {
        match p {
            Place::Stick => self.stick.join("settings"),
            Place::Fallback(_) => self.fallback.clone(),
        }
    }
    pub fn config(&self, p: &Place) -> PathBuf {
        match p {
            Place::Stick => self.stick.join("CONFIG.TXT"),
            Place::Fallback(_) => self.fallback.join("CONFIG.TXT"),
        }
    }
    pub fn pending(&self) -> PathBuf {
        self.fallback.join("PENDING")
    }

    /// Whether the stick's settings directory can be used: `\BITSOS2\` exists and the directory exists or can be
    /// made. Err is the reason (ENODEV without a stick, EROFS read-only, ...).
    pub fn probe_stick(&self) -> Result<(), String> {
        if !self.stick.is_dir() {
            return Err(format!("{} missing (no stick)", self.stick.display()));
        }
        fs::create_dir_all(self.stick.join("settings")).map_err(|e| format!("{}: {e}", self.stick.display()))
    }
}

#[derive(Clone, Debug)]
pub enum Job {
    Write(PathBuf, Vec<u8>),
    Append(PathBuf, Vec<u8>),
    /// Set or remove one CONFIG.TXT key (guarded lines byte-identical).
    Config(PathBuf, String, Option<String>),
    /// The same edit without writing: run before a kernel key is applied, so a refusal changes nothing.
    ConfigCheck(PathBuf, String, Option<String>),
    Remove(PathBuf),
}

impl Job {
    fn run(&self) -> Result<(), String> {
        match self {
            Job::Write(p, b) => write_atomic(p, b),
            Job::Append(p, b) => append(p, b),
            Job::Config(p, k, v) => edit_config_file(p, k, v.as_deref(), true),
            Job::ConfigCheck(p, k, v) => edit_config_file(p, k, v.as_deref(), false),
            Job::Remove(p) => match fs::remove_file(p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", p.display())),
                _ => Ok(()),
            },
        }
    }
}

pub fn write_atomic(p: &Path, b: &[u8]) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", p.display());
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).map_err(err)?;
    }
    let mut tmp = p.as_os_str().to_owned();
    tmp.push(".new");
    let tmp = PathBuf::from(tmp);
    let mut f = fs::File::create(&tmp).map_err(err)?;
    f.write_all(b).map_err(err)?;
    f.sync_all().map_err(err)?;
    drop(f);
    fs::rename(&tmp, p).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(e)
    })
}

fn append(p: &Path, b: &[u8]) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", p.display());
    if let Some(d) = p.parent() {
        fs::create_dir_all(d).map_err(err)?;
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(p).map_err(err)?;
    f.write_all(b).map_err(err)?;
    f.sync_data().map_err(err)
}

/// Marks an error about the file's content (not the device): no reason to leave the stick for the fallback.
pub const CONTENT: &str = "content: ";

/// Reads CONFIG.TXT fresh before every edit, so a line Georg changed meanwhile is kept.
fn edit_config_file(p: &Path, key: &str, value: Option<&str>, write: bool) -> Result<(), String> {
    let old = match fs::read(p) {
        Ok(b) => String::from_utf8(b).map_err(|_| format!("{CONTENT}{}: not UTF-8, left alone", p.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", p.display())),
    };
    let new = guard::edit_config(&old, key, value).map_err(|e| format!("{CONTENT}{e}"))?;
    if write && new != old { write_atomic(p, new.as_bytes()) } else { Ok(()) }
}

/// Reads a file; a missing one is empty.
pub fn read_text(p: &Path) -> Result<String, String> {
    match fs::read(p) {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

type Task = Box<dyn FnOnce() + Send>;

/// The single worker thread for everything that touches the stick (writes, the probe, reads at load and
/// resync), so a hung stick can never hang the caller: every call waits at most `deadline`. The generation is
/// bumped when the caller gives up on a worker, so tasks still queued behind a hung one are dropped instead of
/// landing on the stick later. `busy` says whether the task in hand still runs; the registry does not touch
/// the stick again while an abandoned worker is still busy, so its late write always lands before the next
/// resync reads the stick (and at most one thread per hang is ever stuck in the kernel).
#[derive(Clone)]
pub struct Writer {
    tx: Sender<(u64, Task)>,
    generation: Arc<AtomicU64>,
    busy: Arc<AtomicBool>,
    pub deadline: Duration,
}

/// Prefix of the error a call gives when its deadline passed.
pub const SLOW: &str = "slow: ";

impl Writer {
    pub fn new(deadline: Duration) -> Writer {
        let (tx, rx) = mpsc::channel::<(u64, Task)>();
        let generation = Arc::new(AtomicU64::new(0));
        let busy = Arc::new(AtomicBool::new(false));
        let (g, b) = (generation.clone(), busy.clone());
        // A failed spawn leaves `rx` dropped: every call then fails and the caller falls back, no panic.
        let _ = std::thread::Builder::new().name("settings-writer".into()).spawn(move || {
            for (task_gen, task) in rx {
                // A dropped task drops its reply channel: the caller sees "abandoned".
                if task_gen == g.load(Ordering::Acquire) {
                    b.store(true, Ordering::Release);
                    task();
                    b.store(false, Ordering::Release);
                }
            }
        });
        Writer { tx, generation, busy, deadline }
    }

    /// Runs `f` on the worker; Err when the deadline passes (prefixed [`SLOW`]) or the worker is gone.
    pub fn call<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
        let (rtx, rrx) = mpsc::channel();
        let g = self.generation.load(Ordering::Acquire);
        let task: Task = Box::new(move || {
            let _ = rtx.send(f());
        });
        self.tx.send((g, task)).map_err(|_| "writer thread gone".to_string())?;
        match rrx.recv_timeout(self.deadline) {
            Ok(v) => Ok(v),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("{SLOW}stick slower than {} ms (hung?)", self.deadline.as_millis()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("abandoned".into()),
        }
    }

    /// Runs `jobs` in order; Err on the first failure or when the deadline passes.
    pub fn run(&self, jobs: Vec<Job>) -> Result<(), String> {
        if jobs.is_empty() {
            return Ok(());
        }
        self.call(move || jobs.iter().try_for_each(Job::run))?
    }

    /// Gives up on this worker: whatever it has queued is skipped. Returns its busy flag (true while the
    /// task in hand is still stuck).
    pub fn abandon(&self) -> Arc<AtomicBool> {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.busy.clone()
    }
}

/// What a place holds, read on the worker; parsed (with events) by the registry outside it.
pub struct RawPlace {
    pub files: Vec<(PathBuf, Result<String, String>)>,
    pub config: Result<String, String>,
    pub journal: String,
    /// `*.new` leftovers of an interrupted or abandoned write that were removed.
    pub cleaned: Vec<PathBuf>,
}

/// Reads a place's group files, CONFIG.TXT and journal; first removes `.new` leftovers of settingsd's own
/// writes (a cut or an abandoned write; the renamed file is the valid one).
pub fn read_raw(paths: &Paths, place: &Place) -> RawPlace {
    let dir = paths.dir(place);
    let mut cleaned = Vec::new();
    let config = paths.config(place);
    let mut stale: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
                    n.ends_with(".toml.new") || n == "journal.log.new" || n == "pending.new" || n == "config.txt.new"
                })
                .collect()
        })
        .unwrap_or_default();
    let mut cfg_new = config.as_os_str().to_owned();
    cfg_new.push(".new");
    stale.push(PathBuf::from(cfg_new));
    for p in stale {
        if fs::remove_file(&p).is_ok() {
            cleaned.push(p);
        }
    }
    let mut names: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("toml")))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let files = names.into_iter().map(|p| {
        let t = read_text(&p);
        (p, t)
    });
    RawPlace {
        files: files.collect(),
        config: read_text(&config),
        journal: read_text(&dir.join("journal.log")).unwrap_or_default(),
        cleaned,
    }
}

/// Writes one sysfs attribute: open without O_CREAT/O_TRUNC (an attribute is never made by a writer) and one
/// write of the whole value (linux-6.6/fs/kernfs/file.c kernfs_fop_write_iter takes it at offset 0).
/// Ok(false): there is no such attribute (the kernel does not offer the key at run time).
pub fn write_attr(p: &Path, text: &str) -> Result<bool, std::io::Error> {
    let mut f = match fs::OpenOptions::new().write(true).open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let n = f.write(text.as_bytes())?;
    if n != text.len() {
        return Err(std::io::Error::other(format!("short write {n} of {}", text.len())));
    }
    Ok(true)
}
