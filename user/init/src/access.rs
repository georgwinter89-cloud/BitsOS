//! DB2: the access log's service (`/bin/bitsaccess serve`) as root (it reads the kernel's root-only node and keeps
//! /data/system/access), restarted with its own back-off like the other services. Started once settingsd runs
//! (its `access.*` keys), only as pid 1 on a real boot. It waits for /data by itself; its few lines go to the
//! kernel log.

use initlib::Backoff;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const EXE: &str = "/bin/bitsaccess";

#[derive(Default)]
pub struct Access {
    /// (pid, started)
    child: Option<(usize, Instant)>,
    next: Option<Instant>,
    backoff: Backoff,
}

impl Access {
    /// Starts it when due: Some(line) for init's log, with a key for its de-duplication when it repeats.
    pub fn tick(&mut self, path: &str, search_path: &str, mask: u64) -> Option<(Option<&'static str>, String)> {
        if self.child.is_some() || self.next.is_some_and(|n| Instant::now() < n) {
            return None;
        }
        if !usys::stat(path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.next = Some(Instant::now() + Duration::from_secs(60));
            return Some((Some("nobitsaccess"), format!("no {path}: \"Wer hat was gemacht\" keeps nothing")));
        }
        let mut cmd = Command::new(path);
        usys::power::unblock_in_child(&mut cmd, mask);
        cmd.arg("serve").env_clear().env("PATH", search_path).env("HOME", "/root").current_dir("/");
        let err = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").map_or_else(|_| Stdio::null(), Stdio::from);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(err);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.child = Some((pid, Instant::now()));
                Some((None, format!("started the access log {path} (pid {pid})")))
            }
            Err(e) => {
                let delay = self.backoff.next(0);
                self.next = Some(Instant::now() + Duration::from_millis(delay));
                Some((Some("bitsaccessspawn"), format!("cannot start {path} ({e}); retrying in {} s", delay / 1000)))
            }
        }
    }

    /// The child `pid` ended (`what` as init words it): Some line when it was bitsaccess.
    pub fn ended(&mut self, pid: usize, what: &str) -> Option<String> {
        let (_, started) = self.child.take_if(|(p, _)| *p == pid)?;
        let ran = started.elapsed();
        let delay = self.backoff.next(ran.as_millis() as u64);
        self.next = Some(Instant::now() + Duration::from_millis(delay));
        Some(format!("bitsaccess (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_said_once_a_minute_and_other_children_are_not_it() {
        let mut a = Access::default();
        assert!(matches!(a.tick("/nonexistent/bitsaccess", "/bin", 0), Some((Some("nobitsaccess"), _))));
        assert!(a.tick("/nonexistent/bitsaccess", "/bin", 0).is_none(), "not again within the minute");
        assert!(a.ended(42, "exit 0").is_none());
    }
}
