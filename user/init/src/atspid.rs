//! DB5a: Durchblick's bridge to foreign programs (`/bin/atspid serve`). Started as root: it binds
//! /run/bitsos/atspi.sock, gives it to uid 1000 and drops to that user itself before it runs the accessibility
//! bus in /run/user/1000/at-spi. It waits by itself while Georg has it switched off (desktop.durchblick.programs)
//! and ends when he switches it off; restarted with its own back-off like the other services, once settingsd runs.

use initlib::Backoff;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const EXE: &str = "/bin/atspid";

#[derive(Default)]
pub struct Atspid {
    /// (pid, started)
    child: Option<(usize, Instant)>,
    next: Option<Instant>,
    backoff: Backoff,
}

/// What init should say: `Repeat(key, text)` goes through its de-duplication.
pub enum Say {
    Line(String),
    Repeat(&'static str, String),
}

impl Atspid {
    /// Starts it when due. `path` is /bin/atspid under init's root.
    pub fn tick(&mut self, path: &str, run_dir: &str, search_path: &str, mask: u64) -> Option<Say> {
        if self.child.is_some() || self.next.is_some_and(|n| Instant::now() < n) {
            return None;
        }
        if !usys::stat(path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.next = Some(Instant::now() + Duration::from_secs(60));
            return Some(Say::Repeat("noatspid", format!("no {path}: Durchblick sees foreign programs only as pictures")));
        }
        let mut cmd = Command::new(path);
        usys::power::unblock_in_child(&mut cmd, mask);
        cmd.arg("serve").env_clear().env("PATH", search_path).env("XDG_RUNTIME_DIR", run_dir).current_dir("/");
        let err = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").map_or_else(|_| Stdio::null(), Stdio::from);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(err);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.child = Some((pid, Instant::now()));
                Some(Say::Line(format!("started Durchblick's bridge {path} (pid {pid})")))
            }
            Err(e) => {
                let delay = self.backoff.next(0);
                self.next = Some(Instant::now() + Duration::from_millis(delay));
                Some(Say::Repeat("atspidspawn", format!("cannot start {path} ({e}); retrying in {} s", delay / 1000)))
            }
        }
    }

    /// The child `pid` ended (`what` as init words it): Some line when it was atspid.
    pub fn ended(&mut self, pid: usize, what: &str) -> Option<String> {
        let (_, started) = self.child.take_if(|(p, _)| *p == pid)?;
        let ran = started.elapsed();
        let delay = self.backoff.next(ran.as_millis() as u64);
        self.next = Some(Instant::now() + Duration::from_millis(delay));
        Some(format!("atspid (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_said_and_tried_again_later() {
        let mut a = Atspid::default();
        assert!(matches!(a.tick("/nonexistent/atspid", "/run/user/1000", "/bin", 0), Some(Say::Repeat("noatspid", _))));
        assert!(a.tick("/nonexistent/atspid", "/run/user/1000", "/bin", 0).is_none(), "not again within the minute");
        assert!(a.ended(42, "exit 0").is_none(), "another child is not atspid");
    }
}
