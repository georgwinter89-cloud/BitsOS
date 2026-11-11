//! WY-3: Xwayland on demand. bitscomp owns the X display's sockets (/tmp/.X11-unix/X<n>, its abstract twin and the
//! lock /tmp/.X<n>-lock); the first X client's connection starts Xwayland (rootless) with those sockets
//! (`-listenfd`), a Wayland connection of its own (WAYLAND_SOCKET), the window manager's connection (`-wm`, `xwm`)
//! and `-displayfd` to say when it is ready. Programs bitscomp starts get DISPLAY=:<n>. When Xwayland ends, its
//! windows go and the sockets wait for the next client; three ends within 2 s of their starts in a row close the
//! display until bitscomp restarts (B6: every step is a line).
//!
//! WY-3b: glamor and DRI3 when bitscomp itself draws on the chip and names a render node that opens (`choose`), with the
//! Mesa tree's EGL variables for Xwayland alone; else XWAYLAND_NO_GLAMOR, the software Xwayland of WY-3. A glamor start that
//! ends quickly or misses its deadline does not count against the display: the next start is in software, and the
//! client waiting in the socket's queue is served by it. While a glamor Xwayland runs, a helper thread looks at the node
//! every `NODE_LOOK` (a lost device answers a new open with ENODEV, drivers/amdgpu `hw/mod.rs`): new windows would never
//! get a buffer there, so Xwayland ends and the next client gets the software one. Only an answer that says the device
//! is gone does that (`NodeLook`); bitscomp short of fds says nothing about the node. The open is off the loop: a driver
//! that blocks in it costs the look, not the desktop.

use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel::{self, Sender};
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
/// The same with glamor: it also starts EGL and the driver, read from the stick the first time (`flavour::CHECK_TIMEOUT_GLES`).
const READY_DEADLINE_GLAMOR: Duration = Duration::from_secs(30);
/// How often the render node is looked at while a glamor Xwayland runs: one open and close, nothing else.
const NODE_LOOK: Duration = Duration::from_secs(5);
/// How long a start waits for the look at the node, on the loop; only a driver that blocks in its open ever takes this long.
const START_LOOK: Duration = Duration::from_secs(1);

/// `--xwayland-glamor`: `Auto` decides by `choose`; `Force` is a test's: glamor whatever draws, watching the file named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Glamor {
    Auto,
    Off,
    Force(String),
}

impl Glamor {
    pub fn parse(v: &str) -> Option<Glamor> {
        match v {
            "auto" => Some(Glamor::Auto),
            "off" => Some(Glamor::Off),
            _ => v.strip_prefix("force:").filter(|p| !p.is_empty()).map(|p| Glamor::Force(p.into())),
        }
    }
}

/// What the next start depends on (`inputs`).
#[derive(Clone, Debug, Default)]
struct Inputs {
    /// A start with glamor that failed, or a node that was lost, until bitscomp restarts.
    given_up: Option<String>,
    /// The gpu build's Mesa tree; the rescue build has none.
    tree: Option<String>,
    reads_by_cpu: bool,
    /// What linux-dmabuf names (`dmabuf::advertised`): 0 means Xwayland would find no device.
    main_device: u64,
    node: Option<String>,
    /// Why `node` will not do for this start, when a look says so (`NodeLook::bars_a_start`).
    node_error: Option<String>,
}

/// The render node for a start with glamor, or why that start has none. Glamor's buffers are the chip's own memory, which
/// a renderer reading by CPU cannot map (WY-17b), so `--main-device render` under Pixman stays in software too.
fn choose(want: &Glamor, i: &Inputs) -> Result<String, String> {
    match want {
        Glamor::Off => return Err("--xwayland-glamor off".into()),
        Glamor::Force(p) if i.given_up.is_none() => return Ok(p.clone()),
        _ => {}
    }
    if let Some(why) = &i.given_up {
        return Err(why.clone());
    }
    if i.tree.is_none() {
        return Err("this build has no Mesa tree".into());
    }
    if i.reads_by_cpu {
        return Err("the renderer reads by CPU".into());
    }
    if i.main_device == 0 {
        return Err("linux-dmabuf names no main device".into());
    }
    let Some(node) = &i.node else { return Err("no render node".into()) };
    if let Some(e) = &i.node_error {
        return Err(format!("{node} does not open: {e}"));
    }
    Ok(node.clone())
}

/// What a new open of the node says.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeLook {
    Opens,
    /// The device is lost or unplugged (a lost device takes no new file; an unplugged node is gone).
    Gone(String),
    /// It is there and will not open (EACCES, or no answer in time): Xwayland's own open would meet the same.
    Refused(String),
    /// bitscomp's own shortage or a passing state (EMFILE, ENOMEM, EBUSY): nothing about the device.
    Unsure(String),
}

impl NodeLook {
    fn of(e: &std::io::Error) -> NodeLook {
        let text = e.to_string();
        match e.raw_os_error() {
            Some(libc::ENODEV | libc::ENXIO | libc::ENOENT) => NodeLook::Gone(text),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::EAGAIN | libc::EBUSY | libc::EINTR) => NodeLook::Unsure(text),
            _ => NodeLook::Refused(text),
        }
    }

    /// For a start: `Unsure` does not bar glamor, a glamor start that does fail costs a client nothing.
    fn bars_a_start(&self) -> Option<&str> {
        match self {
            NodeLook::Gone(e) | NodeLook::Refused(e) => Some(e),
            NodeLook::Opens | NodeLook::Unsure(_) => None,
        }
    }
}

fn node_look(node: &str) -> NodeLook {
    match std::fs::OpenOptions::new().read(true).write(true).open(node) {
        Ok(_) => NodeLook::Opens,
        Err(e) => NodeLook::of(&e),
    }
}

/// `look` on a thread of its own, waited for at most `limit`: the loop never waits on a driver without a bound.
fn look_within(limit: Duration, look: impl FnOnce() -> NodeLook + Send + 'static) -> NodeLook {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("bitscomp-xw-node".into()).spawn(move || {
        let _ = tx.send(look());
    });
    if let Err(e) = spawned {
        return NodeLook::Unsure(format!("no thread for the look: {e}"));
    }
    rx.recv_timeout(limit).unwrap_or_else(|_| NodeLook::Refused(format!("no answer within {} ms", limit.as_millis())))
}

/// What one look means for the running Xwayland that was started with glamor.
#[derive(Debug, PartialEq, Eq)]
enum Watch {
    Keep,
    /// Xwayland is serving without DRI3: it fell back itself, there is nothing left to watch.
    FellBack,
    Lost(String),
}

fn watch_step(fell_back: bool, look: &NodeLook) -> Watch {
    match look {
        _ if fell_back => Watch::FellBack,
        NodeLook::Gone(e) => Watch::Lost(e.clone()),
        NodeLook::Opens | NodeLook::Refused(_) | NodeLook::Unsure(_) => Watch::Keep,
    }
}

/// The helper of `watch_node`. It ends when the look's source is gone (`send` fails) or `stop` is set.
fn look_every(node: &str, tx: &Sender<NodeLook>, stop: &AtomicBool) {
    loop {
        std::thread::sleep(NODE_LOOK);
        if stop.load(Ordering::Acquire) || tx.send(node_look(node)).is_err() {
            return;
        }
    }
}

/// The Mesa tree's variables for Xwayland's EGL and GBM, on the real chip: `child_env` keeps them from every other program.
#[cfg(feature = "gpu")]
fn mesa_env(tree: &str) -> Vec<(&'static str, String)> {
    let mut v = crate::render_gl::env_values(tree, false);
    v.push(("AMDGPU_ASIC_ID_TABLE_PATHS", format!("{tree}/share/libdrm")));
    v
}

#[cfg(not(feature = "gpu"))]
fn mesa_env(_tree: &str) -> Vec<(&'static str, String)> {
    Vec::new()
}

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
    glamor: Glamor,
    /// Why glamor is over until bitscomp restarts.
    given_up: Option<String>,
    /// The node the running Xwayland was started on with glamor.
    glamor_node: Option<String>,
    /// Why the running (or last) Xwayland has no glamor.
    no_glamor: String,
    /// The node's look says it is gone (set by its timer, ended by `turn`).
    node_lost: bool,
}

/// The restart budget after one end of Xwayland: the new count of quick ends in a row, and whether the display
/// goes off (QUICK_ENDS_MAX quick ends in a row). An end that is not quick starts the count again.
fn budget(quick_ends: u32, quick: bool) -> (u32, bool) {
    let n = if quick { quick_ends.saturating_add(1) } else { 0 };
    (n, n >= QUICK_ENDS_MAX)
}

/// The display's sockets and lock, or None (no Xwayland at `exe`, or no free display).
pub fn setup(handle: &LoopHandle<'static, App>, exe: &str, glamor: Glamor) -> Option<Xwayland> {
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
        glamor,
        given_up: None,
        glamor_node: None,
        no_glamor: String::new(),
        node_lost: false,
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
    /// What decides this start, looked at now: a node may have gone since the last one.
    fn inputs(&self, app: &App) -> Inputs {
        let (_, node) = crate::dmabuf::render_device();
        let look = node.clone().map(|n| look_within(START_LOOK, move || node_look(&n)));
        if let Some(NodeLook::Unsure(e)) = &look {
            eprintln!("bitscomp: xwayland: the look at the render node says nothing about it ({e}): glamor is tried");
        }
        let node_error = look.as_ref().and_then(NodeLook::bars_a_start).map(String::from);
        Inputs {
            given_up: self.given_up.clone(),
            tree: crate::flavour::tree_root().map(String::from),
            reads_by_cpu: app.dma.reads_by_cpu,
            main_device: app.dma.main_device,
            node,
            node_error,
        }
    }

    /// Looks at the node while this start of glamor runs; gone: `turn` ends this Xwayland.
    fn watch_node(&mut self, start: u32, node: String) {
        let (tx, rx) = channel::channel::<NodeLook>();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_look = stop.clone();
        let watched = node.clone();
        let mut said = String::new();
        let look = self.handle.insert_source(rx, move |ev, _, app: &mut App| {
            let channel::Event::Msg(look) = ev else { return };
            let Some(xw) = app.xwayland.as_mut().filter(|x| x.starts == start && x.child.is_some() && x.glamor_node.is_some()) else {
                stop.store(true, Ordering::Release);
                return;
            };
            let fell_back = xw.wm.as_ref().is_some_and(|w| w.ready && !w.dri3);
            match watch_step(fell_back, &look) {
                Watch::Keep => {
                    let now = match &look {
                        NodeLook::Refused(e) | NodeLook::Unsure(e) => e.as_str(),
                        _ => "",
                    };
                    if now != said && !now.is_empty() {
                        eprintln!("bitscomp: xwayland: the look at {node} answers \"{now}\": not a lost device, this Xwayland stays");
                    }
                    said = now.to_string();
                }
                Watch::FellBack => {
                    eprintln!("bitscomp: xwayland: asked for glamor on {node}, but no DRI3 is offered: it fell back to software");
                    xw.glamor_node = None;
                    xw.no_glamor = "Xwayland fell back to software".into();
                    stop.store(true, Ordering::Release);
                }
                Watch::Lost(e) => {
                    let why = format!("{node} was lost: {e}");
                    eprintln!("bitscomp: xwayland: {why}: this Xwayland ends, the next X11 client gets a software one");
                    xw.given_up = Some(why);
                    xw.node_lost = true;
                    app.needs_frame = true;
                    stop.store(true, Ordering::Release);
                }
            }
        });
        match look {
            Ok(t) => self.sources.push(t),
            Err(e) => {
                eprintln!("bitscomp: xwayland: no look at {e}: a lost render node is not noticed");
                return;
            }
        }
        let helper = std::thread::Builder::new().name("bitscomp-xw-look".into()).spawn(move || look_every(&watched, &tx, &stop_look));
        if let Err(e) = helper {
            eprintln!("bitscomp: xwayland: no thread for the look at the node ({e}): a lost render node is not noticed");
        }
    }

    fn spawn(&mut self, app: &mut App) -> std::io::Result<()> {
        let inputs = self.inputs(app);
        let glamor = choose(&self.glamor, &inputs);
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
        // The variable, not `-glamor off`: a tree built without glamor (the stage before WY-3b) refuses the option and ends.
        match (&glamor, &inputs.tree) {
            (Err(_), _) => {
                c.env("XWAYLAND_NO_GLAMOR", "1");
            }
            (Ok(_), Some(tree)) => {
                c.env_remove("XWAYLAND_NO_GLAMOR").envs(mesa_env(tree));
            }
            (Ok(_), None) => {
                c.env_remove("XWAYLAND_NO_GLAMOR");
            }
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
        self.node_lost = false;
        let deadline_after = if glamor.is_ok() { READY_DEADLINE_GLAMOR } else { READY_DEADLINE };
        match &glamor {
            Ok(node) => {
                eprintln!("bitscomp: xwayland: this start with glamor on {node}");
                self.glamor_node = Some(node.clone());
                self.no_glamor.clear();
            }
            Err(why) => {
                eprintln!("bitscomp: xwayland: this start in software: {why}");
                self.glamor_node = None;
                self.no_glamor.clone_from(why);
            }
        }
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
        let deadline = self.handle.insert_source(Timer::from_duration(deadline_after), move |_, _, app: &mut App| {
            if let Some(xw) = app.xwayland.as_mut()
                && xw.starts == start
                && xw.child.is_some()
                && !(xw.ready_ms.is_some() && xw.wm.as_ref().is_some_and(|w| w.ready))
            {
                eprintln!("bitscomp: xwayland: pid {pid} is not ready {} s after its start: ended", deadline_after.as_secs());
                xw.stuck = true;
                app.needs_frame = true;
            }
            TimeoutAction::Drop
        });
        if let Err(e) = deadline {
            eprintln!("bitscomp: xwayland: no start deadline ({e}): a hung start is not noticed");
        }
        if let Some(node) = self.glamor_node.clone() {
            self.watch_node(start, node);
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
        self.node_lost = false;
        if let Some(mut wm) = self.wm.take() {
            wm.forget_all(app);
            app.dh.backend_handle().kill_client(wm.client.id(), DisconnectReason::ConnectionClosed);
        }
        for t in self.sources.drain(..) {
            self.handle.remove(t);
        }
        let quick = !clean || self.started.is_some_and(|s| s.elapsed() < QUICK_END);
        let node = self.glamor_node.take();
        if let Some(node) = node.filter(|_| quick) {
            // The driver or the node, not the display: the next start is the software one and the budget starts over.
            let why = format!("a start with glamor on {node} failed");
            eprintln!("bitscomp: xwayland: {why}: the next start is in software");
            self.given_up.get_or_insert(why);
            self.quick_ends = 0;
            for t in &self.tokens {
                let _ = self.handle.enable(t);
            }
            return;
        }
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
        let dri3 = self.wm.as_ref().is_some_and(|w| w.dri3);
        let glamor = match (&self.glamor_node, &self.given_up) {
            (Some(n), _) => format!("glamor={n}"),
            (None, Some(why)) => format!("glamor=off why={why:?}"),
            (None, None) if self.no_glamor.is_empty() => "glamor=unknown".into(),
            (None, None) => format!("glamor=off why={:?}", self.no_glamor),
        };
        format!(
            "xwayland display=:{} state={state} pid={pid} starts={} ready_ms={} windows={windows} {sel} dri3={dri3} {glamor}",
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
    } else if xw.node_lost {
        eprintln!("bitscomp: xwayland: pid {pid} ends: its render node is lost");
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

    fn gpu() -> Inputs {
        Inputs { tree: Some("/t".into()), main_device: 0xe280, node: Some("/dev/dri/renderD128".into()), ..Default::default() }
    }

    #[test]
    fn glamor_only_when_bitscomp_draws_on_a_node_that_opens() {
        assert_eq!(choose(&Glamor::Auto, &gpu()), Ok("/dev/dri/renderD128".to_string()));
        let no = |f: fn(&mut Inputs), why: &str| {
            let mut i = gpu();
            f(&mut i);
            let got = choose(&Glamor::Auto, &i).unwrap_err();
            assert!(got.contains(why), "{got}");
        };
        no(|i| i.tree = None, "no Mesa tree");
        no(|i| i.reads_by_cpu = true, "reads by CPU");
        no(|i| i.main_device = 0, "no main device");
        no(|i| i.node = None, "no render node");
        no(|i| i.node_error = Some("No such device".into()), "does not open: No such device");
        no(|i| i.given_up = Some("a start with glamor failed".into()), "failed");
        assert!(choose(&Glamor::Off, &gpu()).is_err());
    }

    #[test]
    fn a_forced_glamor_still_gives_up() {
        let force = Glamor::Force("/x".into());
        assert_eq!(choose(&force, &Inputs::default()), Ok("/x".to_string()));
        let bad = Inputs { given_up: Some("lost".into()), ..Default::default() };
        assert_eq!(choose(&force, &bad), Err("lost".to_string()));
    }

    #[test]
    fn the_switch_parses() {
        assert_eq!(Glamor::parse("auto"), Some(Glamor::Auto));
        assert_eq!(Glamor::parse("off"), Some(Glamor::Off));
        assert_eq!(Glamor::parse("force:/n"), Some(Glamor::Force("/n".into())));
        assert_eq!(Glamor::parse("force:"), None);
        assert_eq!(Glamor::parse("on"), None);
    }

    #[test]
    fn a_node_that_does_not_open_says_so() {
        assert_eq!(node_look("/dev/null"), NodeLook::Opens);
        assert!(matches!(node_look("/nonexistent/renderD128"), NodeLook::Gone(_)));
        // EISDIR: it is there and will not open, the device is not lost.
        assert!(matches!(node_look("/"), NodeLook::Refused(_)));
    }

    fn look_of(errno: i32) -> NodeLook {
        NodeLook::of(&std::io::Error::from_raw_os_error(errno))
    }

    #[test]
    fn only_an_answer_that_the_device_is_gone_ends_xwayland() {
        for e in [libc::ENODEV, libc::ENXIO, libc::ENOENT] {
            assert!(matches!(watch_step(false, &look_of(e)), Watch::Lost(_)), "errno {e}");
        }
        for e in [libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EAGAIN, libc::EBUSY, libc::EINTR, libc::EACCES, libc::EIO] {
            assert_eq!(watch_step(false, &look_of(e)), Watch::Keep, "errno {e}");
        }
        assert_eq!(watch_step(false, &NodeLook::Opens), Watch::Keep);
        assert_eq!(watch_step(true, &NodeLook::Opens), Watch::FellBack);
        // Fallen back itself, the software Xwayland has nothing to do with the node any more.
        assert_eq!(watch_step(true, &look_of(libc::ENODEV)), Watch::FellBack);
    }

    #[test]
    fn a_start_is_barred_by_a_node_that_will_not_open_not_by_our_shortage() {
        assert_eq!(NodeLook::Opens.bars_a_start(), None);
        for e in [libc::EMFILE, libc::ENFILE, libc::ENOMEM, libc::EBUSY] {
            assert_eq!(look_of(e).bars_a_start(), None, "errno {e}");
        }
        for e in [libc::ENODEV, libc::ENOENT, libc::EACCES] {
            assert!(look_of(e).bars_a_start().is_some(), "errno {e}");
        }
    }

    #[test]
    fn a_look_that_blocks_costs_the_loop_only_its_limit() {
        let look = look_within(Duration::from_millis(10), || {
            std::thread::sleep(Duration::from_secs(2));
            NodeLook::Opens
        });
        assert!(matches!(&look, NodeLook::Refused(e) if e.contains("no answer")), "{look:?}");
        assert!(look.bars_a_start().is_some());
        assert_eq!(look_within(Duration::from_secs(60), || NodeLook::Opens), NodeLook::Opens);
    }

    #[test]
    fn the_helper_ends_with_its_source() {
        let (tx, rx) = channel::channel::<NodeLook>();
        drop(rx);
        assert!(tx.send(NodeLook::Opens).is_err());
    }

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
