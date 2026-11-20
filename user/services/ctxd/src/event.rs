//! B6: structured events, ids and numbers only (never document or query text, DESIGN §9.7).
//! One JSON object per line into `<data>/log/events.jsonl` (kept to two files of `ROTATE` bytes: the older
//! half moves to `events.1.jsonl`), and to stderr when `CTXD_LOG=stderr`. The V2 service (`CONSOLE`) also
//! writes its state changes to stderr, which init hands to the kernel log; searches stay in the file.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

struct Sink {
    file: File,
    path: PathBuf,
    size: u64,
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);
/// Service state changes to stderr as one short line each (system mode).
pub static CONSOLE: AtomicBool = AtomicBool::new(false);
const ROTATE: u64 = 4 << 20;

pub fn open_sink(data: &Path) {
    let dir = data.join("log");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("events.jsonl");
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path)
        && let Ok(mut s) = SINK.lock()
    {
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        *s = Some(Sink { file, path, size });
    }
}

fn write_sink(line: &str) {
    let Ok(mut s) = SINK.lock() else { return };
    let Some(k) = s.as_mut() else { return };
    if k.size + line.len() as u64 > ROTATE {
        let _ = std::fs::rename(&k.path, k.path.with_file_name("events.1.jsonl"));
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(&k.path) {
            k.file = f;
            k.size = 0;
        }
    }
    if k.file.write_all(line.as_bytes()).is_ok() {
        k.size += line.len() as u64;
    }
}

pub fn emit(level: &str, name: &str, mut fields: serde_json::Value) {
    if let Some(o) = fields.as_object_mut() {
        o.insert("ts".into(), crate::util::now_ms().to_string().into());
        o.insert("level".into(), level.into());
        o.insert("subsys".into(), "ctxd".into());
        o.insert("name".into(), name.into());
    }
    let line = format!("{fields}\n");
    write_sink(&line);
    if std::env::var_os("CTXD_LOG").is_some_and(|v| v == "stderr") {
        eprint!("{line}");
    } else if CONSOLE.load(Ordering::Relaxed) && name != "search" && level != "debug" {
        let mut short = format!("ctxd {name}");
        if let Some(o) = fields.as_object() {
            for (k, v) in o.iter().filter(|(k, _)| !matches!(k.as_str(), "ts" | "level" | "subsys" | "name")) {
                short.push_str(&format!(" {k}={v}"));
            }
        }
        eprintln!("{}", crate::util::cut_bytes(&short, 400));
    }
}

/// `event!("info", "index.finished", docs = 12, chunks = 40)`.
#[macro_export]
macro_rules! event {
    ($lvl:literal, $name:literal $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::event::emit($lvl, $name, serde_json::json!({ $(stringify!($k): $v),* }))
    };
}

/// Metrics (DESIGN §8 B6): counters read by `context_status`.
pub struct Metrics {
    pub docs: AtomicU64,
    pub chunks: AtomicU64,
    pub queue_len: AtomicU64,
    pub extract_failed_total: AtomicU64,
    pub searches: AtomicU64,
    pub search_ms_total: AtomicU64,
    pub search_partial: AtomicU64,
}

pub static METRICS: Metrics = Metrics {
    docs: AtomicU64::new(0),
    chunks: AtomicU64::new(0),
    queue_len: AtomicU64::new(0),
    extract_failed_total: AtomicU64::new(0),
    searches: AtomicU64::new(0),
    search_ms_total: AtomicU64::new(0),
    search_partial: AtomicU64::new(0),
};

impl Metrics {
    pub fn snapshot(&self) -> serde_json::Value {
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        serde_json::json!({
            "ctxd_docs": g(&self.docs), "ctxd_chunks": g(&self.chunks), "ctxd_queue_len": g(&self.queue_len),
            "ctxd_extract_failed_total": g(&self.extract_failed_total), "ctxd_searches": g(&self.searches),
            "ctxd_search_ms_total": g(&self.search_ms_total), "ctxd_search_partial": g(&self.search_partial),
        })
    }
}
