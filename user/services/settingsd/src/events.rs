//! B6 events of a user-space service: one line per state change, error, retry or fallback, written to
//! `/dev/kmsg` (the kernel turns each line into a klog event `user kmsg`, crates/vfs-sys/src/kmsg.rs; the `<n>`
//! prefix picks the level as Linux's devkmsg_emit does) and to stderr. Tests capture them instead.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub enum Level {
    Error = 3,
    Warn = 4,
    Info = 6,
}

#[derive(Clone, Default)]
pub struct Events {
    kmsg: Option<Arc<Mutex<File>>>,
    stderr: bool,
    capture: Option<Arc<Mutex<Vec<String>>>>,
}

impl Events {
    pub fn system() -> Events {
        let kmsg = OpenOptions::new().write(true).open("/dev/kmsg").ok().map(|f| Arc::new(Mutex::new(f)));
        Events { kmsg, stderr: true, capture: None }
    }

    pub fn capture() -> (Events, Arc<Mutex<Vec<String>>>) {
        let c = Arc::new(Mutex::new(Vec::new()));
        (Events { kmsg: None, stderr: false, capture: Some(c.clone()) }, c)
    }

    /// `name` like `set` or `fallback`; `fields` already `k=v` formatted (values that may hold spaces quoted
    /// by the caller through `q`).
    pub fn emit(&self, level: Level, name: &str, fields: &str) {
        let line = format!("settingsd {name} {fields}");
        let line: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        if let Some(c) = &self.capture
            && let Ok(mut v) = c.lock()
        {
            v.push(line.clone());
        }
        if self.stderr {
            eprintln!("{line}");
        }
        if let Some(k) = &self.kmsg
            && let Ok(mut f) = k.lock()
        {
            let _ = f.write_all(format!("<{}>{line}\n", level as u8).as_bytes());
        }
    }
}

/// A field value quoted when it holds spaces, `=` or quotes (event lines stay one `k=v` list).
pub fn q(s: &str) -> String {
    if s.is_empty() || s.contains([' ', '=', '"']) { format!("{s:?}") } else { s.to_string() }
}
