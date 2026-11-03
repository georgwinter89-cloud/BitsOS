//! UL-7: init waits for busybox's halt/poweroff/reboot signals and the desktop's power socket between its rounds,
//! and ends the system as busybox init does (`run_shutdown_and_kill_processes`): SIGTERM to every process, a
//! bounded wait, SIGKILL, a bounded wait, sync, then reboot(2). A power-off is judged before anything ends
//! (the kernel's `/proc/bitsos/power`, R8f); refused, nothing happens but the log line.
//! Only the system's init does this (pid 1, no test root, not `--once`): a test init keeps the default signals.

use super::Init;
use initlib::power::{self, Action, Request};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant};

const POLLIN: i16 = 1;
/// How long one socket client that may ask (root, bitscomp) may take to send its line; others are not read.
const CLIENT_MS: i32 = 250;
const STEP: Duration = Duration::from_millis(50);

pub struct Waker {
    signals: Option<usize>,
    listener: Option<UnixListener>,
}

impl Waker {
    /// Blocks the power signals and opens their signalfd and the power socket (each failure logged; init then
    /// runs without that way in).
    pub fn new(init: &mut Init) -> Waker {
        let mask = power::signal_mask();
        let signals = match usys::rt_sigprocmask(usys::SIG_BLOCK, Some(mask), false, 0, 8)
            .and_then(|_| usys::power::signalfd(mask, usys::power::SFD_NONBLOCK | usys::power::SFD_CLOEXEC))
        {
            Ok(fd) => Some(fd),
            Err(e) => {
                let _ = usys::rt_sigprocmask(usys::SIG_UNBLOCK, Some(mask), false, 0, 8);
                init.log(&format!("no signalfd ({e}): halt, poweroff and reboot reach init only as default signals"));
                None
            }
        };
        let listener = listen(init);
        init.log(&format!(
            "power: signals {}, socket {}",
            if signals.is_some() { "TERM/INT restart, USR1/USR2 power off" } else { "off" },
            if listener.is_some() { power::POWER_SOCKET } else { "off" }
        ));
        Waker { signals, listener }
    }

    /// Waits up to `d` for a request; the first one that may go ahead is returned with its reason in words.
    pub fn wait(&mut self, init: &mut Init, d: Duration) -> Option<(Action, String)> {
        let end = Instant::now() + d;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let mut fds = Vec::new();
            if let Some(fd) = self.signals {
                fds.push(usys::PollFd { fd: fd as i32, events: POLLIN, revents: 0 });
            }
            if let Some(l) = &self.listener {
                fds.push(usys::PollFd { fd: l.as_raw_fd(), events: POLLIN, revents: 0 });
            }
            match usys::poll(&mut fds, left.as_millis().clamp(0, i32::MAX as u128) as i32) {
                Ok(_) | Err(usys::EINTR) => {}
                Err(e) => {
                    init.log_repeat("powerpoll", &format!("poll failed ({e}); sleeping instead"));
                    let _ = super::pause(left);
                }
            }
            if let Some(r) = self.take_signal(init) {
                return Some(r);
            }
            if let Some(r) = self.take_client(init) {
                return Some(r);
            }
            if Instant::now() >= end {
                return None;
            }
        }
    }

    fn take_signal(&mut self, init: &mut Init) -> Option<(Action, String)> {
        let fd = self.signals?;
        loop {
            let s = match usys::power::read_signal(fd) {
                Ok(Some(s)) => s,
                Ok(None) => return None,
                Err(e) => {
                    init.log_repeat("signalfd", &format!("signalfd read failed ({e})"));
                    return None;
                }
            };
            let Some(action) = power::action_of(s.signo) else { continue };
            let from = format!("signal {} from pid {} (uid {})", s.signo, s.pid, s.uid);
            if let Some(why) = power::signal_refusal(action, s.code, s.pid) {
                init.log(&format!("{} refused: {why} ({from})", action.word()));
                continue;
            }
            if action == Action::PowerOff
                && let Err(why) = judged(s.pid)
            {
                init.log(&format!("power off refused: {why} ({from})"));
                continue;
            }
            return Some((action, from));
        }
    }

    /// One client per call, so a stream of connections cannot keep init from its rounds (`wait` ends at its
    /// time whatever comes in).
    fn take_client(&mut self, init: &mut Init) -> Option<(Action, String)> {
        let l = self.listener.as_ref()?;
        let mut s = match l.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return None,
            Err(e) => {
                init.log_repeat("poweraccept", &format!("power socket accept failed ({e})"));
                // Not a busy loop while it keeps failing (no descriptors left, say).
                let _ = super::pause(STEP);
                return None;
            }
        };
        serve(init, &mut s)
    }
}

/// The kernel's judgement of a power-off `pid` asks for now (R8f: not from MCP's run, only after real input).
fn judged(pid: u32) -> Result<(), String> {
    match std::fs::read_to_string(power::POWER_FILE) {
        Ok(t) => power::verdict_for(&t, pid),
        Err(e) => Err(format!("cannot read {} ({e})", power::POWER_FILE)),
    }
}

fn listen(init: &mut Init) -> Option<UnixListener> {
    let path = power::POWER_SOCKET;
    let _ = std::fs::create_dir_all("/run/bitsos");
    let _ = std::fs::remove_file(path);
    // Mode 0777 so bitscomp (user 1000) can connect; who may ask is the peer check's business.
    let old = usys::umask(0).unwrap_or(0o022);
    let l = UnixListener::bind(path);
    let _ = usys::umask(old);
    match l.and_then(|l| l.set_nonblocking(true).map(|_| l)) {
        Ok(l) => Some(l),
        Err(e) => {
            init.log(&format!("cannot listen on {path} ({e}): the desktop cannot restart or power off"));
            None
        }
    }
}

/// One client: the peer check (a peer that may not ask is answered at once, its line never read), its line
/// within CLIENT_MS, the answer. Some(action) when it is to happen now.
fn serve(init: &mut Init, s: &mut UnixStream) -> Option<(Action, String)> {
    let fd = s.as_raw_fd();
    let (pid, uid, _) = match usys::power::peer_cred(fd as usize) {
        Ok(c) => c,
        Err(e) => {
            init.log(&format!("power socket: no peer credentials ({e}); refused"));
            return None;
        }
    };
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned());
    let from = format!("power socket, pid {pid} uid {uid} {}", exe.as_deref().unwrap_or("?"));
    let answer = |s: &mut UnixStream, text: &str| {
        // Non-blocking: a peer that does not read its answer cannot hold init up.
        let _ = s.set_nonblocking(true);
        let _ = s.write_all(text.as_bytes());
    };
    if let Err(why) = power::peer_allowed(uid, exe.as_deref()) {
        init.log_repeat("powerpeer", &format!("power socket refused: {why} ({from})"));
        answer(s, &format!("refused {why}\n"));
        return None;
    }
    let line = read_line(fd, s);
    match line.as_deref().and_then(power::parse_request) {
        Some(Request::Status) => {
            let t = std::fs::read_to_string(power::POWER_FILE).unwrap_or_default();
            answer(s, &format!("status {}\n", t.lines().next().unwrap_or("poweroff=unknown")));
            None
        }
        Some(Request::Do(a)) => {
            if a == Action::PowerOff
                && let Err(why) = judged(pid)
            {
                init.log(&format!("power off refused: {why} ({from})"));
                answer(s, &format!("refused {why}\n"));
                return None;
            }
            answer(s, &format!("ok {}\n", if a == Action::Restart { "restart" } else { "poweroff" }));
            Some((a, from))
        }
        None => {
            answer(s, "refused unknown request (restart, poweroff or status)\n");
            None
        }
    }
}

fn read_line(fd: i32, s: &mut UnixStream) -> Option<String> {
    let mut buf = Vec::new();
    let end = Instant::now() + Duration::from_millis(CLIENT_MS as u64);
    while buf.len() < power::REQUEST_MAX && !buf.contains(&b'\n') {
        let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
        let mut p = [usys::PollFd { fd, events: POLLIN, revents: 0 }];
        if left <= 0 || !matches!(usys::poll(&mut p, left), Ok(n) if n > 0) {
            break;
        }
        let mut b = [0u8; 64];
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&b[..n.min(power::REQUEST_MAX - buf.len())]),
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    (!text.trim().is_empty()).then_some(text)
}

/// Ends every program and the system. Returns never: a power-off that did not happen leaves init reaping
/// quietly (the drivers are stopped; nothing is restarted).
pub fn shut_down(init: &mut Init, action: Action, from: &str) -> ! {
    init.log(&format!("the system is going down for {} ({from})", action.word()));
    // An interactive shell ignores SIGTERM: the console shell gets the hangup a closing terminal sends.
    if let Some(sh) = &init.shell {
        let _ = usys::kill(sh.pid as isize, power::SIGHUP as usize);
    }
    end_all(init, power::SIGTERM, power::TERM_WAIT_MS);
    end_all(init, power::SIGKILL, power::KILL_WAIT_MS);
    match usys::power::sync() {
        Ok(_) => init.log("file systems written back (sync)"),
        Err(e) => init.log(&format!("sync failed ({e}); the kernel writes back once more")),
    }
    let cmd = if action == Action::Restart { usys::power::CMD_RESTART } else { usys::power::CMD_POWER_OFF };
    init.log(&format!("requesting system {}", action.word()));
    let e = usys::power::reboot(cmd).err();
    if action == Action::PowerOff && e == Some(usys::EPERM) {
        // The judgement changed while the programs ended (an injected key meanwhile): nothing runs any more,
        // so restart rather than stay without a shell.
        init.log("power off refused by the kernel after the programs ended (R8f); restarting instead");
        let e2 = usys::power::reboot(usys::power::CMD_RESTART).err();
        init.log(&format!("restart failed too ({e2:?})"));
    } else {
        let why = e.map_or(String::from("it returned"), |e| format!("{e}"));
        init.log(&format!("{} did not happen ({why}): hold the power button", action.word()));
    }
    loop {
        quiet_reap();
        let _ = super::pause(Duration::from_secs(60));
    }
}

/// Sends `sig` to every process but init (kill(-1)) and waits until none runs, at most `wait_ms`.
fn end_all(init: &mut Init, sig: u32, wait_ms: u64) {
    let name = if sig == power::SIGKILL { "SIGKILL" } else { "SIGTERM" };
    match usys::kill(-1, sig as usize) {
        Ok(_) => {}
        Err(usys::ESRCH) => return,
        Err(e) => init.log(&format!("kill(-1, {name}) failed ({e})")),
    }
    if sig == power::SIGTERM {
        // A stopped program acts on SIGTERM only once it runs again (systemd sends SIGCONT after it, too).
        let _ = usys::kill(-1, power::SIGCONT as usize);
    }
    let t0 = Instant::now();
    let end = t0 + Duration::from_millis(wait_ms);
    loop {
        quiet_reap();
        let left = others_running();
        if left == 0 {
            init.log(&format!("sent {name} to all processes: all ended after {} ms", t0.elapsed().as_millis()));
            return;
        }
        if Instant::now() >= end {
            init.log(&format!("sent {name} to all processes: {left} still running after {wait_ms} ms"));
            return;
        }
        let _ = super::pause(STEP);
    }
}

fn quiet_reap() {
    while let Ok((pid, _)) = usys::wait4(-1, usys::WNOHANG) {
        if pid == 0 {
            break;
        }
    }
}

/// Processes other than init that still run (zombies do not count); kill(-1, 0) when /proc cannot be read.
fn others_running() -> usize {
    let me = std::process::id().to_string();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return usize::from(usys::kill(-1, 0).is_ok());
    };
    rd.flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != &me && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .filter(|n| std::fs::read_to_string(format!("/proc/{n}/stat")).ok().and_then(|t| power::stat_state(&t)).is_some_and(power::running))
        .count()
}

/// `init --power <restart|poweroff|status>`: one request over the power socket, the answer on stdout (the
/// reference client for the desktop's power menu, DS-6). Exit 0 for "ok" and "status" answers, 1 otherwise.
pub fn client(request: &str) -> i32 {
    let mut s = match UnixStream::connect(power::POWER_SOCKET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("init: cannot reach {} ({e})", power::POWER_SOCKET);
            return 1;
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    if let Err(e) = s.write_all(format!("{request}\n").as_bytes()) {
        eprintln!("init: cannot send the request ({e})");
        return 1;
    }
    let mut answer = String::new();
    let _ = s.take(power::REQUEST_MAX as u64 * 8).read_to_string(&mut answer);
    print!("{answer}");
    i32::from(!(answer.starts_with("ok ") || answer.starts_with("status ")))
}
