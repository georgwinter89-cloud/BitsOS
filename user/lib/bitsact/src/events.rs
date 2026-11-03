//! B6 events of an action service: one line per state change, error, retry, timeout or fallback, to `/dev/kmsg`
//! (the kernel makes each line a klog event `user kmsg`; `<n>` is the level as Linux's devkmsg_emit reads it) and
//! to stderr. Tests capture them instead.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Error = 3,
    Warn = 4,
    Info = 6,
}

#[derive(Clone, Default)]
pub struct Events {
    who: String,
    kmsg: Option<Arc<Mutex<File>>>,
    stderr: bool,
    capture: Option<Arc<Mutex<Vec<String>>>>,
}

impl Events {
    /// Events of the service `who` to /dev/kmsg (when it can be opened) and stderr.
    pub fn system(who: &str) -> Events {
        let kmsg = OpenOptions::new().write(true).open("/dev/kmsg").ok().map(|f| Arc::new(Mutex::new(f)));
        Events { who: who.to_string(), kmsg, stderr: true, capture: None }
    }

    pub fn capture(who: &str) -> (Events, Arc<Mutex<Vec<String>>>) {
        let c = Arc::new(Mutex::new(Vec::new()));
        (Events { who: who.to_string(), kmsg: None, stderr: false, capture: Some(c.clone()) }, c)
    }

    /// `fields` already `k=v` formatted (values with spaces through `q`).
    pub fn emit(&self, level: Level, name: &str, fields: &str) {
        let line = format!("{} {name} {fields}", self.who);
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

/// A field value, quoted when it holds spaces, `=` or quotes, and cut to 200 characters (an event stays one line).
pub fn q(s: &str) -> String {
    let s: String = s.chars().take(200).collect();
    if s.is_empty() || s.contains([' ', '=', '"']) { format!("{s:?}") } else { s }
}
