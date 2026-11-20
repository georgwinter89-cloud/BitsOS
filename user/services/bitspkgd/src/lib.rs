//! bitspkgd (PK-6f, desk plan PK-6/PK-7 section 1.10): how the desktop and Claude reach bitspkg. A root service on
//! `/run/bitsos/pkg.sock`, one JSON request per line as settingsd (`ops`), running bitspkg's library in its own
//! process: the `/apps` lock and the cut safety stay in the library, one changing run at a time (`work`), readers
//! never wait for it (a slow mirror never blocks `list`, B4).
//!
//! - `peer`: who may change (root, the compositor grant holder bitscomp) and who only reads;
//! - `plans`: what `plan` found, kept by id until `do`;
//! - `progress`: the library's events and named points as `progress` lines;
//! - `mcp`: Claude's tools (`programs_search`/`programs_info` read; `program_install`/`program_remove`/
//!   `programs_refresh` only after the kernel's installer gate, bitsinstall's `Kind::Program`, D6);
//! - `timer`: D5, the daily look for updates (never installs unless `pkg.updates.install = night`).
#![forbid(unsafe_code)]

pub mod client;
pub mod mcp;
pub mod ops;
pub mod peer;
pub mod planning;
pub mod plans;
pub mod progress;
pub mod server;
pub mod timer;
pub mod update;
pub mod work;

use bitspkg::repo::sources::Config;
use bitspkg::store::Store;
use std::path::PathBuf;
use std::sync::Mutex;

pub const SOCKET: &str = "/run/bitsos/pkg.sock";

/// What every thread of the service shares. A `Store` is made per use (its hooks are not shared between threads).
pub struct Ctx {
    pub root: PathBuf,
    /// settingsd's socket: None = the library's default for the root, Some(None) = none (`--settings none`).
    pub settings: Option<Option<PathBuf>>,
    /// `--sources FILE` (tests): replaces settingsd's `pkg.sources.*` keys.
    pub sources: Option<PathBuf>,
    pub plans: Mutex<plans::Plans>,
    pub work: work::Work,
    pub started: std::time::Instant,
    readers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// One running reader (`Ctx::reader`); its place is free again when it is dropped.
pub struct ReaderSlot(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl Ctx {
    pub fn new(root: PathBuf, settings: Option<Option<PathBuf>>, sources: Option<PathBuf>) -> Ctx {
        Ctx { root, settings, sources, plans: Mutex::new(plans::Plans::default()), work: work::Work::default(), started: std::time::Instant::now(), readers: Default::default() }
    }
    /// How many readers may run at once: eight per CPU (R1: the machine's size, not a fixed count).
    pub fn reader_limit(&self) -> usize {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).saturating_mul(8)
    }
    /// A place for one more reader, None when `reader_limit` run already.
    pub fn reader(&self) -> Option<ReaderSlot> {
        use std::sync::atomic::Ordering;
        let limit = self.reader_limit();
        self.readers.try_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < limit).then_some(n + 1)).ok()?;
        Some(ReaderSlot(self.readers.clone()))
    }
    pub fn store(&self) -> Store {
        let mut st = Store::new(&self.root);
        if let Some(s) = &self.settings {
            st.hooks = bitspkg::hooks::Hooks::standard(s.clone());
        }
        st
    }
    /// The sources as bitspkg's command line reads them (settingsd's keys, registered on the way, or the file).
    pub fn config(&self, st: &Store) -> Result<Config, String> {
        bitspkg::repo::cli::config(st, self.sources.as_ref().and_then(|p| p.to_str()))
    }
    pub fn plans(&self) -> std::sync::MutexGuard<'_, plans::Plans> {
        self.plans.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// B6: an event line in bitspkg's style (`bitspkg: event=pkg.… k=v`), on stderr, which `serve` points at
/// /dev/kmsg (the journal).
pub fn event(name: &str, kv: &[(&str, &str)]) {
    bitspkg::repo::refresh::event(name, kv);
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
