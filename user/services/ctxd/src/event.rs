//! B6: structured events, ids and numbers only (never document or query text, DESIGN §9.7).
//! One JSON object per line into `<data>/log/events.jsonl`, and to stderr when `CTXD_LOG=stderr`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static SINK: Mutex<Option<File>> = Mutex::new(None);

pub fn open_sink(data: &Path) {
    let dir = data.join("log");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl"))
        && let Ok(mut s) = SINK.lock()
    {
        *s = Some(f);
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
    if let Ok(mut s) = SINK.lock()
        && let Some(f) = s.as_mut()
    {
        let _ = f.write_all(line.as_bytes());
    }
    if std::env::var_os("CTXD_LOG").is_some_and(|v| v == "stderr") {
        eprint!("{line}");
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
