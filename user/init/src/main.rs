//! BitsOS V2 init (PID 1, item UL-2).
//!
//! Start: umask 022, mounts the Linux file systems the kernel did not mount already (/proc, /sys, /dev), makes
//! /tmp, /run, /dev/shm, /etc, /root, /run/user/1000 (user 1000, 0700) and /etc/passwd + /etc/group when missing. Then forever, once per interval: reads the kernel's DHCP view and keeps
//! /etc/hostname, /etc/hosts and /etc/resolv.conf in step with it (written as new files and renamed into place,
//! only when the content changes), reaps every ended child (orphans included once the kernel hands them to
//! PID 1), and keeps a console shell running when one exists (/bin/sh or /bin/busybox, item UL-3), restarted with
//! a back-off. Every failure is logged and retried; init never exits (`--once` is for tests only) and catches a
//! panic in a round instead of dying.
//!
//! Log lines start with "init: " and go to stdout (the console) and to /dev/kmsg when the kernel has one.
#![forbid(unsafe_code)]

use initlib::{Backoff, Card, Dedup, Opts};
use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::time::{Duration, Instant};

struct Shell {
    pid: usize,
    path: String,
    started: Instant,
}

struct Init {
    opts: Opts,
    kmsg: Option<usize>,
    dedup: Dedup,
    hostname: String,
    cards: Option<Vec<Card>>,
    shell: Option<Shell>,
    shell_next: Instant,
    shell_backoff: Backoff,
    /// CL-1: the settings registry service (`/bin/settingsd serve`), restarted with its own back-off.
    settingsd: Option<Shell>,
    settingsd_next: Instant,
    settingsd_backoff: Backoff,
    /// VS-2: the snapshot scheduler of /data (`/bin/bitssnap serve`), restarted with its own back-off.
    bitssnap: Option<Shell>,
    bitssnap_next: Instant,
    bitssnap_backoff: Backoff,
    /// RB-8: the journal of /data (`/bin/bitsjournal serve`), restarted with its own back-off.
    bitsjournal: Option<Shell>,
    bitsjournal_next: Instant,
    bitsjournal_backoff: Backoff,
    born: Instant,
    reaped: u64,
    /// `\BITSOS2\home\` on the stick exists (made or found); until then tried once per round.
    home_done: bool,
}

/// How long init waits for the boot stick before it starts settingsd anyway.
const STICK_WAIT: Duration = Duration::from_secs(15);

const DIRS: &[(&str, u32)] = &[
    ("/tmp", 0o1777),
    ("/run", 0o755),
    ("/dev/shm", 0o1777),
    ("/etc", 0o755),
    ("/root", 0o700),
    // UL-3b: where BusyBox's daemons and tools look (syslogd /var/log/messages, crond/crontab, mount's usual target).
    ("/usr", 0o755),
    ("/var/log", 0o755),
    ("/var/tmp", 0o1777),
    ("/var/spool/cron/crontabs", 0o700),
    ("/mnt", 0o755),
];
/// User 1000's runtime directory (`runtime_dir`), also the console shell's XDG_RUNTIME_DIR.
const RUNTIME_DIR: &str = "/run/user/1000";
/// User 1000's home on the stick (YT-2).
const STICK_HOME: &str = "/boot/BITSOS2/home";
const USER_UID: u32 = 1000;
/// Every program lives in /bin (UL-3a decision 3); Linux scripts name /usr/bin/env, /sbin/ip, /usr/sbin/...: these
/// links (relative, so they also hold under a test root) make the usual paths lead there. An existing path is left alone.
const LINKS: &[(&str, &str)] = &[("/usr/bin", "../bin"), ("/usr/sbin", "../bin"), ("/sbin", "bin"), ("/var/run", "../run")];
/// The console shell's PATH; it ends with the stick's program directory (UL-5: `\BITSOS2\bin`, FS-7 mounts the
/// stick at /boot), so programs too big for the initramfs run by name. The same as init's own (crates/userland
/// supervisor INIT_PATH, whose test compares them).
const SHELL_PATH: &str = "/bin:/sbin:/usr/bin:/boot/BITSOS2/bin";
const MOUNTS: &[(&str, &str)] = &[("/proc", "proc"), ("/sys", "sysfs"), ("/dev", "devtmpfs")];

impl Init {
    fn new(opts: Opts) -> Init {
        // O_WRONLY | O_CLOEXEC; the kernel may not have /dev/kmsg (BitsOS: change request), then stdout alone.
        let kmsg = if opts.root.is_empty() { usys::open("/dev/kmsg", 0o1 | 0o2000000, 0).ok() } else { None };
        Init {
            opts,
            kmsg,
            dedup: Dedup::default(),
            hostname: String::from("bitsos"),
            cards: None,
            shell: None,
            shell_next: Instant::now(),
            shell_backoff: Backoff::default(),
            settingsd: None,
            settingsd_next: Instant::now(),
            settingsd_backoff: Backoff::default(),
            bitssnap: None,
            bitssnap_next: Instant::now(),
            bitssnap_backoff: Backoff::default(),
            bitsjournal: None,
            bitsjournal_next: Instant::now(),
            bitsjournal_backoff: Backoff::default(),
            born: Instant::now(),
            reaped: 0,
            home_done: false,
        }
    }

    fn log(&mut self, msg: &str) {
        let line = format!("init: {msg}\n");
        write_all(1, line.as_bytes());
        if let Some(fd) = self.kmsg {
            write_all(fd, format!("<6>init: {msg}\n").as_bytes());
        }
    }

    /// A message that may repeat every round: shown the 1st, 10th, 100th, ... time.
    fn log_repeat(&mut self, key: &str, msg: &str) {
        if let Some(n) = self.dedup.hit(key) {
            if n == 1 { self.log(msg) } else { self.log(&format!("{msg} (seen {n} times)")) }
        }
    }

    fn start(&mut self) {
        let _ = usys::umask(0o022);
        let pid = usys::getpid().unwrap_or(0);
        let uid = usys::getuid().unwrap_or(u32::MAX as usize);
        let test = !self.opts.root.is_empty();
        self.log(&format!(
            "BitsOS init starting (pid {pid}, uid {uid}{})",
            if test { format!(", test root {}", self.opts.root) } else { String::new() }
        ));
        for u in self.opts.unknown.clone() {
            self.log(&format!("ignoring argument '{u}'"));
        }
        if !test && pid != 1 {
            self.log(&format!("running as pid {pid}, not 1: orphans reach init only once the kernel makes it PID 1"));
        }
        if !test && uid != 0 {
            self.log(&format!("running as uid {uid}, not root: writing outside writable folders will be refused"));
        }
        self.hostname = match usys::uname() {
            Ok(u) => initlib::clean_hostname(&u.nodename),
            Err(e) => {
                self.log(&format!("uname failed ({e}); host name bitsos"));
                "bitsos".to_string()
            }
        };
        self.mounts();
        self.dirs();
        self.runtime_dir();
        self.accounts();
        self.log(&format!("ready (host name {}, DHCP view {})", self.hostname, self.opts.net_file));
    }

    fn mounts(&mut self) {
        let root_dev = usys::stat(&self.opts.at("/")).map(|s| s.dev).ok();
        for &(path, fs) in MOUNTS {
            let target = self.opts.at(path);
            match usys::stat(&target) {
                Ok(st) if st.file_type() == "directory" && Some(st.dev) != root_dev => {
                    self.log(&format!("{path}: mounted already{}", fs_name(&target)));
                    continue;
                }
                _ => {}
            }
            if !self.opts.root.is_empty() {
                self.log(&format!("{path}: not a mount point; mount skipped under a test root"));
                continue;
            }
            if let Err(e) = mkdir_p(&target, 0o555) {
                self.log(&format!("{path}: cannot make the directory ({e})"));
                continue;
            }
            match usys::mount(fs, &target, fs, 0, None) {
                Ok(_) => self.log(&format!("{path}: mounted {fs}")),
                Err(e) => self.log(&format!("{path}: mount {fs} failed ({e}); continuing without it")),
            }
        }
    }

    /// /etc/passwd and /etc/group when missing; an existing file is left alone (someone may have edited it).
    fn accounts(&mut self) {
        for (path, text) in [("/etc/passwd", initlib::PASSWD), ("/etc/group", initlib::GROUP)] {
            let p = self.opts.at(path);
            if usys::stat(&p).is_ok() {
                continue;
            }
            match write_if_changed(&p, text) {
                Ok(_) => self.log(&format!("{path}: written (root, user 1000, nobody)")),
                Err(e) => self.log(&format!("cannot write {path} ({e})")),
            }
        }
    }

    /// WY-1a CR6: user 1000's XDG_RUNTIME_DIR (bitscomp's Wayland and control sockets), theirs alone (0700), as
    /// systemd-logind makes /run/user/<uid>. One that exists is left alone; a wrong owner or mode is said.
    fn runtime_dir(&mut self) {
        let p = self.opts.at(RUNTIME_DIR);
        match usys::stat(&p) {
            Ok(st) if st.file_type() == "directory" => {
                if st.uid != USER_UID || st.mode & 0o7777 != 0o700 {
                    self.log(&format!("{RUNTIME_DIR}: exists with owner {} mode {:o}; left alone", st.uid, st.mode & 0o7777));
                }
                return;
            }
            Ok(st) => {
                self.log(&format!("{RUNTIME_DIR}: exists as a {}, not a directory; left alone", st.file_type()));
                return;
            }
            Err(_) => {}
        }
        if let Err(e) = mkdir_p(&p, 0o700) {
            self.log(&format!("{RUNTIME_DIR}: cannot make it ({e})"));
            return;
        }
        // O_RDONLY | O_DIRECTORY | O_CLOEXEC: fchown on the directory just made, never through a symlink.
        let owned = usys::open(&p, 0o200000 | 0o2000000, 0).and_then(|fd| {
            let r = usys::fchown(fd, USER_UID, USER_UID);
            let _ = usys::close(fd);
            r
        });
        match owned {
            Ok(_) => self.log(&format!("{RUNTIME_DIR}: made (owner {USER_UID}, mode 700)")),
            Err(e) => self.log(&format!("{RUNTIME_DIR}: made, but cannot give it to user {USER_UID} ({e})")),
        }
    }

    fn dirs(&mut self) {
        let (mut made, mut failed) = (0, 0);
        for &(path, mode) in DIRS {
            let p = self.opts.at(path);
            match usys::stat(&p) {
                Ok(st) if st.file_type() == "directory" => {
                    // An older generation or the stick image may have made it otherwise; no chmod while
                    // chmod is ENOSYS (FS-1), and a mode someone set on purpose is theirs: say it, leave it.
                    if st.mode & 0o7777 != mode {
                        failed += 1;
                        self.log(&format!("{path}: exists with mode {:o} instead of {mode:o}; left alone", st.mode & 0o7777));
                    }
                    continue;
                }
                Ok(st) => {
                    failed += 1;
                    self.log(&format!("{path}: exists as a {}, not a directory; left alone", st.file_type()));
                    continue;
                }
                Err(_) => {}
            }
            match mkdir_p(&p, mode) {
                Ok(()) => {
                    made += 1;
                    let got = usys::stat(&p).map(|s| s.mode & 0o7777).unwrap_or(0);
                    if got == mode {
                        self.log(&format!("{path}: made (mode {mode:o})"));
                    } else {
                        self.log(&format!("{path}: made, mode {got:o} instead of {mode:o}"));
                    }
                }
                Err(e) => {
                    failed += 1;
                    self.log(&format!("{path}: cannot make it ({e})"));
                }
            }
        }
        for &(path, target) in LINKS {
            let p = self.opts.at(path);
            if usys::lstat(&p).is_ok() {
                continue;
            }
            made += 1;
            match usys::symlink(target, &p) {
                Ok(_) => self.log(&format!("{path}: link to {target}")),
                Err(e) => {
                    failed += 1;
                    self.log(&format!("{path}: cannot link it to {target} ({e})"));
                }
            }
        }
        if made + failed == 0 {
            self.log("directories: all present");
        }
    }

    /// One round: reap, /etc from the DHCP view, the shell.
    fn round(&mut self) {
        self.reap();
        self.network_files();
        self.settingsd();
        self.bitssnap();
        self.bitsjournal();
        self.stick_home();
        self.shell();
    }

    /// YT-2: user 1000's writable place on the stick (Cobalt's profile, bitscomp's session). fs-fat gives
    /// everything below `\BITSOS2\home\` to 1000:1000 (0700/0600), so root only has to make the directory once
    /// /boot is mounted read-write; while the stick is missing or read-only it is tried again each round.
    fn stick_home(&mut self) {
        if self.home_done || !usys::stat(&self.opts.at("/boot/BITSOS2")).is_ok_and(|s| s.file_type() == "directory") {
            return;
        }
        let p = self.opts.at(STICK_HOME);
        match usys::stat(&p) {
            Ok(st) if st.file_type() == "directory" => {
                self.home_done = true;
                return;
            }
            Ok(st) => {
                self.log(&format!("{STICK_HOME}: exists as a {}, not a directory; left alone", st.file_type()));
                self.home_done = true;
                return;
            }
            Err(_) => {}
        }
        match usys::mkdir(&p, 0o700) {
            Ok(_) | Err(usys::EEXIST) => {
                self.dedup.clear("stickhome");
                self.log(&format!("{STICK_HOME}: made (the stick gives it to user {USER_UID})"));
                self.home_done = true;
            }
            Err(e) => self.log_repeat("stickhome", &format!("{STICK_HOME}: cannot make it yet ({e}); trying again")),
        }
    }

    fn reap(&mut self) {
        loop {
            match usys::wait4(-1, usys::WNOHANG) {
                Ok((0, _)) | Err(usys::ECHILD) => break,
                Ok((pid, st)) => {
                    self.reaped += 1;
                    let what = usys::status_text(st);
                    if let Some(bs) = self.bitssnap.take_if(|bs| bs.pid == pid) {
                        let ran = bs.started.elapsed();
                        let delay = self.bitssnap_backoff.next(ran.as_millis() as u64);
                        self.bitssnap_next = Instant::now() + Duration::from_millis(delay);
                        self.log(&format!("bitssnap (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000));
                        continue;
                    }
                    if let Some(bj) = self.bitsjournal.take_if(|bj| bj.pid == pid) {
                        let ran = bj.started.elapsed();
                        let delay = self.bitsjournal_backoff.next(ran.as_millis() as u64);
                        self.bitsjournal_next = Instant::now() + Duration::from_millis(delay);
                        self.log(&format!("bitsjournal (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000));
                        continue;
                    }
                    if let Some(sd) = self.settingsd.take_if(|sd| sd.pid == pid) {
                        let ran = sd.started.elapsed();
                        let delay = self.settingsd_backoff.next(ran.as_millis() as u64);
                        self.settingsd_next = Instant::now() + Duration::from_millis(delay);
                        self.log(&format!("settingsd (pid {pid}) ended: {what} after {} s; next start in {} s", ran.as_secs(), delay / 1000));
                        continue;
                    }
                    match self.shell.take() {
                        Some(sh) if sh.pid == pid => {
                            let ran = sh.started.elapsed();
                            let delay = self.shell_backoff.next(ran.as_millis() as u64);
                            self.shell_next = Instant::now() + Duration::from_millis(delay);
                            self.log(&format!(
                                "shell {} (pid {pid}) ended: {what} after {} s; next start in {} s",
                                sh.path,
                                ran.as_secs(),
                                delay / 1000
                            ));
                        }
                        other => {
                            self.shell = other;
                            self.log(&format!("reaped pid {pid} ({what})"));
                        }
                    }
                }
                Err(usys::EINTR) => continue,
                Err(e) => {
                    self.log_repeat("wait4", &format!("wait4 failed ({e})"));
                    break;
                }
            }
        }
    }

    fn network_files(&mut self) {
        let cards = match std::fs::read_to_string(&self.opts.net_file) {
            Ok(t) => {
                self.dedup.clear("netfile");
                initlib::parse_net(&t)
            }
            Err(e) => {
                let f = self.opts.net_file.clone();
                self.log_repeat("netfile", &format!("no DHCP view ({f}: {e}); writing /etc without network data"));
                Vec::new()
            }
        };
        if self.cards.as_ref() != Some(&cards) {
            self.log(&format!("network: {}", initlib::describe(&cards)));
            self.cards = Some(cards.clone());
        }
        let files = [
            ("/etc/hostname", initlib::render_hostname(&self.hostname)),
            ("/etc/hosts", initlib::render_hosts(&cards, &self.hostname)),
            ("/etc/resolv.conf", initlib::render_resolv(&cards)),
        ];
        for (path, text) in files {
            let p = self.opts.at(path);
            match write_if_changed(&p, &text) {
                Ok(true) => {
                    self.dedup.clear(path);
                    let first = text.lines().find(|l| !l.starts_with('#')).unwrap_or("").to_string();
                    self.log(&format!("wrote {path} ({first})"));
                }
                Ok(false) => self.dedup.clear(path),
                Err(e) => self.log_repeat(path, &format!("cannot write {path} ({e})")),
            }
        }
    }

    /// VS-2: starts `/bin/bitssnap serve` (uid 0, no terminal; it logs to /dev/kmsg and waits by itself until
    /// /data is a writable pool) once settingsd was started, and restarts it with a back-off. Only as pid 1.
    fn bitssnap(&mut self) {
        if !self.opts.root.is_empty() || self.opts.once || std::process::id() != 1 || self.bitssnap.is_some() || self.settingsd.is_none() || Instant::now() < self.bitssnap_next {
            return;
        }
        let path = self.opts.at("/bin/bitssnap");
        if !usys::stat(&path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.log_repeat("nobitssnap", &format!("no {path}: /data gets no snapshots"));
            self.bitssnap_next = Instant::now() + Duration::from_secs(60);
            return;
        }
        self.dedup.clear("nobitssnap");
        let mut cmd = Command::new(&path);
        cmd.arg("serve").env_clear().env("PATH", SHELL_PATH).env("HOME", "/root").current_dir(self.opts.at("/"));
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.dedup.clear("bitssnapspawn");
                self.log(&format!("started the snapshot scheduler {path} (pid {pid})"));
                self.bitssnap = Some(Shell { pid, path, started: Instant::now() });
            }
            Err(e) => {
                let delay = self.bitssnap_backoff.next(0);
                self.bitssnap_next = Instant::now() + Duration::from_millis(delay);
                self.log_repeat("bitssnapspawn", &format!("cannot start {path} ({e}); retrying in {} s", delay / 1000));
            }
        }
    }

    /// RB-8: starts `/bin/bitsjournal serve` (uid 0, no terminal; it logs to /dev/kmsg and waits by itself until
    /// /data is writable) once settingsd was started, and restarts it with a back-off. Only as pid 1.
    fn bitsjournal(&mut self) {
        if !self.opts.root.is_empty() || self.opts.once || std::process::id() != 1 || self.bitsjournal.is_some() || self.settingsd.is_none() || Instant::now() < self.bitsjournal_next {
            return;
        }
        let path = self.opts.at("/bin/bitsjournal");
        if !usys::stat(&path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.log_repeat("nobitsjournal", &format!("no {path}: events are not kept beyond the kernel's ring"));
            self.bitsjournal_next = Instant::now() + Duration::from_secs(60);
            return;
        }
        self.dedup.clear("nobitsjournal");
        let mut cmd = Command::new(&path);
        cmd.arg("serve").env_clear().env("PATH", SHELL_PATH).env("HOME", "/root").current_dir(self.opts.at("/"));
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.dedup.clear("bitsjournalspawn");
                self.log(&format!("started the journal {path} (pid {pid})"));
                self.bitsjournal = Some(Shell { pid, path, started: Instant::now() });
            }
            Err(e) => {
                let delay = self.bitsjournal_backoff.next(0);
                self.bitsjournal_next = Instant::now() + Duration::from_millis(delay);
                self.log_repeat("bitsjournalspawn", &format!("cannot start {path} ({e}); retrying in {} s", delay / 1000));
            }
        }
    }

    /// CL-1/2: starts `/bin/settingsd serve` (uid 0, umask 022, no terminal; it logs to /dev/kmsg) and restarts it
    /// with a back-off. Only on a real boot as pid 1: a test root or a one-pass run never starts services.
    fn settingsd(&mut self) {
        // Only the system's init (pid 1): a test init in real mode (ktest ul.init.real_mode) must not leave a
        // second settingsd behind when it is killed.
        if !self.opts.root.is_empty() || self.opts.once || std::process::id() != 1 || self.settingsd.is_some() || Instant::now() < self.settingsd_next {
            return;
        }
        let path = self.opts.at("/bin/settingsd");
        if !usys::stat(&path).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0) {
            self.log_repeat("nosettingsd", &format!("no {path}: the settings registry is not started"));
            self.settingsd_next = Instant::now() + Duration::from_secs(60);
            return;
        }
        self.dedup.clear("nosettingsd");
        // Its values live on the stick: wait for /boot (the FAT stick mounts a moment after init starts) so the
        // first reads are the stick's, at most STICK_WAIT; without a stick it starts on its fallback.
        if !usys::stat(&self.opts.at("/boot/BITSOS2")).is_ok_and(|s| s.file_type() == "directory") && self.born.elapsed() < STICK_WAIT {
            self.settingsd_next = Instant::now() + Duration::from_millis(200);
            return;
        }
        let mut cmd = Command::new(&path);
        cmd.arg("serve").env_clear().env("PATH", SHELL_PATH).env("HOME", "/root").current_dir(self.opts.at("/"));
        if let Some(lang) = self.lang() {
            cmd.env("LANG", lang);
        }
        cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                drop(child);
                self.dedup.clear("settingsdspawn");
                self.log(&format!("started the settings registry {path} (pid {pid})"));
                self.settingsd = Some(Shell { pid, path, started: Instant::now() });
            }
            Err(e) => {
                let delay = self.settingsd_backoff.next(0);
                self.settingsd_next = Instant::now() + Duration::from_millis(delay);
                self.log_repeat("settingsdspawn", &format!("cannot start {path} ({e}); retrying in {} s", delay / 1000));
            }
        }
    }

    /// LANG for the programs init starts (UL-6, `initlib::pick_lang`), looked up at each start: a locale counts
    /// when its LC_CTYPE is a file (an empty or half-built directory would give a LANG glibc drops to ASCII).
    fn lang(&self) -> Option<&'static str> {
        initlib::pick_lang(|d| usys::stat(&self.opts.at(&format!("{d}/LC_CTYPE"))).is_ok_and(|s| s.file_type() == "regular"))
    }

    fn shell(&mut self) {
        if self.shell.is_some() || Instant::now() < self.shell_next {
            return;
        }
        let found = self.opts.shells.iter().map(|s| self.opts.at(s)).find(|p| usys::stat(p).is_ok_and(|s| s.file_type() == "regular" && s.mode & 0o111 != 0));
        let Some(path) = found else {
            let names = self.opts.shells.join(", ");
            self.log_repeat("noshell", &format!("no console shell ({names} missing; busybox: user/ports/busybox/build.sh): starting nothing"));
            self.shell_next = Instant::now() + Duration::from_secs(10);
            return;
        };
        self.dedup.clear("noshell");
        let mut cmd = Command::new(&path);
        if path.ends_with("/busybox") {
            cmd.arg("sh");
        }
        cmd.env_clear().env("PATH", SHELL_PATH).env("HOME", "/root").env("TERM", "linux").env("HOSTNAME", &self.hostname).env("XDG_RUNTIME_DIR", RUNTIME_DIR).current_dir(self.opts.at("/"));
        if let Some(lang) = self.lang() {
            cmd.env("LANG", lang);
        }
        // A session of its own with the console as controlling terminal, so ash turns job control on (Ctrl+C,
        // Ctrl+Z, fg/bg); taking the console from an older session is fine, init is its only giver.
        usys::session::console_session(&mut cmd, true);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as usize;
                // The handle is dropped without waiting: init's own wait4(-1) collects the shell.
                drop(child);
                self.dedup.clear("shellspawn");
                self.log(&format!("started the console shell {path} (pid {pid})"));
                self.shell = Some(Shell { pid, path, started: Instant::now() });
            }
            Err(e) => {
                let delay = self.shell_backoff.next(0);
                self.shell_next = Instant::now() + Duration::from_millis(delay);
                self.log_repeat("shellspawn", &format!("cannot start the console shell {path} ({e}); retrying in {} s", delay / 1000));
            }
        }
    }
}

/// The file system type of a mount point when statfs answers (else nothing).
fn fs_name(path: &str) -> String {
    match usys::statfs(path) {
        Ok((0x9fa0, _)) => " (proc)".to_string(),
        Ok((0x6265_6572, _)) => " (sysfs)".to_string(),
        Ok((0x0102_1994, _)) => " (tmpfs)".to_string(),
        Ok((t, _)) => format!(" (fs type {t:#x})"),
        Err(_) => String::new(),
    }
}

fn write_all(fd: usize, mut b: &[u8]) {
    let mut tries = 0;
    while !b.is_empty() && tries < 100 {
        match usys::write(fd, b) {
            Ok(0) => tries += 1,
            Ok(n) => b = &b[n.min(b.len())..],
            Err(usys::EINTR) | Err(usys::EAGAIN) => tries += 1,
            Err(_) => return,
        }
    }
}

/// mkdir with parents (parents 0755). The umask is 0 during init's own mkdir calls, so every directory gets its
/// mode exactly, sticky bit included (Linux keeps S_ISVTX in mkdir, linux-6.6/fs/namei.c:4113), without a chmod
/// (which a young kernel may not have).
fn mkdir_p(path: &str, mode: u32) -> Result<(), usys::Errno> {
    let old = usys::umask(0).unwrap_or(0o022);
    let r = mkdir_parents(path, mode);
    let _ = usys::umask(old);
    r
}

fn mkdir_parents(path: &str, mode: u32) -> Result<(), usys::Errno> {
    let mut cur = String::new();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        cur.push('/');
        cur.push_str(part);
        let last = i + 1 == parts.len();
        match usys::mkdir(&cur, if last { mode as usize & 0o7777 } else { 0o755 }) {
            Ok(_) | Err(usys::EEXIST) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Writes `text` to `path` unless it holds exactly that already: into `<path>.init-new.<pid>`, then renamed over
/// the old file (never an in-place update). The pid in the temporary name keeps two inits on the same tree (the
/// system's and a ktest's) from writing into each other's temporary file: each rename puts one whole file in
/// place. Ok(true) when written.
fn write_if_changed(path: &str, text: &str) -> std::io::Result<bool> {
    if std::fs::read(path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(false);
    }
    let tmp = format!("{path}.init-new.{}", std::process::id());
    std::fs::write(&tmp, text)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(true)
}

fn main() {
    let opts = Opts::parse(std::env::args().skip(1));
    let interval = Duration::from_millis(opts.interval_ms);
    let once = opts.once;
    let mut init = Init::new(opts);
    if std::panic::catch_unwind(AssertUnwindSafe(|| init.start())).is_err() {
        init.log("the start-up panicked; going on with the main loop");
    }
    let mut panics = 0u64;
    loop {
        if std::panic::catch_unwind(AssertUnwindSafe(|| init.round())).is_err() {
            panics += 1;
            init.log_repeat("panic", &format!("a round panicked ({panics} so far); going on"));
        }
        if once {
            let r = init.reaped;
            init.log(&format!("one round done (--once, reaped {r}); exiting as asked"));
            usys::exit_group(0);
        }
        if let Some(why) = pause(interval) {
            init.log_repeat("pause", &format!("nanosleep failed ({why}); pausing with poll/sched_yield instead"));
        }
    }
}

/// Sleeps `d` with the nanosleep syscall itself: Rust std's `thread::sleep` uses clock_nanosleep, which a young
/// kernel may not have (BitsOS answers ENOSYS until AB adds it, and std then panics). Never panics. The pause
/// always lasts `d` by the monotonic clock: EINTR sleeps the rest, and when nanosleep fails it waits with
/// `poll(no fds, ms)`, and if that fails too, with sched_yield until the time is up, so a broken sleep never
/// turns init's loop into a busy loop that floods the log. Returns why nanosleep failed (for the caller's log).
fn pause(d: Duration) -> Option<String> {
    let now = || usys::clock_gettime(usys::CLOCK_MONOTONIC).ok().map(|(s, n)| Duration::new(s.max(0) as u64, n.clamp(0, 999_999_999) as u32));
    let mut failed: Option<String> = None;
    let Some(start) = now() else {
        // No clock to measure the rest: one try per way, never a loop.
        if let Err(e) = usys::nanosleep(d.as_secs() as i64, d.subsec_nanos() as i64) {
            if e != usys::EINTR && usys::poll(&mut [], d.as_millis().clamp(1, i32::MAX as u128) as i32).is_err() {
                let _ = usys::sched_yield();
            }
            return Some(format!("{e}; no monotonic clock"));
        }
        return None;
    };
    let end = start.saturating_add(d);
    loop {
        let Some(t) = now() else { return failed };
        let left = end.saturating_sub(t);
        if left.is_zero() {
            return failed;
        }
        if failed.is_none() {
            match usys::nanosleep(left.as_secs() as i64, left.subsec_nanos() as i64) {
                Ok(_) | Err(usys::EINTR) => continue,
                Err(e) => failed = Some(format!("{e}")),
            }
        }
        match usys::poll(&mut [], left.as_millis().clamp(1, i32::MAX as u128) as i32) {
            Ok(_) | Err(usys::EINTR) => {}
            Err(_) => {
                let _ = usys::sched_yield();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_lasts_its_time() {
        let t = Instant::now();
        assert_eq!(pause(Duration::from_millis(30)), None);
        assert!(t.elapsed() >= Duration::from_millis(30));
        assert_eq!(pause(Duration::ZERO), None);
    }
}
