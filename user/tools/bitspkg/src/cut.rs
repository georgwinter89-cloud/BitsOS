//! Cut points for the "a cut during install leaves the old version working" test. Every durable step of a
//! changing command calls `step`; with `BITSPKG_CUT_AT=N` the process ends at once before step N, as if the
//! power went off (no clean-up runs; open files and the store lock are dropped by the kernel). Cost: one
//! counter increment per file system step on the cold install path, nothing on any program's hot path.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub const ENV: &str = "BITSPKG_CUT_AT";
/// Exit status of a cut process, so a test tells a cut from a refusal.
pub const EXIT: i32 = 86;

static STEP: AtomicU64 = AtomicU64::new(0);
static AT: OnceLock<Option<u64>> = OnceLock::new();

pub fn step(what: &str) {
    let n = STEP.fetch_add(1, Ordering::Relaxed) + 1;
    let at = AT.get_or_init(|| std::env::var(ENV).ok().and_then(|v| v.parse().ok()));
    if *at == Some(n) {
        eprintln!("bitspkg: cut before step {n} ({what})");
        std::process::exit(EXIT);
    }
}

/// Steps taken so far by this process (printed by `--steps` runs of the tests).
pub fn taken() -> u64 {
    STEP.load(Ordering::Relaxed)
}
