#![allow(dead_code)]

use settingsd::change::Who;
use settingsd::events::Events;
use settingsd::json::Json;
use settingsd::registry::Registry;
use settingsd::store::Paths;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Env {
    pub root: PathBuf,
    pub paths: Paths,
}

impl Env {
    /// A fresh tree under the target dir: `stick/` (= \BITSOS2\, made when `stick`), `fb/`, `sys/`.
    pub fn new(name: &str, stick: bool) -> Env {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let root = base.join(format!("cl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sys")).unwrap();
        if stick {
            std::fs::create_dir_all(root.join("stick")).unwrap();
        }
        let paths = Paths {
            stick: root.join("stick"),
            fallback: root.join("fb"),
            sys: root.join("sys"),
            socket: root.join("settings.sock"),
        };
        Env { root, paths }
    }

    pub fn open(&self) -> (Registry, Arc<Mutex<Vec<String>>>) {
        let (ev, cap) = Events::capture();
        (Registry::open(self.paths.clone(), ev, Duration::from_millis(500)), cap)
    }

    pub fn stick_file(&self, name: &str) -> String {
        std::fs::read_to_string(self.paths.stick.join(name)).unwrap_or_default()
    }
    pub fn fb_file(&self, name: &str) -> String {
        std::fs::read_to_string(self.paths.fallback.join(name)).unwrap_or_default()
    }
}

pub fn mkfifo(p: &std::path::Path) {
    assert!(std::process::Command::new("mkfifo").arg(p).status().unwrap().success());
}

/// Calls `f` until it is true or `secs` pass.
pub fn until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let end = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

pub const WHO: Who<'static> = Who { source: "test", confirmed: false };

pub fn s(v: &str) -> Json {
    Json::from(v)
}

pub fn value(r: &Registry, key: &str) -> Json {
    r.get(key, None).unwrap().get("value").cloned().unwrap()
}
