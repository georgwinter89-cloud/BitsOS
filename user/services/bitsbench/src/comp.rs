//! bitscomp's control socket (`$XDG_RUNTIME_DIR/<wayland socket>.ctl`, one command line, one answer): whether
//! anybody uses the desktop (`idle`, SELFOPT-1), the frame counters (`stats`), a paced frame bench (`bench`) and
//! the window list (`list`). bitscomp starts bitsbench with WAYLAND_DISPLAY and XDG_RUNTIME_DIR set.

use crate::rules::Desk;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// An answer longer than this is cut (R1: per input; `list` of many windows stays far below).
const MAX_ANSWER: u64 = 1 << 20;

#[derive(Clone, Debug)]
pub struct Comp {
    pub path: PathBuf,
}

impl Comp {
    /// The control socket by bitscomp's own rule (rundir.rs): XDG_RUNTIME_DIR, else /run/user/<uid>.
    pub fn from_env() -> Comp {
        let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
            let uid = usys::getuid().unwrap_or(1000);
            PathBuf::from(format!("/run/user/{uid}"))
        });
        let sock = std::env::var("WAYLAND_DISPLAY").ok().filter(|s| !s.is_empty() && !s.contains('/')).unwrap_or_else(|| "wayland-bits-0".into());
        Comp { path: dir.join(format!("{sock}.ctl")) }
    }

    pub fn send(&self, cmd: &str, wait: Duration) -> Result<String, String> {
        let mut s = UnixStream::connect(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        s.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
        s.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
        let mut out = String::new();
        s.take(MAX_ANSWER).read_to_string(&mut out).map_err(|e| format!("{cmd}: {e}"))?;
        Ok(out)
    }

    pub fn idle(&self) -> Result<Desk, String> {
        let a = self.send("idle", Duration::from_secs(5))?;
        parse_idle(&a).ok_or_else(|| format!("idle: unexpected answer {:?}", a.trim()))
    }

    /// `stats` as key=value pairs (numbers only).
    pub fn stats(&self, reset: bool) -> Result<Vec<(String, f64)>, String> {
        let a = self.send(if reset { "stats reset" } else { "stats" }, Duration::from_secs(5))?;
        Ok(pairs(&a))
    }

    pub fn windows(&self) -> Result<String, String> {
        self.send("list", Duration::from_secs(5))
    }
}

/// `idle input_s=N inputs=N fullscreen=0|1 games=N offset_min=N`.
pub fn parse_idle(a: &str) -> Option<Desk> {
    let line = a.lines().find(|l| l.starts_with("idle "))?;
    let p = pairs(line);
    let get = |k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    Some(Desk {
        idle_s: get("input_s")? as u64,
        inputs: get("inputs")? as u64,
        fullscreen: get("fullscreen")? != 0.0,
        games: get("games")? as u32,
        offset_min: get("offset_min").unwrap_or(0.0) as i32,
    })
}

pub fn pairs(a: &str) -> Vec<(String, f64)> {
    a.split_whitespace().filter_map(|w| w.split_once('=')).filter_map(|(k, v)| v.parse::<f64>().ok().map(|v| (k.to_string(), v))).collect()
}

pub fn get(p: &[(String, f64)], k: &str) -> Option<f64> {
    p.iter().find(|(n, _)| n == k).map(|(_, v)| *v)
}

/// The ids of the windows in a `list` answer whose app id `app` accepts (`... id=N app_id=X title=...`).
pub fn window_ids(list: &str, app: &dyn Fn(&str) -> bool) -> Vec<u64> {
    list.lines()
        .filter(|l| app_of(l).is_some_and(app))
        .filter_map(|l| l.split_whitespace().find_map(|w| w.strip_prefix("id=")).and_then(|v| v.parse().ok()))
        .collect()
}

fn app_of(line: &str) -> Option<&str> {
    line.split_whitespace().find_map(|w| w.strip_prefix("app_id="))
}
