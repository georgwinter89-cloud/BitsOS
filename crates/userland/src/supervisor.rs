//! Starting and watching init.
//!
//! The kernel calls [`start_init`] once it is ready (after MCP). CONFIG `init=` picks the program (default
//! `/bin/init`, `init=off` starts nothing). A supervisor thread (class Normal: it waits for a user program, which
//! B4 allows only outside KernelRt/Interactive) spawns it with the console as stdin/stdout/stderr and waits for
//! its end. Linux panics when init dies ("Attempted to kill init!"); BitsOS keeps the kernel, MCP and the
//! console and restarts init after a back-off instead (1 s doubling to 60 s; a run of 60 s or more counts as
//! healthy and resets it). Five deaths within 10 minutes, or a missing program, end the restarts with an Error
//! event (`ul init.given_up` / `ul init.missing`): Claude sees it in `log`, Georg sees the kernel console.
//!
//! B6 events (subsystem `ul`): `init.off`, `init.start`, `init.died`, `init.restart`, `init.spawn_failed`,
//! `init.missing`, `init.given_up`, `init.bad_config`.

use alloc::string::String;
use alloc::vec::Vec;

const SEC: u64 = 1_000_000_000;

/// What CONFIG `init=` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitChoice {
    Off,
    Path(String),
    /// Not an absolute path: the default is used and the value logged.
    Bad(String),
}

pub const DEFAULT_INIT: &str = "/bin/init";

pub fn parse_init(v: Option<&str>) -> InitChoice {
    match v.map(str::trim) {
        None | Some("") => InitChoice::Path(String::from(DEFAULT_INIT)),
        Some("off" | "none" | "no" | "0") => InitChoice::Off,
        Some(p) if p.starts_with('/') && !p.contains(char::is_whitespace) => InitChoice::Path(String::from(p)),
        Some(other) => InitChoice::Bad(String::from(other)),
    }
}

/// The restart rule (numbers are policy, not resource limits; desk may make them CONFIG keys later).
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub first_delay_ns: u64,
    pub max_delay_ns: u64,
    /// A run at least this long resets the back-off and forgets earlier deaths.
    pub healthy_ns: u64,
    pub give_up_deaths: usize,
    pub window_ns: u64,
}

pub const POLICY: Policy = Policy { first_delay_ns: SEC, max_delay_ns: 60 * SEC, healthy_ns: 60 * SEC, give_up_deaths: 5, window_ns: 600 * SEC };

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    RestartIn(u64),
    GiveUp { deaths: usize },
}

/// Deaths so far and the next delay.
#[derive(Clone, Debug, Default)]
pub struct Restarts {
    delay_ns: u64,
    deaths: Vec<u64>,
}

impl Restarts {
    /// init ended at `now_ns` after running `ran_ns`.
    pub fn after_death(&mut self, p: &Policy, now_ns: u64, ran_ns: u64) -> Decision {
        if ran_ns >= p.healthy_ns || self.delay_ns == 0 {
            self.delay_ns = p.first_delay_ns;
        }
        if ran_ns >= p.healthy_ns {
            self.deaths.clear();
        }
        self.deaths.retain(|t| now_ns.saturating_sub(*t) < p.window_ns);
        self.deaths.push(now_ns);
        if self.deaths.len() >= p.give_up_deaths {
            return Decision::GiveUp { deaths: self.deaths.len() };
        }
        let d = self.delay_ns;
        self.delay_ns = self.delay_ns.saturating_mul(2).min(p.max_delay_ns);
        Decision::RestartIn(d)
    }
}

/// init's PATH, the Linux default plus the stick's program directory at the end (UL-5, `\BITSOS2\bin` at /boot):
/// the same as the console shell's (user/init SHELL_PATH; the test `same_path_as_the_shell` holds them together).
macro_rules! init_path {
    () => {
        "/bin:/sbin:/usr/bin:/boot/BITSOS2/bin"
    };
}
pub const INIT_PATH: &str = init_path!();

#[cfg(target_os = "none")]
pub use runtime::*;

#[cfg(target_os = "none")]
mod runtime {
    use super::*;
    use alloc::format;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};
    use ksync::SpinLock;
    use proc::{Process, SpawnError, SpawnOptions};

    static INIT: SpinLock<Option<Arc<Process>>> = SpinLock::named("ul.init", None);
    static STARTED: AtomicBool = AtomicBool::new(false);

    /// The running init process (for the kernel's orphan hand-over, `ps`, tests).
    pub fn init_process() -> Option<Arc<Process>> {
        INIT.lock().clone()
    }

    /// What one supervision ended with.
    #[derive(Clone, Debug, Default)]
    pub struct RunStats {
        pub starts: u32,
        pub gave_up: bool,
        pub missing: bool,
    }

    /// init's environment (PATH: `INIT_PATH`).
    pub const ENV: &[&str] = &["HOME=/", concat!("PATH=", init_path!()), "TERM=linux"];

    /// Starts init once (a second call is refused). Returns at once; the supervisor runs in its own thread.
    pub fn start_init(get: &dyn Fn(&str) -> Option<String>) -> Result<(), &'static str> {
        let path = match parse_init(get("init").as_deref()) {
            InitChoice::Off => {
                klog::event!(Info, "ul", "init.off"; "CONFIG init=off: no user program is started");
                return Ok(());
            }
            InitChoice::Path(p) => p,
            InitChoice::Bad(v) => {
                klog::event!(Warn, "ul", "init.bad_config", value = v.as_str(), using = DEFAULT_INIT; "init= wants an absolute path or off");
                String::from(DEFAULT_INIT)
            }
        };
        if STARTED.swap(true, Ordering::AcqRel) {
            return Err("init was started already");
        }
        let shown = path.clone();
        sched::spawn("init-supervisor", sched::Class::Normal, move || {
            let argv = [path.as_str()];
            let _ = supervise(&path, &argv, &POLICY, true);
        })
        .map(|_| ())
        .map_err(|_| {
            // No supervisor runs, so nothing was started: a later call may try again.
            STARTED.store(false, Ordering::Release);
            klog::event!(Error, "ul", "init.spawn_failed", path = shown.as_str(), error = "no supervisor thread"; "start_init may be called again");
            "no supervisor thread"
        })
    }

    /// Runs `path` and restarts it by `policy` until it gives up (or the program is missing). `publish` makes it
    /// the system's init (`init_process`).
    pub fn supervise(path: &str, argv: &[&str], policy: &Policy, publish: bool) -> RunStats {
        let mut st = RunStats::default();
        let mut restarts = Restarts::default();
        loop {
            let t0 = time::now_ns();
            let mut o = SpawnOptions::new(argv, ENV, policy::ProgramId::SYSTEM);
            o.class = sched::Class::Normal;
            // UL-2 change request 2 (INT-A1): the system's init is pid 1 with uid 0 (it writes /etc and reaps
            // orphans); test runs (`publish` false) stay ordinary processes.
            if publish {
                o.uid = 0;
                o.pid1 = true;
            }
            match proc::spawn_with(path, &o) {
                Ok(p) => {
                    st.starts += 1;
                    klog::event!(Info, "ul", "init.start", pid = p.pid, path = path, start = st.starts);
                    if publish {
                        *INIT.lock() = Some(p.clone());
                    }
                    let code = p.wait(None).unwrap_or(-1);
                    if publish {
                        *INIT.lock() = None;
                    }
                    let msg = p.end_message().unwrap_or_default();
                    klog::event!(Error, "ul", "init.died", pid = p.pid, path = path, code = code, ran_ms = time::now_ns().saturating_sub(t0) / 1_000_000, message = msg.as_str());
                }
                Err(SpawnError::NotFound) => {
                    st.missing = true;
                    klog::event!(Warn, "ul", "init.missing", path = path; "no init program in the file tree: nothing started");
                    return st;
                }
                Err(e) => {
                    klog::event!(Error, "ul", "init.spawn_failed", path = path, error = format!("{e}"));
                }
            }
            let now = time::now_ns();
            match restarts.after_death(policy, now, now.saturating_sub(t0)) {
                Decision::RestartIn(d) => {
                    klog::event!(Warn, "ul", "init.restart", path = path, in_ms = d / 1_000_000);
                    sched::sleep_ns(d);
                }
                Decision::GiveUp { deaths } => {
                    st.gave_up = true;
                    klog::event!(Error, "ul", "init.given_up", path = path, deaths = deaths, window_s = policy.window_ns / SEC; "init keeps dying: no more restarts (the kernel and MCP stay up)");
                    return st;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_as_the_shell() {
        let init = include_str!("../../../user/init/src/main.rs");
        assert!(init.contains(&alloc::format!("const SHELL_PATH: &str = \"{INIT_PATH}\";")), "user/init SHELL_PATH differs from INIT_PATH");
    }

    #[test]
    fn config_values() {
        assert_eq!(parse_init(None), InitChoice::Path("/bin/init".into()));
        assert_eq!(parse_init(Some(" ")), InitChoice::Path("/bin/init".into()));
        assert_eq!(parse_init(Some("off")), InitChoice::Off);
        assert_eq!(parse_init(Some("/sbin/init")), InitChoice::Path("/sbin/init".into()));
        assert_eq!(parse_init(Some("init")), InitChoice::Bad("init".into()));
        assert_eq!(parse_init(Some("/bin/x y")), InitChoice::Bad("/bin/x y".into()));
    }

    #[test]
    fn restart_policy() {
        let p = POLICY;
        let mut r = Restarts::default();
        let mut now = 1000 * SEC;
        let mut delays = Vec::new();
        for _ in 0..4 {
            match r.after_death(&p, now, SEC) {
                Decision::RestartIn(d) => {
                    delays.push(d / SEC);
                    now += d + SEC;
                }
                Decision::GiveUp { .. } => panic!("gave up too early"),
            }
        }
        assert_eq!(delays, vec![1, 2, 4, 8]);
        assert_eq!(r.after_death(&p, now, SEC), Decision::GiveUp { deaths: 5 });
        // A healthy run forgets the deaths and resets the delay.
        let mut r = Restarts::default();
        for _ in 0..3 {
            let _ = r.after_death(&p, now, SEC);
        }
        assert_eq!(r.after_death(&p, now + SEC, 120 * SEC), Decision::RestartIn(SEC));
        // Deaths further apart than the window never add up to a give-up.
        let mut r = Restarts::default();
        for i in 0..20 {
            assert!(matches!(r.after_death(&p, i * 200 * SEC, SEC), Decision::RestartIn(_)));
        }
        // The delay never passes the maximum.
        let mut r = Restarts::default();
        let q = Policy { give_up_deaths: usize::MAX, ..p };
        let mut last = 0;
        for i in 0..20 {
            if let Decision::RestartIn(d) = r.after_death(&q, i, 0) {
                last = d;
            }
        }
        assert_eq!(last, 60 * SEC);
    }
}
