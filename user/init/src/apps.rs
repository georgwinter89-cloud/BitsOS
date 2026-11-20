//! PK-0: `/apps` on the data volume and `bitspkg register` at boot (plan/spec/program-library.md section 3).
//!
//! Each round until decided: once the kernel's /data thread says /data is mounted writable
//! (/run/bitsos/data-state), init makes /data/apps (root, 0755) and bind-mounts it on /apps; a /data that is
//! read-only, skipped, failed, or still not there after `APPS_WAIT` leaves /apps the initramfs tmpfs for this boot
//! (a volume that comes later is not bound under running programs). /run/bitsos/apps-state says which, in one
//! line (initlib::apps_state). Then, once settingsd listens, `/bin/bitspkg register` gives it the installed
//! packages' settings keys again, and again after every restart of settingsd (it forgets them).
//!
//! The pure parts (the lines, which volume) are initlib::apps_state; here are the system calls.

use crate::{Init, SHELL_PATH, Shell, mkdir_p, write_if_changed};
use initlib::apps_state::{self as st, Data};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long init waits for the data volume's word. The kernel's own waits are bounded (60 s for the NVMe window,
/// 120 s for the stick, then making the image); after this /apps stays tmpfs for the boot.
const APPS_WAIT: Duration = Duration::from_secs(300);
/// Under a test root only: APPS_WAIT in seconds, so a test reaches "still waiting" without waiting.
const APPS_WAIT_ENV: &str = "INIT_TEST_APPS_WAIT_S";
/// After the bind: how long the window path gets to show the stick's image at /stickdata (it does so right after
/// it mounted /data), before an armed window with an unshown data.img counts as /data on data.img.
const SETTLE: Duration = Duration::from_secs(10);
/// MS_BIND (linux/fs.h; crates/vfs sys_mount.rs).
const MS_BIND: usize = 4096;
const BITSPKG: &str = "/bin/bitspkg";
const SETTINGS_SOCK: &str = "/run/bitsos/settings.sock";

pub(crate) enum Apps {
    Waiting,
    /// Bound at that time; the volume's name is still being made out.
    Bound(Instant),
    Done,
}

impl Init {
    /// Before settingsd in every round; a no-op once decided.
    pub(crate) fn apps(&mut self) {
        match self.apps {
            Apps::Done => {}
            Apps::Bound(at) => self.apps_volume(at),
            Apps::Waiting => self.apps_decide(),
        }
    }

    /// The final line (data: or tmpfs:) is written. Not yet while the volume is still being made out after the
    /// bind: bitspkg refuses while the line says `waiting`, so register would only fail and back off.
    pub(crate) fn apps_decided(&self) -> bool {
        matches!(self.apps, Apps::Done)
    }

    fn apps_wait(&self) -> Duration {
        let test = (!self.opts.root.is_empty()).then(|| std::env::var(APPS_WAIT_ENV).ok()).flatten();
        test.and_then(|v| v.parse().ok()).map_or(APPS_WAIT, Duration::from_secs)
    }

    fn apps_decide(&mut self) {
        let text = std::fs::read_to_string(self.opts.at(st::DATA_STATE)).ok();
        let wait = self.apps_wait();
        match st::data(text.as_deref()) {
            Data::Pending(phase) if self.born.elapsed() < wait => self.apps_line(&st::line_waiting(&phase)),
            Data::Pending(phase) => self.apps_tmpfs(&format!("data volume still {phase} after {} s", wait.as_secs())),
            Data::Refused(why) => self.apps_tmpfs(&why),
            Data::Writable => match self.apps_bind() {
                Ok(()) => {
                    let now = Instant::now();
                    self.apps = Apps::Bound(now);
                    self.apps_volume(now);
                }
                Err(why) => self.apps_tmpfs(&why),
            },
        }
    }

    /// /data/apps (root's directory, made when missing) on /apps. Err: why /apps stays tmpfs.
    fn apps_bind(&mut self) -> Result<(), String> {
        let test = !self.opts.root.is_empty();
        let (src, dst) = (self.opts.at(st::DATA_APPS), self.opts.at(st::APPS));
        // lstat: a link put there must not carry /apps elsewhere; under a test root everything is the tester's.
        match usys::lstat(&src) {
            Ok(s) if s.file_type() == "directory" && (s.uid == 0 || test) => {
                // B3: root runs what lies below /apps; a group- or world-writable /data/apps would let others
                // plant files there. Only root can have changed it, so it is set back rather than refused.
                let mode = s.mode & 0o7777;
                if mode != 0o755 {
                    usys::chmod(&src, 0o755).map_err(|e| format!("{} has mode {mode:o} and cannot be set to 755 ({e})", st::DATA_APPS))?;
                    self.log(&format!("{}: mode {mode:o} set back to 755", st::DATA_APPS));
                }
            }
            Ok(s) if s.file_type() == "directory" => return Err(format!("{} belongs to uid {}, not root", st::DATA_APPS, s.uid)),
            Ok(s) => return Err(format!("{} is a {}, not a directory", st::DATA_APPS, s.file_type())),
            Err(_) => {
                mkdir_p(&src, 0o755).map_err(|e| format!("cannot make {} ({e})", st::DATA_APPS))?;
                self.log(&format!("{}: made (mode 755)", st::DATA_APPS));
            }
        }
        match usys::lstat(&dst) {
            Ok(s) if s.file_type() == "directory" => {}
            Ok(s) => return Err(format!("{} is a {}, not a directory", st::APPS, s.file_type())),
            Err(_) => mkdir_p(&dst, 0o755).map_err(|e| format!("cannot make {} ({e})", st::APPS))?,
        }
        if test {
            self.log(&format!("{}: bind mount of {} skipped under a test root", st::APPS, st::DATA_APPS));
            return Ok(());
        }
        if mounted(&dst) {
            self.log(&format!("{}: a mount point already; left as it is", st::APPS));
            return Ok(());
        }
        usys::mount(&src, &dst, "none", MS_BIND, None).map_err(|e| format!("bind of {} on {} failed ({e})", st::DATA_APPS, st::APPS))?;
        self.log(&format!("{}: bound to {} (programs stay installed across boots)", st::APPS, st::DATA_APPS));
        Ok(())
    }

    fn apps_volume(&mut self, bound: Instant) {
        let window = std::fs::read_to_string(self.opts.at(st::DATA_WINDOW)).ok();
        let image = usys::stat(&self.opts.at(st::STICK_IMAGE)).ok().filter(|s| s.file_type() == "regular").map(|s| s.size.max(0) as u64);
        let shown = mounted(&self.opts.at(st::STICK_DATA));
        match st::volume(window.as_deref(), image, shown, bound.elapsed() >= SETTLE) {
            Some(v) => {
                self.apps_line(&st::line_data(&v));
                self.apps = Apps::Done;
            }
            None => self.apps_line(&st::line_waiting("being identified")),
        }
    }

    fn apps_tmpfs(&mut self, why: &str) {
        self.log(&format!("{}: stays the initramfs tmpfs ({why}); programs installed now are gone at the next boot", st::APPS));
        self.apps_line(&st::line_tmpfs(why));
        self.apps = Apps::Done;
    }

    fn apps_line(&mut self, line: &str) {
        let p = self.opts.at(st::STATE_FILE);
        if let Some(dir) = std::path::Path::new(&p).parent().and_then(|d| d.to_str()) {
            let _ = mkdir_p(dir, 0o755);
        }
        match write_if_changed(&p, line) {
            Ok(true) => {
                self.dedup.clear("appsstate");
                self.log(&format!("{}: {}", st::STATE_FILE, line.trim_end()));
            }
            Ok(false) => {}
            Err(e) => self.log_repeat("appsstate", &format!("cannot write {} ({e})", st::STATE_FILE)),
        }
    }

    /// Starts `/bin/bitspkg register` (uid 0; its lines go to /dev/kmsg) once /apps is decided and settingsd's
    /// socket is there; again after settingsd started anew. It ends by itself: its lock wait and every settingsd
    /// answer are bounded (store.rs LOCK_WAIT, settings.rs TIMEOUT). Only as pid 1.
    pub(crate) fn pkg_register(&mut self) {
        if !self.opts.root.is_empty() || self.opts.once || std::process::id() != 1 || self.register_done || self.register.is_some() {
            return;
        }
        if !self.apps_decided() || self.settingsd.is_none() || Instant::now() < self.register_next {
            return;
        }
        if !usys::stat(SETTINGS_SOCK).is_ok_and(|s| s.file_type() == "socket") {
            self.register_next = Instant::now() + Duration::from_millis(200);
            return;
        }
        if !usys::stat(BITSPKG).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.log(&format!("no {BITSPKG}: installed programs' settings are not registered again"));
            self.register_done = true;
            return;
        }
        let mut cmd = Command::new(BITSPKG);
        usys::power::unblock_in_child(&mut cmd, initlib::power::signal_mask());
        cmd.arg("register").env_clear().env("PATH", SHELL_PATH).env("HOME", "/root").current_dir("/");
        let kmsg = || std::fs::OpenOptions::new().write(true).open("/dev/kmsg").map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        cmd.stdin(Stdio::null()).stdout(kmsg()).stderr(kmsg());
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.log(&format!("started {BITSPKG} register (pid {pid}): installed programs' settings for settingsd"));
                self.register = Some(Shell { pid, path: BITSPKG.into(), started: Instant::now() });
            }
            Err(e) => {
                let delay = self.register_backoff.next(0);
                self.register_next = Instant::now() + Duration::from_millis(delay);
                self.log_repeat("registerspawn", &format!("cannot start {BITSPKG} register ({e}); retrying in {} s", delay / 1000));
            }
        }
    }

    /// From `reap`: true when `pid` was the register run.
    pub(crate) fn register_ended(&mut self, pid: usize, status: i32) -> bool {
        let Some(r) = self.register.take_if(|r| r.pid == pid) else { return false };
        let what = usys::status_text(status);
        // A settingsd that started anew during the run lost what it registered: run once more.
        let fresh = self.settingsd.as_ref().is_some_and(|s| s.started <= r.started);
        if status == 0 && !fresh {
            self.log(&format!("{BITSPKG} register (pid {pid}) done, but settingsd started anew meanwhile; once more"));
        } else if status == 0 {
            self.register_done = true;
            self.dedup.clear("registerfail");
            self.log(&format!("{BITSPKG} register (pid {pid}) done after {} ms", r.started.elapsed().as_millis()));
        } else {
            let delay = self.register_backoff.next(r.started.elapsed().as_millis() as u64);
            self.register_next = Instant::now() + Duration::from_millis(delay);
            self.log_repeat("registerfail", &format!("{BITSPKG} register (pid {pid}) ended: {what}; again in {} s (its lines are in the kernel log)", delay / 1000));
        }
        true
    }

    /// settingsd (re)started: it knows no package key until register runs again.
    pub(crate) fn register_again(&mut self) {
        self.register_done = false;
    }
}

/// `path` is a mount point: another device than its parent's.
fn mounted(path: &str) -> bool {
    let parent = std::path::Path::new(path).parent().and_then(|p| p.to_str()).filter(|p| !p.is_empty()).unwrap_or("/");
    match (usys::stat(path), usys::stat(parent)) {
        (Ok(a), Ok(b)) => a.file_type() == "directory" && a.dev != b.dev,
        _ => false,
    }
}
