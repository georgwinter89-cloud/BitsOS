//! File events on the roots (DESIGN §3): one inotify watch per directory wakes the indexer soon after a
//! change, so a deleted or renamed file leaves the index in seconds, not at the next rescan. The periodic
//! rescan stays the safety net: on a queue overflow (`watch.overflow`), when the watch limit of the system is
//! reached (`watch.limit`), and on V2, where inotify is a stub that never fires (rust-ecosystem.md).

use crate::engine::Engine;
use crate::roots::{self, Root};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn mask() -> WatchMask {
    WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MODIFY
        | WatchMask::CLOSE_WRITE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::ATTRIB
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF
        | WatchMask::ONLYDIR
        | WatchMask::DONT_FOLLOW
}

struct Watcher {
    ino: Inotify,
    dirs: HashMap<WatchDescriptor, PathBuf>,
    excludes: Vec<String>,
    /// ctxd's own data folder: its writes must not wake the indexer, even when a root contains it.
    skip: PathBuf,
    full: bool,
}

impl Watcher {
    /// Watch `dir` and every directory below it that the walk would enter (same rules as `roots::scan`).
    fn add_tree(&mut self, dir: &Path, dev: u64) {
        for d in roots::walk_dirs(dir, dev, &self.excludes, &self.skip) {
            if self.full {
                return;
            }
            match self.ino.watches().add(&d, mask()) {
                Ok(wd) => {
                    self.dirs.insert(wd, d);
                }
                Err(e) if e.raw_os_error() == Some(28) => {
                    // ENOSPC: fs.inotify.max_user_watches is reached. The rescan covers the rest.
                    self.full = true;
                    crate::event!("warn", "watch.limit", watched = self.dirs.len(), fallback = "rescan");
                }
                // The directory went away between the walk and the watch: the next event or scan sees it.
                Err(_) => {}
            }
        }
    }
}

/// Start the watcher thread; `None` when inotify is unavailable (the rescan alone then keeps the index).
pub fn start(e: Arc<Engine>, roots: Vec<Root>, data: PathBuf) -> Option<std::thread::JoinHandle<()>> {
    let ino = match Inotify::init() {
        Ok(i) => i,
        Err(x) => {
            crate::event!("warn", "watch.unavailable", reason = x.kind().to_string(), fallback = "rescan");
            return None;
        }
    };
    let skip = std::fs::canonicalize(&data).unwrap_or(data);
    let mut w = Watcher { ino, dirs: HashMap::new(), excludes: e.cfg.excludes.clone(), skip, full: false };
    let debounce = Duration::from_millis(e.cfg.index.watch_debounce_ms.max(50));
    std::thread::Builder::new()
        .name("ctxd-watch".into())
        .spawn(move || {
            for r in &roots {
                w.add_tree(&r.path, r.dev);
            }
            crate::event!("info", "watch.started", roots = roots.len(), watched = w.dirs.len());
            run(&mut w, &e, &roots, debounce);
        })
        .ok()
}

/// Background thread (B4: Background class; its blocking read waits only for file events).
fn run(w: &mut Watcher, e: &Engine, roots: &[Root], debounce: Duration) {
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let mut new_dirs: Vec<PathBuf> = Vec::new();
        let mut overflow = false;
        let mut relevant = false;
        let mut first = true;
        loop {
            let evs = if first { w.ino.read_events_blocking(&mut buf) } else { w.ino.read_events(&mut buf) };
            let evs = match evs {
                Ok(evs) => evs,
                Err(x) if x.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(x) if x.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(x) => {
                    crate::event!("warn", "watch.failed", reason = x.kind().to_string(), fallback = "rescan");
                    return;
                }
            };
            for ev in evs {
                if ev.mask.contains(EventMask::Q_OVERFLOW) {
                    overflow = true;
                    continue;
                }
                if ev.mask.contains(EventMask::IGNORED) {
                    w.dirs.remove(&ev.wd);
                    continue;
                }
                let Some(dir) = w.dirs.get(&ev.wd) else { continue };
                if dir.starts_with(&w.skip) {
                    continue;
                }
                relevant = true;
                if ev.mask.contains(EventMask::ISDIR)
                    && ev.mask.intersects(EventMask::CREATE | EventMask::MOVED_TO)
                    && let Some(n) = ev.name
                {
                    new_dirs.push(dir.join(n));
                }
            }
            if first {
                // Collect what follows within the debounce time into this one wake-up.
                first = false;
                std::thread::sleep(debounce);
            }
        }
        for d in new_dirs {
            if let Some(r) = roots.iter().find(|r| d.starts_with(&r.path)) {
                w.add_tree(&d, r.dev);
            }
        }
        if overflow {
            crate::event!("warn", "watch.overflow", fallback = "rescan");
        }
        if relevant || overflow {
            e.wake_indexer();
        }
    }
}
