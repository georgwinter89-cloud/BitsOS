//! FS-8c host tests: `writeback::sync_all`'s stop pass. Every file's flush comes before every file system's
//! flush, every flush before any stop; a failing stop is counted in `failed`, a stop with nothing to do is not
//! counted, and nothing waits (a stop that would wait for a thaw returns at once).

extern crate std;

use crate::pagecache::SyncKind;
use crate::writeback::{self, Flush};
use crate::Errno;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::SpinLock;
use uapi::errno::*;

static LOG: SpinLock<Vec<String>> = SpinLock::new(Vec::new());

#[derive(Clone, Copy)]
enum Stop {
    Nothing,
    Did,
    Fails,
}

struct Fake {
    name: &'static str,
    stop: Stop,
}

impl Flush for Fake {
    fn flush(&self, _: SyncKind) -> Result<(), Errno> {
        LOG.lock().push(alloc::format!("flush {}", self.name));
        Ok(())
    }
    fn stop(&self) -> Result<bool, Errno> {
        LOG.lock().push(alloc::format!("stop {}", self.name));
        match self.stop {
            Stop::Nothing => Ok(false),
            Stop::Did => Ok(true),
            Stop::Fails => Err(EIO),
        }
    }
}

fn fake(name: &'static str, stop: Stop) -> Arc<dyn Flush> {
    Arc::new(Fake { name, stop })
}

#[test]
fn sync_all_flushes_files_then_file_systems_then_stops() {
    let file = fake("fs8c-file", Stop::Nothing);
    let (a, b, c) = (fake("fs8c-a", Stop::Did), fake("fs8c-b", Stop::Fails), fake("fs8c-c", Stop::Nothing));
    writeback::register(Arc::downgrade(&file) as Weak<dyn Flush>).unwrap();
    for f in [&a, &b, &c] {
        writeback::register_fs(Arc::downgrade(f) as Weak<dyn Flush>).unwrap();
    }
    let t0 = std::time::Instant::now();
    let p = writeback::sync_all();
    assert!(t0.elapsed() < std::time::Duration::from_secs(5), "sync_all returned in {:?}", t0.elapsed());
    // Other tests' objects may be in the lists too: only ours are looked at.
    let log: Vec<String> = LOG.lock().iter().filter(|l| l.contains("fs8c-")).cloned().collect();
    let pos = |s: &str| log.iter().position(|l| l == s).unwrap_or_else(|| panic!("{s} missing in {log:?}"));
    assert!(pos("flush fs8c-file") < pos("flush fs8c-a"), "files before file systems: {log:?}");
    for f in ["a", "b", "c"] {
        let last_flush = ["a", "b", "c"].iter().map(|g| pos(&alloc::format!("flush fs8c-{g}"))).max().unwrap();
        assert!(last_flush < pos(&alloc::format!("stop fs8c-{f}")), "every flush before stop {f}: {log:?}");
    }
    assert!(!log.contains(&String::from("stop fs8c-file")), "files have no stop: {log:?}");
    assert!(p.failed >= 1, "the failing stop is counted: {p:?}");
    // sync_durable: the flushes only, no stop.
    LOG.lock().clear();
    let q = writeback::sync_durable();
    let log: Vec<String> = LOG.lock().iter().filter(|l| l.contains("fs8c-")).cloned().collect();
    assert!(log.iter().all(|l| l.starts_with("flush ")), "sync_durable never stops: {log:?}");
    assert!(log.len() >= 4, "{log:?}");
    let _ = q;
    drop((file, a, b, c));
    // Dropped objects are pruned, not flushed.
    LOG.lock().clear();
    writeback::sync_all();
    assert!(!LOG.lock().iter().any(|l| l.contains("fs8c-")));
}
