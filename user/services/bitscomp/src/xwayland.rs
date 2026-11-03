//! WY-3: Xwayland on demand. bitscomp owns the X display's sockets (/tmp/.X11-unix/X<n>, its abstract twin and the
//! lock /tmp/.X<n>-lock); the first X client's connection starts Xwayland (rootless) with those sockets
//! (`-listenfd`), a Wayland connection of its own (WAYLAND_SOCKET), the window manager's connection (`-wm`, `xwm`)
//! and `-displayfd` to say when it is ready. Programs bitscomp starts get DISPLAY=:<n>. When Xwayland ends, its
//! windows go and the sockets wait for the next client; three ends within 2 s of their starts in a row close the
//! display until bitscomp restarts (B6: every step is a line).

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use smithay::reexports::wayland_server::backend::DisconnectReason;
use smithay::wayland::selection::{SelectionSource, SelectionTarget};

use crate::ClientState;
use crate::state::App;
use crate::xwm::Xwm;

/// Where the stick's Xwayland tree puts it (user/ports/xwayland); `--xwayland PATH|none` overrides.
pub const DEFAULT_EXE: &str = "/boot/BITSOS2/xwayland/bin/Xwayland";
const QUICK_END: Duration = Duration::from_secs(2);
const QUICK_ENDS_MAX: u32 = 3;
/// How long Xwayland may take from its start to serving the display (displayfd written and WM_S0 taken).
const READY_DEADLINE: Duration = Duration::from_secs(10);

pub struct Xwayland {
    pub display: u32,
    exe: PathBuf,
    listeners: Vec<UnixListener>,
    lock: PathBuf,
    socket: PathBuf,
    tokens: Vec<RegistrationToken>,
    handle: LoopHandle<'static, App>,
    child: Option<std::process::Child>,
    started: Option<Instant>,
    ready_ms: Option<u128>,
    pub wm: Option<Xwm>,
    sources: Vec<RegistrationToken>,
    quick_ends: u32,
    pub starts: u32,
    failed: bool,
    /// The start that missed READY_DEADLINE (set by its timer, ended by `turn`).
    stuck: bool,
}

/// The restart budget after one end of Xwayland: the new count of quick ends in a row, and whether the display
/// goes off (QUICK_ENDS_MAX quick ends in a row). An end that is not quick starts the count again.
fn budget(quick_ends: u32, quick: bool) -> (u32, bool) {
    let n = if quick { quick_ends.saturating_add(1) } else { 0 };
    (n, n >= QUICK_ENDS_MAX)
}

/// The display's sockets and lock, or None (no Xwayland at `exe`, or no free display).
pub fn setup(handle: &LoopHandle<'static, App>, exe: &str) -> Option<Xwayland> {
    if exe == "none" {
        return None;
    }
    if !crate::spawn::executable(Path::new(exe)) {
        eprintln!("bitscomp: xwayland: {exe} is not there: no X11 display (X11 programs cannot start)");
        return None;
    }
    let dir = Path::new("/tmp/.X11-unix");
    if std::fs::create_dir(dir).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777));
    }
    // The first free display number; a display whose lock names a live process belongs to someone else.
    let mut n = 0;
    let (display, listeners, lock, socket) = loop {
        if n > 1024 {
            eprintln!("bitscomp: xwayland: no free X display found in :0..:1024");
            return None;
        }
        match claim(n) {
            Ok(Some(v)) => break v,
            Ok(None) => n += 1,
            Err(e) => {
                eprintln!("bitscomp: xwayland: display :{n}: {e}");
                n += 1;
            }
        }
    };
    let mut xw = Xwayland {
        display,
        exe: exe.into(),
        listeners,
        lock,
        socket,
        tokens: Vec::new(),
        handle: handle.clone(),
        child: None,
        started: None,
        ready_ms: None,
        wm: None,
        sources: Vec::new(),
        quick_ends: 0,
        starts: 0,
        failed: false,
        stuck: false,
    };
    for l in &xw.listeners {
        let Ok(fd) = l.as_fd().try_clone_to_owned() else { continue };
        match handle.insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, _, app: &mut App| {
            start(app);
            Ok(PostAction::Continue)
        }) {
            Ok(t) => xw.tokens.push(t),
            Err(e) => eprintln!("bitscomp: xwayland: cannot watch the X socket: {e}"),
        }
    }
    eprintln!("bitscomp: xwayland on demand: DISPLAY=:{display} ({exe})");
    Some(xw)
}

/// A display's number, its sockets (path, abstract), its lock and socket file.
type Claimed = (u32, Vec<UnixListener>, PathBuf, PathBuf);

/// Display `n`'s lock and both sockets, Ok(None) when it is taken.
fn claim(n: u32) -> std::io::Result<Option<Claimed>> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let lock = PathBuf::from(format!("/tmp/.X{n}-lock"));
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
    let open = || std::fs::OpenOptions::new().write(true).create_new(true).mode(0o444).open(&lock);
    let mut f = match open() {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let pid: i32 = std::fs::read_to_string(&lock).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            // SAFETY: kill with signal 0 only asks whether the pid exists; nothing is sent.
            let alive = pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
            if alive {
                return Ok(None);
            }
            eprintln!("bitscomp: xwayland: stale lock of :{n} (pid {pid} is gone): taken over");
            std::fs::remove_file(&lock)?;
            let _ = std::fs::remove_file(&socket);
            open()?
        }
        Err(e) => return Err(e),
    };
    writeln!(f, "{:>10}", std::process::id())?;
    let release = |lock: &Path| {
        let _ = std::fs::remove_file(lock);
    };
    use std::os::linux::net::SocketAddrExt;
    let abs = std::os::unix::net::SocketAddr::from_abstract_name(socket.as_os_str().as_encoded_bytes())?;
    let a = match UnixListener::bind_addr(&abs) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            release(&lock);
            return Ok(None);
        }
        Err(e) => {
            release(&lock);
            return Err(e);
        }
    };
    let _ = std::fs::remove_file(&socket);
    let p = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(e) => {
            release(&lock);
            return Err(e);
        }
    };
    Ok(Some((n, vec![p, a], lock, socket)))
}

impl Drop for Xwayland {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.lock);
    }
}

/// `f` with the WM, if Xwayland runs and bitscomp is not inside another WM call.
pub fn with_wm<T>(app: &mut App, f: impl FnOnce(&mut Xwm, &mut App) -> T) -> Option<T> {
    let mut xw = app.xwayland.take()?;
    let out = xw.wm.as_mut().map(|wm| {
        let r = f(wm, app);
        wm.conn.flush();
        r
    });
    app.xwayland = Some(xw);
    out
}

fn start(app: &mut App) {
    let Some(mut xw) = app.xwayland.take() else { return };
    if xw.child.is_none()
        && !xw.failed
        && let Err(e) = xw.spawn(app)
    {
        eprintln!("bitscomp: xwayland: cannot start {}: {e}", xw.exe.display());
        xw.ended(app, false);
    }
    // While Xwayland runs it accepts on the sockets itself (`ended` watches them again).
    if xw.child.is_some() {
        for t in &xw.tokens {
            let _ = xw.handle.disable(t);
        }
    }
    app.xwayland = Some(xw);
}

/// Clears FD_CLOEXEC so the fd reaches the exec'd Xwayland under the same number.
fn inherit(fd: i32) {
    // SAFETY: fcntl on an fd that is open in this (forked) process; only its descriptor flags change.
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, 0);
    }
}

impl Xwayland {
    fn spawn(&mut self, app: &mut App) -> std::io::Result<()> {
        let (wl_ours, wl_theirs) = UnixStream::pair()?;
        let (wm_ours, wm_theirs) = UnixStream::pair()?;
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: pipe2 writes two new fds into the array; each is owned once below.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both were just made by pipe2 and belong to nobody else.
        let (ready_r, ready_w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let pass: Vec<i32> = self
            .listeners
            .iter()
            .map(|l| l.as_raw_fd())
            .chain([wl_theirs.as_raw_fd(), wm_theirs.as_raw_fd(), ready_w.as_raw_fd()])
            .collect();
        let mut c = std::process::Command::new(&self.exe);
        c.arg(format!(":{}", self.display)).arg("-rootless");
        for l in &self.listeners {
            c.arg("-listenfd").arg(l.as_raw_fd().to_string());
        }
        c.arg("-wm").arg(wm_theirs.as_raw_fd().to_string()).arg("-displayfd").arg(ready_w.as_raw_fd().to_string());
        c.env("WAYLAND_SOCKET", wl_theirs.as_raw_fd().to_string()).env_remove("WAYLAND_DISPLAY").env_remove("DISPLAY");
        for k in crate::flavour::GLES_ENV {
            c.env_remove(k);
        }
        c.stdin(std::process::Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe calls (fcntl, prctl, getppid, raise, sigprocmask) between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut c, move || {
                for fd in &pass {
                    inherit(*fd);
                }
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
                crate::spawn::unblock_all();
                Ok(())
            });
        }
        let child = c.spawn()?;
        drop((wl_theirs, wm_theirs, ready_w));
        let pid = child.id() as i32;
        // Kept at once: an error below ends it through `ended` (std's Child is neither killed nor reaped on drop).
        self.child = Some(child);
        self.starts += 1;
        self.started = Some(Instant::now());
        self.ready_ms = None;
        self.stuck = false;
        let data = ClientState { pid, start: crate::ending::start_time(pid), ..Default::default() };
        let client = app.dh.insert_client(wl_ours, Arc::new(data)).map_err(std::io::Error::other)?;
        let mut wm = Xwm::new(wm_ours, client)?;
        wm.sel.handle = Some(self.handle.clone());
        let watch = wm.conn.fd().try_clone_to_owned()?;
        self.wm = Some(wm);
        let t = self
            .handle
            .insert_source(Generic::new(watch, Interest::READ, Mode::Level), |_, _, app: &mut App| {
                // A closed connection stays readable (EOF): the source rests and `turn` ends this Xwayland.
                match with_wm(app, |wm, app| {
                    wm.dispatch(app);
                    wm.conn.closed
                }) {
                    Some(false) => Ok(PostAction::Continue),
                    _ => Ok(PostAction::Disable),
                }
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.sources.push(t);
        let mut ready = std::fs::File::from(ready_r);
        let mut said = Vec::new();
        let t = self
            .handle
            .insert_source(Generic::new(ready.try_clone()?, Interest::READ, Mode::Level), move |_, _, app: &mut App| {
                let mut b = [0u8; 32];
                match ready.read(&mut b) {
                    Ok(n) if n > 0 => {
                        said.extend_from_slice(&b[..n]);
                        if !said.contains(&b'\n') {
                            return Ok(PostAction::Continue);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted => {
                        return Ok(PostAction::Continue);
                    }
                    // EOF or a broken pipe before the display number: not ready (`turn` sees its end).
                    end => {
                        let why = match end {
                            Err(e) => e.to_string(),
                            _ => "end of the pipe".into(),
                        };
                        eprintln!("bitscomp: xwayland: pid {pid} closed -displayfd before it was ready ({why})");
                        return Ok(PostAction::Remove);
                    }
                }
                if let Some(xw) = app.xwayland.as_mut() {
                    xw.ready_ms = xw.started.map(|s| s.elapsed().as_millis());
                    eprintln!(
                        "bitscomp: xwayland: ready on :{} after {} ms (pid {pid})",
                        String::from_utf8_lossy(&said).trim(),
                        xw.ready_ms.unwrap_or(0)
                    );
                }
                Ok(PostAction::Remove)
            })
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.sources.push(t);
        let start = self.starts;
        let deadline = self.handle.insert_source(Timer::from_duration(READY_DEADLINE), move |_, _, app: &mut App| {
            if let Some(xw) = app.xwayland.as_mut()
                && xw.starts == start
                && xw.child.is_some()
                && !(xw.ready_ms.is_some() && xw.wm.as_ref().is_some_and(|w| w.ready))
            {
                eprintln!("bitscomp: xwayland: pid {pid} is not ready {} s after its start: ended", READY_DEADLINE.as_secs());
                xw.stuck = true;
                app.needs_frame = true;
            }
            TimeoutAction::Drop
        });
        if let Err(e) = deadline {
            eprintln!("bitscomp: xwayland: no start deadline ({e}): a hung start is not noticed");
        }
        eprintln!("bitscomp: xwayland: started {} :{} (pid {pid}, start {})", self.exe.display(), self.display, self.starts);
        Ok(())
    }

    /// Xwayland is gone or given up (`clean`: it ran, and its end or the end of its WM connection counts as quick
    /// only within QUICK_END of the start; false: it could not start or missed READY_DEADLINE, always quick).
    /// A child still running is killed and reaped here.
    fn ended(&mut self, app: &mut App, clean: bool) {
        if let Some(mut c) = self.child.take() {
            // Both are no-ops for a child `turn` has reaped already (std keeps its status).
            let _ = c.kill();
            let _ = c.wait();
        }
        self.stuck = false;
        if let Some(mut wm) = self.wm.take() {
            wm.forget_all(app);
            app.dh.backend_handle().kill_client(wm.client.id(), DisconnectReason::ConnectionClosed);
        }
        for t in self.sources.drain(..) {
            self.handle.remove(t);
        }
        let quick = !clean || self.started.is_some_and(|s| s.elapsed() < QUICK_END);
        let (n, off) = budget(self.quick_ends, quick);
        self.quick_ends = n;
        if off {
            self.failed = true;
            for t in self.tokens.drain(..) {
                self.handle.remove(t);
            }
            // The sockets close: an X client is refused at once instead of waiting.
            self.listeners.clear();
            let _ = std::fs::remove_file(&self.socket);
            eprintln!(
                "bitscomp: xwayland: {QUICK_ENDS_MAX} quick ends in a row (within {} s of the start, or not ready): :{} is off until bitscomp restarts",
                QUICK_END.as_secs(),
                self.display
            );
            return;
        }
        for t in &self.tokens {
            let _ = self.handle.enable(t);
        }
    }

    /// `ctl xwayland`: one line.
    pub fn describe(&self) -> String {
        let state = if self.failed {
            "failed"
        } else if self.child.is_none() {
            "idle"
        } else if self.wm.as_ref().is_some_and(|w| w.ready) {
            "ready"
        } else {
            "starting"
        };
        let pid = self.child.as_ref().map_or(0, |c| c.id());
        let (windows, sel) = self.wm.as_ref().map_or((0, String::from("clipboard=none")), |w| (w.windows(), w.sel.describe()));
        format!(
            "xwayland display=:{} state={state} pid={pid} starts={} ready_ms={} windows={windows} {sel}",
            self.display,
            self.starts,
            self.ready_ms.map_or(-1, |m| m as i64)
        )
    }
}

/// Once per loop turn (before the frame): Xwayland's end, then the WM's turn (places, focus, closes, writes).
pub fn turn(app: &mut App) {
    let Some(mut xw) = app.xwayland.take() else { return };
    let pid = xw.child.as_ref().map_or(0, |c| c.id());
    let gone = match xw.child.as_mut().map(|c| c.try_wait()) {
        Some(Ok(Some(status))) => {
            eprintln!("bitscomp: xwayland: pid {pid} ended ({status}); its X11 windows are gone");
            true
        }
        Some(Err(e)) => {
            eprintln!("bitscomp: xwayland: waiting for it failed: {e}");
            true
        }
        _ => false,
    };
    let lost = xw.child.is_some() && xw.wm.as_ref().is_some_and(|w| w.conn.closed);
    if gone {
        xw.ended(app, true);
    } else if xw.stuck {
        // Not serving the display in time: every X client would hang in its setup. Always a quick end.
        eprintln!("bitscomp: xwayland: pid {pid} missed its start deadline: ended");
        xw.ended(app, false);
    } else if lost {
        // No WM connection (EOF: it is ending, or refused setup, no Composite): the display would never be served
        // again. Quick only when it comes within QUICK_END of the start, as for an end of the process itself.
        eprintln!("bitscomp: xwayland: pid {pid} closed or refused its window manager connection: ended");
        xw.ended(app, true);
    } else if let Some(wm) = xw.wm.as_mut() {
        wm.turn(app);
    }
    app.xwayland = Some(xw);
}

/// The DISPLAY for programs bitscomp starts (none while no X display is up).
pub fn display(app: &App) -> Option<String> {
    app.xwayland.as_ref().filter(|x| !x.failed).map(|x| format!(":{}", x.display))
}

/// Selection hooks (`handlers`): a Wayland client's clipboard for X clients, X data for Wayland clients.
pub fn new_selection(app: &mut App, ty: SelectionTarget, source: Option<SelectionSource>) {
    let mimes = source.map(|s| s.mime_types());
    with_wm(app, |wm, _| crate::xsel_serve::wl_selection(wm, ty, mimes));
}

pub fn send_selection(app: &mut App, ty: SelectionTarget, mime: String, fd: OwnedFd) {
    if ty == SelectionTarget::Clipboard {
        with_wm(app, |wm, _| crate::xsel::send_selection(wm, &mime, fd));
    }
}

impl App {
    /// An X11 window of pid `pid` is on the screen: its start is answered (WY-7, as `note_first_frame` does).
    pub fn x11_mapped(&mut self, pid: u32) {
        // SAFETY: getpgid only reads the pid; an ended or foreign pid answers -1 (ESRCH).
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        let group = u32::try_from(pgid).unwrap_or(pid);
        if self.launches.mapped(pid) | self.launches.mapped(group) {
            self.needs_frame = true;
        }
    }

    /// The X11 windows drawn over everything (menus, tooltips of X11 programs).
    pub fn x_unmanaged(&self) -> Vec<crate::win::Window> {
        self.xwayland.as_ref().and_then(|x| x.wm.as_ref()).map(|w| w.unmanaged.clone()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_quick_ends_in_a_row_switch_the_display_off() {
        let (n, off) = budget(0, true);
        assert_eq!((n, off), (1, false));
        let (n, off) = budget(n, true);
        assert_eq!((n, off), (2, false));
        assert_eq!(budget(n, true), (3, true));
        // A run that lasted resets the count: the next quick end is the first again.
        assert_eq!(budget(n, false), (0, false));
        assert_eq!(budget(0, true), (1, false));
        assert_eq!(budget(u32::MAX, true), (u32::MAX, true));
    }
}
