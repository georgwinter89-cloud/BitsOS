//! WY-14 (gx-roadmap.md §9 answer 2, gx-scout-mesa.md step C0): the two builds of bitscomp and how the one hands
//! over to the other.
//!
//! - The rescue build (`/bin/bitscomp`, without the `gpu` feature) runs with the system's libraries of the
//!   initramfs and never loads anything of the Mesa tree. It is what every start runs first.
//! - The gpu build (`bitscomp-gpu`, `--features gpu`, on the stick at `GPU_EXE`) is linked against the Mesa tree's
//!   own glibc: its loader is the tree's `ld-linux-x86-64.so.2` and its DT_RPATH the tree's `lib/` (build.rs), so
//!   Smithay can dlopen the tree's libEGL later (WY-15) without the tree's libc meeting the initramfs's. It draws
//!   with Pixman exactly like the rescue build today.
//!
//! The rescue build hands over only on request: `--build gpu` on its command line, or else CONFIG.TXT
//! `bitscomp=gpu` on the stick. It first starts the gpu build once as `bitscomp-gpu check-build` (bounded wait,
//! `CHECK_TIMEOUT`): only when that comes back with exit 0 and its answer line does it execve the gpu build (same
//! pid, same arguments, same argv[0]). The check proves what an execve cannot undo: the tree's loader resolves
//! every library, its glibc fits, and Pixman makes a renderer. A missing gpu build or tree, a broken or incomplete
//! tree, a garbage file, a hang, and a failed execve all keep the rescue build running. The `exec`, `page`, `ctl`,
//! `licenses` and `check-build` commands never hand over.
//!
//! WY-17: `bitscomp=auto` asks for GLES only with a render node up (GX-8c, `gpu=on`, or the fake GPU): the check then
//! decides as for `gles`, and without a node or with a failed check the rescue build's Pixman draws, said in the log.
//!
//! WY-21: `bitscomp=auto` is the default (a CONFIG.TXT without the key); without a CONFIG.TXT (a host) the rescue build
//! stays. The node must open, not only be there: a lost device keeps its node and answers a new open with ENODEV.
//! The hand-over no longer replaces the rescue build: it starts the gpu build as its child and waits beside it, as
//! nothing but a waiting process (no file, no socket, root kept for the screen). When the gpu build gives the GPU up
//! (`gpu_watch::LOST_EXIT`), crashes, or ends with an error, the rescue build takes the screen itself and brings the
//! gpu build's session back (`Session::restore_since`); a clean end or a kill ends it too (`after_gpu`). SIGTERM,
//! SIGINT and SIGHUP are passed on; the gpu build dies with it (PR_SET_PDEATHSIG). While it waits its command name is
//! `bitscomp-rescue`, so `pidof bitscomp` names the compositor alone.
//!
//! The kernel names the process after the file it ran (`/proc/PID/comm`: bitscomp-gpu), so the gpu build sets its
//! command name back to `bitscomp` first thing (PR_SET_NAME): `pidof bitscomp`, `ps` and anything that matches the
//! command name see the compositor under one name in both builds; `/proc/PID/exe` still tells them apart.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::render::CompRenderer;

/// Whether this is the gpu build.
pub const GPU: bool = cfg!(feature = "gpu");
/// Where the gpu build lies on V2: the stick's programs, beside the Mesa tree it runs with (`\BITSOS2\bin\`).
pub const GPU_EXE: &str = "/boot/BITSOS2/bin/bitscomp-gpu";
/// The stick's CONFIG.TXT as V2 shows it (`--config` names another, for host tests).
pub const CONFIG: &str = "/boot/BITSOS2/CONFIG.TXT";
/// The CONFIG.TXT key: `bitscomp=rescue|gpu|gles|auto` (default auto, WY-21).
pub const KEY: &str = "bitscomp";
/// WY-17: the CONFIG.TXT key `bitscomp_gl=software|hardware`: what `gles` draws with, past `gl_software`'s choice.
pub const GL_KEY: &str = "bitscomp_gl";
/// The gpu build's self-check before the hand-over.
pub const CHECK_ARG: &str = "check-build";
/// How long the rescue build waits for the check (B4: bounded; the tree is loaded from the stick the first time).
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(10);
/// WY-15: the check of the GLES renderer also starts EGL and llvmpipe (libgallium and LLVM, read from the stick the
/// first time) and draws a frame: bounded too, but longer.
pub const CHECK_TIMEOUT_GLES: Duration = Duration::from_secs(30);
/// WY-15: the variables the gpu build sets for its own EGL (`render_gl::set_env`) and that no program bitscomp
/// starts may inherit (`App::spawn` removes them): they name the tree, which only the gpu build's glibc may load.
pub const GLES_ENV: &[&str] = &[
    "__EGL_VENDOR_LIBRARY_FILENAMES",
    "LIBGL_DRIVERS_PATH",
    "GBM_BACKENDS_PATH",
    "DRIRC_CONFIGDIR",
    "LIBGL_ALWAYS_SOFTWARE",
    "MESA_SHADER_CACHE_DISABLE",
];
/// The gpu build's tree's lib/ (build.rs): its loader, DT_RPATH, and where every mapped library must lie.
const TREE_LIB: Option<&str> = option_env!("BITSCOMP_GPU_LIB");
/// The first words of the check's answer on stdout.
const CHECK_OK: &str = "bitscomp check-build: build=gpu";

pub fn name() -> &'static str {
    if GPU { "gpu" } else { "rescue" }
}

/// The tree the gpu build was linked for (the parent of its lib/), for `render_gl::set_env`.
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub fn tree_root() -> Option<&'static str> {
    TREE_LIB.filter(|_| GPU).map(|l| l.trim_end_matches('/')).and_then(|l| l.rsplit_once('/')).map(|(root, _)| root)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Build {
    Rescue,
    Gpu,
    /// WY-15: the gpu build drawing with GLES (`bitscomp=gles`, `--build gles`).
    Gles,
    /// WY-17: GLES when a render node is up and the check proves the renderer, else the rescue build's Pixman.
    Auto,
}

pub fn parse(v: &str) -> Option<Build> {
    match v.trim() {
        "rescue" => Some(Build::Rescue),
        "gpu" => Some(Build::Gpu),
        "gles" => Some(Build::Gles),
        "auto" => Some(Build::Auto),
        _ => None,
    }
}

/// WY-15: what draws the picture. Pixman everywhere by default; GLES only in the gpu build and only when chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    Pixman,
    Gles,
}

impl Renderer {
    pub fn parse(v: &str) -> Option<Renderer> {
        match v.trim() {
            "pixman" => Some(Renderer::Pixman),
            "gles" => Some(Renderer::Gles),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Renderer::Pixman => "pixman",
            Renderer::Gles => "gles",
        }
    }
}

/// WY-15: the gpu build's renderer: `--renderer` first, then `--build gles`/`gpu`, then CONFIG.TXT `bitscomp=gles`
/// (the rescue build hands that over as `--renderer gles`; this covers a direct start), else Pixman. The second
/// value says where the choice came from. `bitscomp=auto` in a gpu build started directly: GLES with a render node.
pub fn renderer_wanted(cli: Option<&str>, build: Option<&str>, config: Result<&str, &str>, node: bool) -> (Renderer, String) {
    if let Some(v) = cli {
        return match Renderer::parse(v) {
            Some(r) => (r, format!("--renderer {v}")),
            None => (Renderer::Pixman, format!("--renderer {v:?} is neither pixman nor gles")),
        };
    }
    if let Some(b) = build.and_then(parse) {
        return match b {
            Build::Gles => (Renderer::Gles, "--build gles".into()),
            Build::Auto if node => (Renderer::Gles, "--build auto, a render node is up".into()),
            _ => (Renderer::Pixman, "default".into()),
        };
    }
    let Ok(text) = config else { return (Renderer::Pixman, "default".into()) };
    match config_value(text, KEY).as_deref().map(parse) {
        Some(Some(Build::Gles)) => (Renderer::Gles, format!("CONFIG.TXT {KEY}=gles")),
        Some(Some(Build::Auto)) if node => (Renderer::Gles, format!("CONFIG.TXT {KEY}=auto, a render node is up")),
        None if node => (Renderer::Gles, "default auto, a render node is up".into()),
        _ => (Renderer::Pixman, "default".into()),
    }
}

/// The arguments the rescue build adds when it hands over for GLES that its own command line did not ask for by
/// `--renderer` (CONFIG.TXT `bitscomp=gles`): the gpu build then needs no CONFIG.TXT of its own.
pub fn hand_over_args(build: Build, argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if matches!(build, Build::Gles | Build::Auto) && !argv.iter().any(|a| a == "--renderer") {
        out.extend(["--renderer".to_string(), "gles".to_string()]);
    }
    out
}

/// The renderer given to `check-build` (its arguments after the command): `--renderer gles`, else Pixman.
pub fn check_renderer(args: &[String]) -> Result<Renderer, String> {
    match args {
        [] => Ok(Renderer::Pixman),
        [flag, v] if flag == "--renderer" => Renderer::parse(v).ok_or(format!("--renderer {v:?} is neither pixman nor gles")),
        _ => Err(format!("{CHECK_ARG} [--renderer pixman|gles], not {args:?}")),
    }
}

/// The value of `key` in a CONFIG.TXT text as the kernel reads it: `name=value` lines, `#` starts a comment, outer
/// spaces trimmed, the last line wins.
pub fn config_value(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        if let Some((k, v)) = line.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key) {
                found = Some(v.trim().to_string());
            }
        }
    }
    found
}

/// Which build is asked for: the command line first, then CONFIG.TXT (`config`: its text, or why it could not be
/// read). The second value says where the choice came from, for the log line.
pub fn wanted(cli: Option<&str>, renderer: Option<&str>, config: Result<&str, &str>) -> (Build, String) {
    if let Some(v) = cli {
        return match parse(v) {
            Some(b) => (b, format!("--build {v}")),
            None => (Build::Rescue, format!("--build {v:?} is neither rescue, gpu, gles nor auto")),
        };
    }
    // WY-15: GLES exists only in the gpu build, so asking for it on the command line asks for that build.
    if renderer == Some("gles") {
        return (Build::Gles, "--renderer gles".into());
    }
    match config {
        Ok(text) => match config_value(text, KEY) {
            None if renderer == Some("pixman") => (Build::Rescue, "default".into()),
            None => (Build::Auto, "default auto".into()),
            Some(v) => match parse(&v) {
                // `--renderer pixman` on the command line keeps the gpu build of `bitscomp=gles` on Pixman.
                Some(Build::Gles | Build::Auto) if renderer == Some("pixman") => (Build::Gpu, format!("CONFIG.TXT {KEY}={v}, --renderer pixman")),
                Some(b) => (b, format!("CONFIG.TXT {KEY}={v}")),
                None => (Build::Rescue, format!("CONFIG.TXT {KEY}={v:?} is neither rescue, gpu, gles nor auto")),
            },
        },
        Err(why) => (Build::Rescue, format!("default (CONFIG.TXT: {why})")),
    }
}

/// Called by the rescue build before anything is opened (one thread): hands over to the gpu build when asked.
/// Returns when the rescue build draws: None as before any hand-over, Some(the gpu build's start, unix seconds) when it
/// takes over from a gpu build that could not go on; `argv` without the program name.
pub fn hand_over(cli: Option<&str>, renderer: Option<&str>, gpu_exe: &str, config: &str, argv: &[String]) -> Option<i64> {
    if GPU {
        if cli == Some("rescue") {
            eprintln!("bitscomp: this is the gpu build: --build rescue is not followed (start /bin/bitscomp for the rescue build)");
        }
        return None;
    }
    let text = if cli.is_some() || renderer == Some("gles") { Ok(String::new()) } else { read_config(config) };
    let (build, why) = wanted(cli, renderer, text.as_deref().map_err(String::as_str));
    if build == Build::Rescue {
        if why != "default" && !why.starts_with("default (") {
            eprintln!("bitscomp: build rescue ({why})");
        }
        if renderer == Some("gles") {
            eprintln!("bitscomp: --renderer gles needs the gpu build; the rescue build draws with pixman");
        }
        return None;
    }
    // WY-17: `auto` asks for GLES only with a render node up; without one the rescue build's Pixman draws. WY-21: up
    // means it opens; a lost device keeps its node.
    if build == Build::Auto {
        let node = crate::dmabuf::render_device().1;
        let Some(node) = node else {
            eprintln!("bitscomp: build rescue ({why}): no render node");
            return None;
        };
        if let crate::gpu_watch::Look::Gone(e) = crate::gpu_watch::look_within(&node, NODE_LOOK) {
            eprintln!("bitscomp: build rescue ({why}): the GPU is lost ({e})");
            return None;
        }
    }
    let gles = matches!(build, Build::Gles | Build::Auto);
    let (r, timeout) = if gles { (Renderer::Gles, CHECK_TIMEOUT_GLES) } else { (Renderer::Pixman, CHECK_TIMEOUT) };
    let label = if gles { "gpu, renderer gles" } else { "gpu" };
    if let Err(e) = check_gpu(gpu_exe, r, timeout) {
        eprintln!("bitscomp: build {label} ({why}): {gpu_exe} {CHECK_ARG}: {e}; the rescue build runs on");
        return None;
    }
    eprintln!("bitscomp: build {label} ({why}): handing over to {gpu_exe}");
    let since = crate::status::epoch_secs();
    let arg0 = std::env::args_os().next().unwrap_or_else(|| "bitscomp".into());
    let me = std::process::id();
    let mut cmd = Command::new(gpu_exe);
    cmd.arg0(arg0).args(hand_over_args(build, argv)).env(RESCUE_ENV, me.to_string());
    // SAFETY: runs in the child between fork and exec, where only async-signal-safe calls are allowed: prctl and getppid
    // are. A parent that died before the prctl would never send the signal: the pid says so.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != me {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    // Orphans of the gpu build (its programs, Xwayland) come to this process, which ends them before it takes over.
    // SAFETY: a prctl with an integer argument.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1 as libc::c_ulong, 0, 0, 0) };
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bitscomp: build gpu: {gpu_exe}: {e}; the rescue build runs on");
            return None;
        }
    };
    eprintln!("bitscomp: the rescue build waits as pid {} beside the gpu build (pid {}): it takes over if the GPU is lost", std::process::id(), child.id());
    CHILD.store(child.id() as i32, std::sync::atomic::Ordering::Release);
    pass_signals(true);
    name_self(c"bitscomp-rescue");
    let status = child.wait();
    CHILD.store(0, std::sync::atomic::Ordering::Release);
    pass_signals(false);
    end_orphans();
    // SAFETY: as above.
    unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 0 as libc::c_ulong, 0, 0, 0) };
    name_self(c"bitscomp");
    let why = match status {
        Ok(st) => after_gpu(st),
        Err(e) => Ok(format!("could not be waited for ({e})")),
    };
    match why {
        Ok(why) => {
            eprintln!("bitscomp: the gpu build {why}: the rescue build takes the screen, with the session");
            Some(since)
        }
        Err(code) => std::process::exit(code),
    }
}

/// The environment variable that tells the gpu build which rescue build waits for it (its pid).
pub const RESCUE_ENV: &str = "BITSCOMP_RESCUE_PID";
/// The rescue build's pid, taken out of the environment at the start (`take_rescue`).
static RESCUE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The gpu build at its start, while it has one thread: the waiting rescue build's pid, out of the environment so no
/// program it starts inherits it.
pub fn take_rescue() {
    if let Some(p) = std::env::var(RESCUE_ENV).ok().and_then(|v| v.parse::<u32>().ok()) {
        RESCUE.store(p, std::sync::atomic::Ordering::Release);
    }
    // SAFETY: called first in main, before any thread exists.
    unsafe { std::env::remove_var(RESCUE_ENV) };
}

/// After the gpu build gave root up: the kernel cleared its parent-death signal with the credentials (Linux
/// commit_creds, V2 `proc::cred`), so it is armed again; a rescue build that died meanwhile is no parent any more, and
/// the gpu build ends (nobody would take over, and nothing ends it otherwise).
pub fn rearm_death_signal() {
    let want = RESCUE.load(std::sync::atomic::Ordering::Acquire);
    if want == 0 {
        return;
    }
    // SAFETY: a prctl with integer arguments, and getppid.
    let (armed, parent) = unsafe { (libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0), libc::getppid() as u32) };
    if armed != 0 {
        eprintln!("bitscomp: PR_SET_PDEATHSIG: {}", std::io::Error::last_os_error());
    }
    if parent != want {
        eprintln!("bitscomp: the rescue build (pid {want}) is gone (parent now {parent}): the gpu build ends");
        std::process::exit(1);
    }
}

/// The processes the gpu build left (they came here as their subreaper): SIGTERM to each one's group, SIGKILL after a
/// second, all reaped. Read from /proc: this process has no other children now.
fn end_orphans() {
    let me = std::process::id();
    let kids = || -> Vec<i32> {
        let Ok(d) = std::fs::read_dir("/proc") else { return Vec::new() };
        d.flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|&p| std::fs::read_to_string(format!("/proc/{p}/stat")).ok().and_then(|t| stat_ppid(&t)) == Some(me))
            .collect()
    };
    // SAFETY: waitpid without a status pointer, never blocking.
    let reap = || while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {};
    let first = kids();
    if first.is_empty() {
        return;
    }
    eprintln!("bitscomp: the gpu build left {} programs: ending them before the session comes back", first.len());
    for sig in [libc::SIGTERM, libc::SIGKILL] {
        for &p in &kids() {
            // SAFETY: kill(2) of a child of ours (unreaped, so its pid is not reused) and of the group it leads.
            unsafe {
                libc::kill(-p, sig);
                libc::kill(p, sig);
            }
        }
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            reap();
            if kids().is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    reap();
}

/// The parent pid in a /proc/PID/stat line (the fourth field, after the name in parentheses).
fn stat_ppid(text: &str) -> Option<u32> {
    text.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// How long the hand-over waits for one open of the render node.
const NODE_LOOK: Duration = Duration::from_secs(2);

/// What the rescue build does once the gpu build ended: Ok(why) it takes over, Err(status) it ends with that status.
pub fn after_gpu(st: std::process::ExitStatus) -> Result<String, i32> {
    use std::os::unix::process::ExitStatusExt;
    match (st.code(), st.signal()) {
        (Some(0), _) => Err(0),
        (Some(crate::gpu_watch::LOST_EXIT), _) => Ok("gave the GPU up".into()),
        (Some(c), _) => Ok(format!("ended with status {c}")),
        // Someone ended it on purpose: the desktop ends as it would have without a hand-over.
        (None, Some(sig @ (libc::SIGKILL | libc::SIGTERM | libc::SIGINT | libc::SIGHUP))) => Err(128 + sig),
        (None, Some(sig)) => Ok(format!("crashed (signal {sig})")),
        (None, None) => Ok("ended in an unknown way".into()),
    }
}

/// The gpu build's pid while the rescue build waits for it.
static CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
const PASSED: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

extern "C" fn pass_on(sig: libc::c_int) {
    let pid = CHILD.load(std::sync::atomic::Ordering::Acquire);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe; the pid is our unreaped child's.
        unsafe { libc::kill(pid, sig) };
    }
}

/// The signals that end a desktop go to the gpu build while the rescue build waits (on), default again after (off).
fn pass_signals(on: bool) {
    for sig in PASSED {
        // SAFETY: a sigaction with a handler that only calls kill(2), or the default action; no memory but the struct.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = if on { pass_on as extern "C" fn(libc::c_int) as libc::sighandler_t } else { libc::SIG_DFL };
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

fn name_self(name: &std::ffi::CStr) {
    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes; nothing else is passed.
    unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr() as libc::c_ulong, 0, 0, 0) };
}

/// WY-17: whether GLES draws with the software rasteriser. `config` is CONFIG.TXT's `bitscomp_gl=`, which wins; else a
/// real GPU only on V2's own hardware with a render node: a host's GPU is not this test's (wy/98 compares
/// llvmpipe's picture), and under a hypervisor the node is the fake GPU, which runs no shader and draws nothing.
pub fn software_gl(config: Option<&str>, on_v2: bool, hypervisor: bool, node: bool) -> (bool, &'static str) {
    match config {
        Some("software") => (true, "CONFIG.TXT bitscomp_gl=software"),
        Some("hardware") => (false, "CONFIG.TXT bitscomp_gl=hardware"),
        _ if !on_v2 => (true, "not on V2"),
        _ if hypervisor => (true, "under a hypervisor"),
        _ if !node => (true, "no render node"),
        _ => (false, "a render node on the machine"),
    }
}

/// `software_gl` on this machine, said once by the caller.
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub fn gl_software(config_path: &str) -> (bool, &'static str) {
    let text = read_config(config_path).unwrap_or_default();
    let key = config_value(&text, GL_KEY);
    let leaf1 = std::arch::x86_64::__cpuid(1);
    let on_v2 = std::path::Path::new("/proc/bitsos").exists();
    software_gl(key.as_deref(), on_v2, leaf1.ecx & (1 << 31) != 0, crate::dmabuf::render_device().1.is_some())
}

/// Starts `gpu_exe check-build` (with `--renderer gles` for GLES) and waits at most `timeout` for it: Ok only for
/// exit 0 with the answer line naming that renderer.
pub fn check_gpu(gpu_exe: &str, renderer: Renderer, timeout: Duration) -> Result<(), String> {
    let mut cmd = Command::new(gpu_exe);
    cmd.arg(CHECK_ARG);
    if renderer == Renderer::Gles {
        cmd.args(["--renderer", "gles"]);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("does not start: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} s (stopped)", timeout.as_secs()));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("wait: {e}"));
            }
        }
    };
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        // The child has exited: what it wrote is all there is (a pipe, bounded by its buffer).
        let _ = so.read_to_string(&mut out);
    }
    if !status.success() {
        return Err(format!("{status}"));
    }
    let want = format!(" renderer={} ", renderer.name());
    match out.lines().find(|l| l.starts_with(CHECK_OK)) {
        Some(l) if l.contains(&want) => Ok(()),
        Some(l) => Err(format!("answered for another renderer ({l:?})")),
        None => Err(format!("exit 0 but no answer line ({:?})", out.chars().take(80).collect::<String>())),
    }
}

/// `bitscomp check-build`: what the hand-over needs of this build, said on stdout; the exit status. The gpu build
/// also refuses a mixed process: a library the tree lacks is found by its loader's default path (the initramfs's
/// /usr/lib), which would put another glibc's libraries next to the tree's glibc.
pub fn check_build<R: CompRenderer>() -> i32 {
    let t0 = Instant::now();
    let made = R::create().and_then(|mut r| check_frame(&mut r));
    let egl = egl_probe();
    let made = made.and_then(|()| match TREE_LIB.filter(|_| GPU) {
        None => Ok(()),
        Some(lib) => {
            let maps = std::fs::read_to_string("/proc/self/maps").map_err(|e| format!("/proc/self/maps: {e}"))?;
            let out = libs_outside(&maps, lib);
            if out.is_empty() { Ok(()) } else { Err(format!("libraries outside {lib}: {}", out.join(" "))) }
        }
    });
    match made {
        Ok(()) => {
            println!("bitscomp check-build: build={} renderer={} egl={egl} ms={}", name(), R::NAME, t0.elapsed().as_millis());
            0
        }
        Err(e) => {
            eprintln!("bitscomp check-build: build={}: {e}", name());
            1
        }
    }
}

/// WY-15: one 2x2 frame cleared to a known colour and read back through `pixels`, as the back ends will: proves the
/// renderer draws and its picture comes out in XRGB8888 with the rows and channels where fb0 expects them.
fn check_frame<R: CompRenderer>(r: &mut R) -> Result<(), String> {
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::utils::Transform;
    let mut target = r.target(2, 2)?;
    let mut tracker = OutputDamageTracker::new((2, 2), 1.0, Transform::Normal);
    let (damage, _) = r.draw(&mut target, &mut tracker, &[], CHECK_COLOUR.0)?;
    if damage.is_empty() {
        return Err("the check frame drew nothing".into());
    }
    let (px, stride) = R::pixels(&target);
    for y in 0..2 {
        for x in 0..2 {
            let p = px.get(y * stride + x).copied().unwrap_or(0);
            if !crate::readback::near(p, CHECK_COLOUR.1, 1) {
                return Err(format!("the check frame reads back {p:#010x} at {x},{y}, not {:#08x}", CHECK_COLOUR.1));
            }
        }
    }
    r.cleanup();
    Ok(())
}

/// Red, green and blue all different, so swapped channels show; exact in eight bits (0.2*255 = 51).
const CHECK_COLOUR: (smithay::backend::renderer::Color32F, u32) =
    (smithay::backend::renderer::Color32F::new(0.2, 0.4, 0.6, 1.0), 0x0033_6699);

/// The shared libraries in a /proc/PID/maps text that do not lie in `dir`.
pub fn libs_outside(maps: &str, dir: &str) -> Vec<String> {
    let dir = dir.trim_end_matches('/');
    let mut out: Vec<String> = Vec::new();
    for path in maps.lines().filter_map(|l| l.split_whitespace().nth(5)) {
        let file = path.rsplit('/').next().unwrap_or(path);
        let shared = file.split_once(".so").is_some_and(|(_, rest)| rest.is_empty() || rest.strip_prefix('.').is_some_and(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.')));
        let inside = path.strip_prefix(dir).is_some_and(|r| r.starts_with('/'));
        if shared && path.starts_with('/') && !inside && !out.iter().any(|p| p == path) {
            out.push(path.to_string());
        }
    }
    out
}

/// The gpu build: the command name back to `bitscomp` (the kernel set it from the file name, bitscomp-gpu).
pub fn set_comm() {
    if !GPU {
        return;
    }
    // SAFETY: PR_SET_NAME reads a NUL-terminated string of at most 16 bytes from a static; nothing else is passed.
    let r = unsafe { libc::prctl(libc::PR_SET_NAME, c"bitscomp".as_ptr() as libc::c_ulong, 0, 0, 0) };
    if r != 0 {
        eprintln!("bitscomp: build gpu: PR_SET_NAME: {}", std::io::Error::last_os_error());
    }
}

fn read_config(path: &str) -> Result<String, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(String::from_utf8_lossy(&b).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("none".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// The gpu build's look at the Mesa tree: whether `libEGL.so.1` loads (through the build's DT_RPATH, the way
/// Smithay's libloading will open it), then unloaded again. Nothing of EGL is called. The rescue build says "off".
pub fn egl_probe() -> String {
    if !GPU {
        return "off".into();
    }
    probe("libEGL.so.1")
}

fn probe(soname: &str) -> String {
    let Ok(name) = std::ffi::CString::new(soname) else { return "bad-name".into() };
    // SAFETY: dlopen with a NUL-terminated name; the handle is only passed back to dlclose, and dlerror's text is
    // copied before the next dl call. Loading runs the library's constructors (glvnd's: none that touch a display).
    unsafe {
        let h = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if h.is_null() {
            let err = libc::dlerror();
            let why = if err.is_null() { "unknown".into() } else { std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned() };
            eprintln!("bitscomp: build gpu: {soname} does not load: {why}");
            return "missing".into();
        }
        libc::dlclose(h);
    }
    "loadable".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_like_the_kernel() {
        assert_eq!(config_value("a=1\nbitscomp=gpu\n", KEY).as_deref(), Some("gpu"));
        assert_eq!(config_value("bitscomp=gpu\nbitscomp = rescue # back\n", KEY).as_deref(), Some("rescue"));
        assert_eq!(config_value("# bitscomp=gpu\nbitscompx=gpu\n", KEY), None);
        assert_eq!(config_value("BITSCOMP=gpu", KEY).as_deref(), Some("gpu"));
        assert_eq!(config_value("", KEY), None);
        // CONFIG.TXT written on Windows: CRLF lines.
        assert_eq!(config_value("loglevel=info\r\nbitscomp=gpu\r\n", KEY).as_deref(), Some("gpu"));
        assert_eq!(wanted(None, None, Ok("bitscomp=gpu\r\n")).0, Build::Gpu);
    }

    #[test]
    fn the_command_line_wins_and_anything_odd_stays_rescue() {
        assert_eq!(wanted(Some("gpu"), None, Ok("bitscomp=rescue")).0, Build::Gpu);
        assert_eq!(wanted(Some("rescue"), None, Ok("bitscomp=gpu")).0, Build::Rescue);
        assert_eq!(wanted(Some("fast"), None, Ok("bitscomp=gpu")).0, Build::Rescue);
        assert_eq!(wanted(None, None, Ok("bitscomp=gpu")).0, Build::Gpu);
        assert_eq!(wanted(None, None, Ok("bitscomp=GPU!")).0, Build::Rescue);
        // WY-21: a CONFIG.TXT without the key asks for auto; without a CONFIG.TXT (a host) the rescue build stays.
        assert_eq!(wanted(None, None, Ok("loglevel=info")), (Build::Auto, "default auto".into()));
        assert_eq!(wanted(None, Some("pixman"), Ok("loglevel=info")).0, Build::Rescue);
        assert_eq!(wanted(None, None, Err("none")).0, Build::Rescue);
    }

    #[test]
    fn gles_is_asked_for_by_build_renderer_or_config() {
        assert_eq!(wanted(Some("gles"), None, Ok("")).0, Build::Gles);
        assert_eq!(wanted(None, None, Ok("bitscomp=gles")).0, Build::Gles);
        assert_eq!(wanted(None, Some("gles"), Ok("bitscomp=rescue")), (Build::Gles, "--renderer gles".into()));
        // --build wins over --renderer: --build rescue --renderer gles stays rescue (the renderer is then pixman).
        assert_eq!(wanted(Some("rescue"), Some("gles"), Ok("")).0, Build::Rescue);
        assert_eq!(wanted(None, Some("pixman"), Ok("bitscomp=gles")).0, Build::Gpu);
        assert_eq!(wanted(None, None, Ok("bitscomp=auto")).0, Build::Auto);
        assert_eq!(wanted(None, Some("pixman"), Ok("bitscomp=auto")).0, Build::Gpu);
        assert_eq!(wanted(Some("auto"), None, Ok("")).0, Build::Auto);
        // The gpu build's renderer.
        assert_eq!(renderer_wanted(None, None, Ok(""), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(Some("gles"), Some("gpu"), Ok(""), false).0, Renderer::Gles);
        assert_eq!(renderer_wanted(Some("pixman"), Some("gles"), Ok("bitscomp=gles"), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(Some("vulkan"), None, Ok(""), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, Some("gles"), Ok(""), false).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, Some("gpu"), Ok("bitscomp=gles"), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=gles\r\n"), false).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=gpu"), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, None, Err("none"), false).0, Renderer::Pixman);
        // WY-17: auto is GLES only with a render node up.
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=auto"), true).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=auto"), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, Some("auto"), Ok(""), true).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, Some("auto"), Ok(""), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(Some("pixman"), None, Ok("bitscomp=auto"), true).0, Renderer::Pixman);
        // WY-21: no key is auto.
        assert_eq!(renderer_wanted(None, None, Ok("loglevel=info"), true).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, None, Ok("loglevel=info"), false).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=what"), true).0, Renderer::Pixman);
    }

    /// WY-21: what the waiting rescue build does when the gpu build ended.
    #[test]
    fn the_rescue_build_takes_over_unless_the_end_was_asked_for() {
        use std::os::unix::process::ExitStatusExt;
        let exited = |c: i32| std::process::ExitStatus::from_raw(c << 8);
        let killed = |s: i32| std::process::ExitStatus::from_raw(s);
        assert_eq!(after_gpu(exited(0)), Err(0), "a clean end (ctl quit, Strg+Alt+Q, SIGTERM) ends the desktop");
        assert_eq!(after_gpu(exited(crate::gpu_watch::LOST_EXIT)), Ok("gave the GPU up".into()));
        assert!(after_gpu(exited(1)).unwrap().contains("status 1"), "an error: the rescue build draws");
        assert!(after_gpu(killed(libc::SIGABRT)).unwrap().contains("crashed"), "Mesa's abort on a lost device");
        assert!(after_gpu(killed(libc::SIGSEGV)).is_ok());
        assert_eq!(after_gpu(killed(libc::SIGKILL)), Err(128 + libc::SIGKILL), "a kill -9 ends the desktop, as before");
        assert_eq!(after_gpu(killed(libc::SIGTERM)), Err(128 + libc::SIGTERM));
        assert_eq!(stat_ppid("42 (a (b) c) S 7 42 42 0"), Some(7), "the name may hold parentheses and spaces");
        assert_eq!(stat_ppid("garbage"), None);
    }

    /// WY-17: GLES on the real GPU only where a real GPU can be: never on a host's, never on the fake node.
    #[test]
    fn the_software_rasteriser_unless_the_machine_has_the_chip() {
        assert!(software_gl(None, false, false, true).0, "a host's GPU is not this test's");
        assert!(software_gl(None, true, true, true).0, "the node under a hypervisor is the fake GPU, which draws nothing");
        assert!(software_gl(None, true, false, false).0, "no node, no GPU");
        assert!(!software_gl(None, true, false, true).0, "the Mini-PC with the node up");
        assert!(!software_gl(Some("hardware"), true, true, false).0, "CONFIG wins");
        assert!(software_gl(Some("software"), true, false, true).0, "CONFIG wins");
        assert!(!software_gl(Some("?"), true, false, true).0, "an unknown value is no choice");
    }

    #[test]
    fn the_hand_over_carries_the_renderer() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(hand_over_args(Build::Gles, &a(&["--stats", "5"])), a(&["--stats", "5", "--renderer", "gles"]));
        assert_eq!(hand_over_args(Build::Gles, &a(&["--renderer", "gles"])), a(&["--renderer", "gles"]));
        assert_eq!(hand_over_args(Build::Gpu, &a(&["--stats", "5"])), a(&["--stats", "5"]));
        assert_eq!(hand_over_args(Build::Auto, &a(&["--stats", "5"])), a(&["--stats", "5", "--renderer", "gles"]));
        assert_eq!(check_renderer(&[]), Ok(Renderer::Pixman));
        assert_eq!(check_renderer(&a(&["--renderer", "gles"])), Ok(Renderer::Gles));
        assert!(check_renderer(&a(&["--renderer", "x"])).is_err());
        assert!(check_renderer(&a(&["gles"])).is_err());
    }

    #[test]
    fn the_check_must_answer_for_the_renderer_asked() {
        let d = std::env::temp_dir().join(format!("bitscomp-wy15-check-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("ok");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&f, format!("#!/bin/sh\necho \"{CHECK_OK} renderer=pixman egl=loadable ms=1 $*\"\n")).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let f = f.to_string_lossy().into_owned();
        assert_eq!(check_gpu(&f, Renderer::Pixman, CHECK_TIMEOUT), Ok(()));
        assert!(check_gpu(&f, Renderer::Gles, CHECK_TIMEOUT).unwrap_err().contains("another renderer"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_check_refuses_what_is_not_a_gpu_build() {
        let d = std::env::temp_dir().join(format!("bitscomp-wy14-check-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = |n: &str| d.join(n).to_string_lossy().into_owned();
        assert!(check_gpu(&p("missing"), Renderer::Pixman, CHECK_TIMEOUT).unwrap_err().starts_with("does not start"));
        let script = |n: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(d.join(n), body).unwrap();
            std::fs::set_permissions(d.join(n), std::fs::Permissions::from_mode(0o755)).unwrap();
            p(n)
        };
        assert!(check_gpu(&script("garbage", "\x7fELF garbage"), Renderer::Pixman, CHECK_TIMEOUT).is_err());
        assert!(check_gpu(&script("true", "#!/bin/sh\nexit 0\n"), Renderer::Pixman, CHECK_TIMEOUT).unwrap_err().contains("no answer line"));
        assert!(check_gpu(&script("loader", "#!/bin/sh\nexit 127\n"), Renderer::Pixman, CHECK_TIMEOUT).unwrap_err().contains("127"));
        let hang = script("hang", "#!/bin/sh\nexec sleep 30\n");
        assert!(check_gpu(&hang, Renderer::Pixman, Duration::from_millis(300)).unwrap_err().contains("no answer within"));
        let ok = script("ok", &format!("#!/bin/sh\necho '{CHECK_OK} renderer=pixman egl=loadable'\n"));
        assert_eq!(check_gpu(&ok, Renderer::Pixman, CHECK_TIMEOUT), Ok(()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_library_outside_the_tree_is_named_once() {
        let maps = "\
55-56 r-xp 0 00:1 1 /boot/BITSOS2/bin/bitscomp-gpu
7f-80 r-xp 0 00:1 2 /boot/BITSOS2/mesa/lib/libc.so.6
7f-80 r--p 0 00:1 2 /boot/BITSOS2/mesa/lib/libc.so.6
7f-80 r-xp 0 00:1 3 /boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2
7f-80 r-xp 0 00:1 4 /usr/lib/libpixman-1.so.0.46.4
7f-80 r--p 0 00:1 4 /usr/lib/libpixman-1.so.0.46.4
7f-80 r-xp 0 00:1 5 /boot/BITSOS2/mesa/lib/dri/radeonsi_dri.so
7f-80 rw-p 0 00:0 0 [heap]
7f-80 rw-p 0 00:0 0
7f-80 r--p 0 00:1 6 /usr/share/fonts/a.sold
";
        assert_eq!(
            libs_outside(maps, "/boot/BITSOS2/mesa/lib/"),
            vec!["/usr/lib/libpixman-1.so.0.46.4".to_string()]
        );
        assert!(libs_outside("7f-80 r-xp 0 00:1 2 /t/lib/libc.so.6\n", "/t/lib").is_empty());
        assert_eq!(libs_outside("7f-80 r-xp 0 00:1 2 /t/library/libc.so.6\n", "/t/lib").len(), 1);
    }

    #[test]
    fn a_missing_library_is_reported_not_fatal() {
        assert_eq!(probe("libbitscomp-wy14-not-there.so.0"), "missing");
        assert_eq!(probe("bad\0name"), "bad-name");
    }
}
