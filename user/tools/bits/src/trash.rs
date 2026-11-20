//! The trash on disk (PLAN K6, eins-c.md §2): per file system `<fs>/.papierkorb/<id>/` with `record.json` =
//! `{"v":1, "id", "fs", "path", "what", "time", "pid", "command", "chat", "tree"}` and the removed names under
//! `files/<their path below the file system's top>`; on the boot stick the trash is `/boot/BITSOS2/home/.papierkorb`.
//! Restoring and emptying are the kernel's (`/dev/bitsos/papierkorb`, which checks the caller); without that node (a
//! kernel without the trash, the host tests) bits moves the names itself with the caller's own rights.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::out::{Fail, R};

pub const DIR: &str = ".papierkorb";
const RECORD: &str = "record.json";
const CONTENT: &str = "files";
/// Where the trash of a file system whose top takes no writes lives (the boot stick).
const STICK_HOME: &str = "BITSOS2/home";
const NODE: &str = "/dev/bitsos/papierkorb";
/// File systems that keep their files over a restart (K6: tmpfs, /proc, /run are never diverted).
const PERSISTENT: &[&str] = &["ext4", "ext3", "ext2", "vfat", "msdos", "exfat", "btrfs", "xfs", "f2fs"];

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: String,
    /// The file system's top (the record's `fs`).
    pub mount: String,
    pub dir: PathBuf,
    /// The common top of what the entry holds (the record's `path`).
    pub original: String,
    pub time: u64,
    pub pid: u64,
    pub command: String,
    pub chat: Option<String>,
    pub kind: &'static str,
    pub bytes: u64,
}

/// The mount points to look at: `BITS_PAPIERKORB_MOUNTS` (colon separated, tests), else /proc/mounts' persistent ones.
pub fn mounts() -> Vec<String> {
    if let Ok(m) = std::env::var("BITS_PAPIERKORB_MOUNTS") {
        return m.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect();
    }
    let text = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let mut v: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let (_, at, ty) = (w.next()?, w.next()?, w.next()?);
            PERSISTENT.contains(&ty).then(|| at.replace("\\040", " "))
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

fn size(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).map(|rd| rd.filter_map(Result::ok).map(|e| size(&e.path())).sum()).unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

pub fn size_text(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{b} Bytes"),
    }
}

fn read(mount: &str, dir: PathBuf) -> Option<Entry> {
    let rec: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(RECORD)).ok()?).ok()?;
    let original = rec["path"].as_str()?.to_string();
    let fs = rec["fs"].as_str().unwrap_or(mount).to_string();
    let content = moved(&dir, &fs, &original);
    let kind = if content.is_dir() { "folder" } else { "file" };
    Some(Entry {
        id: dir.file_name()?.to_string_lossy().into_owned(),
        original,
        time: rec["time"].as_u64().unwrap_or(0),
        pid: rec["pid"].as_u64().unwrap_or(0),
        command: rec["command"].as_str().unwrap_or("").to_string(),
        chat: rec["chat"].as_str().map(str::to_string),
        kind,
        bytes: size(&dir.join(CONTENT)),
        mount: fs,
        dir,
    })
}

/// Where `original` (below the file system `fs`) lies in the entry `dir`.
fn moved(dir: &Path, fs: &str, original: &str) -> PathBuf {
    let below = Path::new(original).strip_prefix(fs).unwrap_or(Path::new(original));
    dir.join(CONTENT).join(below.strip_prefix("/").unwrap_or(below))
}

/// Every entry of every trash, newest first.
pub fn entries() -> R<Vec<Entry>> {
    let mut out = Vec::new();
    for m in mounts() {
        for t in [Path::new(&m).join(DIR), Path::new(&m).join(STICK_HOME).join(DIR)] {
            let Ok(rd) = std::fs::read_dir(&t) else { continue };
            out.extend(rd.filter_map(Result::ok).filter(|e| e.path().is_dir()).filter_map(|e| read(&m, e.path())));
        }
    }
    out.sort_by(|a, b| b.time.cmp(&a.time).then(a.id.cmp(&b.id)));
    out.dedup_by(|a, b| a.dir == b.dir);
    Ok(out)
}

pub fn find(id: &str) -> R<Entry> {
    entries()?.into_iter().find(|e| e.id == id).ok_or_else(|| Fail::new("not_found", format!("Im Papierkorb gibt es „{id}“ nicht.")))
}

/// One command to the kernel's node (its fields one per line); None when there is no node.
fn node(fields: &[&str]) -> Option<std::io::Result<String>> {
    let path = std::env::var("BITS_PAPIERKORB_NODE").unwrap_or_else(|_| NODE.into());
    let mut f = match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(Err(e)),
    };
    let text: String = fields.iter().map(|x| format!("{x}\n")).collect();
    Some(f.write_all(text.as_bytes()).and_then(|()| {
        let mut answer = String::new();
        f.read_to_string(&mut answer).map(|_| answer.trim().to_string())
    }))
}

fn taken(to: &str) -> Fail {
    Fail::failed(format!("{to} gibt es schon; mit --nach <pfad> geht es an einen anderen Ort. Nichts verschoben."))
}

/// Puts the entry back (at its place, or its top at `to`); never over something that is there now.
pub fn restore(e: &Entry, to: &str) -> R<()> {
    let target = Path::new(to);
    if !target.is_absolute() {
        return Err(Fail::usage("Der Pfad zum Zurücklegen beginnt mit /."));
    }
    let same = to == e.original;
    let fields: Vec<&str> = if same { vec!["restore", &e.mount, &e.id] } else { vec!["restore-to", &e.mount, &e.id, to] };
    match node(&fields) {
        Some(Ok(_)) => Ok(()),
        Some(Err(err)) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(taken(if same { &e.original } else { to })),
        Some(Err(err)) if err.raw_os_error() == Some(18) => Err(Fail::failed(format!("{to}: zurücklegen geht nur auf demselben Datenträger."))),
        Some(Err(err)) => Err(Fail::failed(format!("{to}: {err}"))),
        None => restore_here(e, target),
    }
}

/// Without the kernel's node: the names move back with the caller's own rights (only the entry's top, so a
/// restore into a folder that is still there checks the one name).
fn restore_here(e: &Entry, target: &Path) -> R<()> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(taken(&target.to_string_lossy()));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| Fail::failed(format!("{}: {err}", parent.display())))?;
    }
    let from = moved(&e.dir, &e.mount, &e.original);
    std::fs::rename(&from, target).map_err(|err| Fail::failed(format!("{}: {err} (zurücklegen geht nur auf demselben Datenträger)", target.display())))?;
    std::fs::remove_dir_all(&e.dir).map_err(|err| Fail::failed(format!("{}: {err}", e.dir.display())))
}

/// Removes the entry for good (the kernel's `empty`, which refuses the assistant's tree).
pub fn remove(e: &Entry) -> R<()> {
    match node(&["empty", &e.mount, &e.id]) {
        Some(Ok(_)) => Ok(()),
        Some(Err(err)) => Err(Fail::failed(format!("{}: {err}", e.dir.display()))),
        None => std::fs::remove_dir_all(&e.dir).map_err(|err| Fail::failed(format!("{}: {err}", e.dir.display()))),
    }
}
