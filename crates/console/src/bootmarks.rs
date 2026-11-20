//! GS-1: the times of the start's graphics stages, kept beside the step list (`Steps::marks`). The same lines feed
//! them (also through IN-9's watch, so they exist at every log level); `/proc/bitsos/boot` shows them as
//! `mark <name> <ms>` lines, which bitscomp adds to its own when it says `boot.desktop_ready`. Milliseconds since
//! boot on the log's clock (CLOCK_MONOTONIC in user space). Each mark is the first of its kind: a boot has one.

use alloc::format;
use alloc::string::String;

/// The kernel's stages, in the order of the start.
pub const NAMES: [&str; 4] = ["gfx_start", "engines", "node", "desktop_start"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Marks {
    /// By `NAMES`; 0 = not reached.
    ms: [u64; NAMES.len()],
}

impl Marks {
    pub const fn new() -> Marks {
        Marks { ms: [0; NAMES.len()] }
    }

    /// Which mark an event sets, if any.
    pub fn of(subsystem: &str, name: &str) -> Option<usize> {
        let n = match (subsystem, name) {
            ("gx", "boot.start") => "gfx_start",
            ("gx", "boot.engines_up") => "engines",
            ("gx", "boot.up") => "node",
            ("console", "desktop.start") => "desktop_start",
            _ => return None,
        };
        NAMES.iter().position(|m| *m == n)
    }

    /// One log line's event at `t_ms`; a mark already set stays.
    pub fn note(&mut self, subsystem: &str, name: &str, t_ms: u64) {
        if let Some(i) = Self::of(subsystem, name)
            && self.ms[i] == 0
        {
            // A line at 0 ms would read as "not reached": the first millisecond counts as the second.
            self.ms[i] = t_ms.max(1);
        }
    }

    pub fn get(&self, name: &str) -> Option<u64> {
        NAMES.iter().position(|m| *m == name).map(|i| self.ms[i]).filter(|&t| t != 0)
    }

    /// The `mark` lines of `/proc/bitsos/boot`, only the reached ones.
    pub fn proc_lines(&self) -> String {
        let mut s = String::new();
        for (n, t) in NAMES.iter().zip(self.ms).filter(|(_, t)| *t != 0) {
            s.push_str(&format!("mark {n} {t}\n"));
        }
        s
    }

    /// One `mark <name> <ms>` line's words after `mark`.
    pub fn parse_line(&mut self, name: &str, ms: &str) {
        if let (Some(i), Ok(t)) = (NAMES.iter().position(|m| *m == name), ms.trim().parse::<u64>()) {
            self.ms[i] = t;
        }
    }
}
