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
/// The CONFIG.TXT key: `bitscomp=rescue|gpu` (default rescue).
pub const KEY: &str = "bitscomp";
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
}

pub fn parse(v: &str) -> Option<Build> {
    match v.trim() {
        "rescue" => Some(Build::Rescue),
        "gpu" => Some(Build::Gpu),
        "gles" => Some(Build::Gles),
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
/// value says where the choice came from.
pub fn renderer_wanted(cli: Option<&str>, build: Option<&str>, config: Result<&str, &str>) -> (Renderer, String) {
    if let Some(v) = cli {
        return match Renderer::parse(v) {
            Some(r) => (r, format!("--renderer {v}")),
            None => (Renderer::Pixman, format!("--renderer {v:?} is neither pixman nor gles")),
        };
    }
    if let Some(b) = build.and_then(parse) {
        return if b == Build::Gles { (Renderer::Gles, "--build gles".into()) } else { (Renderer::Pixman, "default".into()) };
    }
    match config.ok().and_then(|t| config_value(t, KEY)).as_deref().and_then(parse) {
        Some(Build::Gles) => (Renderer::Gles, format!("CONFIG.TXT {KEY}=gles")),
        _ => (Renderer::Pixman, "default".into()),
    }
}

/// The arguments the rescue build adds when it hands over for GLES that its own command line did not ask for by
/// `--renderer` (CONFIG.TXT `bitscomp=gles`): the gpu build then needs no CONFIG.TXT of its own.
pub fn hand_over_args(build: Build, argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if build == Build::Gles && !argv.iter().any(|a| a == "--renderer") {
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
            None => (Build::Rescue, format!("--build {v:?} is neither rescue, gpu nor gles")),
        };
    }
    // WY-15: GLES exists only in the gpu build, so asking for it on the command line asks for that build.
    if renderer == Some("gles") {
        return (Build::Gles, "--renderer gles".into());
    }
    match config {
        Ok(text) => match config_value(text, KEY) {
            None => (Build::Rescue, "default".into()),
            Some(v) => match parse(&v) {
                // `--renderer pixman` on the command line keeps the gpu build of `bitscomp=gles` on Pixman.
                Some(Build::Gles) if renderer == Some("pixman") => (Build::Gpu, format!("CONFIG.TXT {KEY}={v}, --renderer pixman")),
                Some(b) => (b, format!("CONFIG.TXT {KEY}={v}")),
                None => (Build::Rescue, format!("CONFIG.TXT {KEY}={v:?} is neither rescue, gpu nor gles")),
            },
        },
        Err(why) => (Build::Rescue, format!("default (CONFIG.TXT: {why})")),
    }
}

/// Called by the rescue build before anything is opened (one thread): hands over to the gpu build when asked.
/// Returns only when the rescue build stays; `argv` without the program name.
pub fn hand_over(cli: Option<&str>, renderer: Option<&str>, gpu_exe: &str, config: &str, argv: &[String]) {
    if GPU {
        if cli == Some("rescue") {
            eprintln!("bitscomp: this is the gpu build: --build rescue is not followed (start /bin/bitscomp for the rescue build)");
        }
        return;
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
        return;
    }
    let (r, timeout) = if build == Build::Gles { (Renderer::Gles, CHECK_TIMEOUT_GLES) } else { (Renderer::Pixman, CHECK_TIMEOUT) };
    let label = if build == Build::Gles { "gpu, renderer gles" } else { "gpu" };
    if let Err(e) = check_gpu(gpu_exe, r, timeout) {
        eprintln!("bitscomp: build {label} ({why}): {gpu_exe} {CHECK_ARG}: {e}; the rescue build runs on");
        return;
    }
    eprintln!("bitscomp: build {label} ({why}): handing over to {gpu_exe}");
    // execve only returns on failure; the rescue build then runs on, as if nothing had been asked.
    let arg0 = std::env::args_os().next().unwrap_or_else(|| "bitscomp".into());
    let e = Command::new(gpu_exe).arg0(arg0).args(hand_over_args(build, argv)).exec();
    eprintln!("bitscomp: build gpu: {gpu_exe}: {e}; the rescue build runs on");
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
        assert_eq!(wanted(None, None, Ok("loglevel=info")), (Build::Rescue, "default".into()));
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
        // The gpu build's renderer.
        assert_eq!(renderer_wanted(None, None, Ok("")).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(Some("gles"), Some("gpu"), Ok("")).0, Renderer::Gles);
        assert_eq!(renderer_wanted(Some("pixman"), Some("gles"), Ok("bitscomp=gles")).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(Some("vulkan"), None, Ok("")).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, Some("gles"), Ok("")).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, Some("gpu"), Ok("bitscomp=gles")).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=gles\r\n")).0, Renderer::Gles);
        assert_eq!(renderer_wanted(None, None, Ok("bitscomp=gpu")).0, Renderer::Pixman);
        assert_eq!(renderer_wanted(None, None, Err("none")).0, Renderer::Pixman);
    }

    #[test]
    fn the_hand_over_carries_the_renderer() {
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(hand_over_args(Build::Gles, &a(&["--stats", "5"])), a(&["--stats", "5", "--renderer", "gles"]));
        assert_eq!(hand_over_args(Build::Gles, &a(&["--renderer", "gles"])), a(&["--renderer", "gles"]));
        assert_eq!(hand_over_args(Build::Gpu, &a(&["--stats", "5"])), a(&["--stats", "5"]));
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
