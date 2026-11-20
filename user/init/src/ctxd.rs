//! CX-5: the context service (`/bin/ctxd serve --system`) for the session's user, uid 1000 (it reads that
//! user's files with that user's rights, never as root), restarted with its own back-off like the other
//! services. Started once settingsd runs (its `context.*` keys), only as pid 1 on a real boot. It waits for
//! /data by itself, keeps its indexing in the Background class and its stderr goes to the kernel log.

use initlib::Backoff;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const EXE: &str = "/bin/ctxd";
const UID: u32 = 1000;

#[derive(Default)]
pub struct Ctxd {
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

impl Ctxd {
    /// Starts it when due. `path` is /bin/ctxd under init's root; `env` the session's environment.
    pub fn tick(&mut self, path: &str, run_dir: &str, home: &str, search_path: &str, mask: u64) -> Option<Say> {
        if self.child.is_some() || self.next.is_some_and(|n| Instant::now() < n) {
            return None;
        }
        if !usys::stat(path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.next = Some(Instant::now() + Duration::from_secs(60));
            return Some(Say::Repeat("noctxd", format!("no {path}: the context service is not started")));
        }
        let mut cmd = Command::new(path);
        usys::power::unblock_in_child(&mut cmd, mask);
        cmd.args(["serve", "--system"]).env_clear().env("PATH", search_path).env("HOME", home).env("XDG_RUNTIME_DIR", run_dir);
        cmd.uid(UID).gid(UID).current_dir("/");
        // Its few state lines (start, model, settings) in the kernel log; searches never go there.
        let err = std::fs::OpenOptions::new().write(true).open("/dev/kmsg").map_or_else(|_| Stdio::null(), Stdio::from);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(err);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.child = Some((pid, Instant::now()));
                Some(Say::Line(format!("started the context service {path} as uid {UID} (pid {pid})")))
            }
            Err(e) => {
                let delay = self.backoff.next(0);
                self.next = Some(Instant::now() + Duration::from_millis(delay));
                Some(Say::Repeat("ctxdspawn", format!("cannot start {path} ({e}); retrying in {} s", delay / 1000)))
            }
        }
    }

    /// The child `pid` ended (`what` as init words it): Some line when it was ctxd.
    pub fn ended(&mut self, pid: usize, what: &str) -> Option<String> {
        let (_, started) = self.child.take_if(|(p, _)| *p == pid)?;
        let ran = started.elapsed();
        let delay = self.backoff.next(ran.as_millis() as u64);
        self.next = Some(Instant::now() + Duration::from_millis(delay));
        Some(format!("ctxd (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_program_is_said_and_tried_again_later() {
        let mut c = Ctxd::default();
        assert!(matches!(c.tick("/nonexistent/ctxd", "/run/user/1000", "/data/home/1000", "/bin", 0), Some(Say::Repeat("noctxd", _))));
        assert!(c.tick("/nonexistent/ctxd", "/run/user/1000", "/data/home/1000", "/bin", 0).is_none(), "not again within the minute");
        assert!(c.child.is_none());
        assert!(c.ended(42, "exit 0").is_none(), "another child is not ctxd");
    }
}
