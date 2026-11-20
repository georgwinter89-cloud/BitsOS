//! B6 events of the service: one line per state change, refusal or failure, on stderr (bitscomp starts this
//! mode and its stderr is the journal's: a program of uid 1000 cannot write /dev/kmsg itself) and to
//! /dev/kmsg where that opens. Never a word of the conversation, never a key. Tests capture them.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Arc, Mutex};

pub use settingsd::events::q;

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

    /// `fields` is a finished `k=v` list (values that may hold spaces quoted with [`q`]).
    pub fn emit(&self, level: Level, name: &str, fields: &str) {
        let line: String = format!("assist serve.{name} {fields}").chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
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
