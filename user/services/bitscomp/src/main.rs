//! bitscomp: BitsOS's Wayland compositor (V2-PLAN §8a.2). WY-0: nested on CachyOS, software composition;
//! WY-1a: on V2 itself (`--backend fbdev`, the default without the `nested` feature).
//!
//! WY-4: never waits for a client (B4: `hung`, `answer`, `ending`, `veil`, the MCP tools in `mcp`).
//!
//! WY-5: the approved look (plan/spec/desktop.md): chrome, workspaces, start page, theme from settingsd, live.
//!
//! bitscomp [--backend winit|headless|fbdev] [--fb PATH] [--size WxH] [--socket NAME] [--stats SECS] [--run CMD]...
//!          [--mcp SOCKET|none] [--settings SOCKET|none] [--fixed-status] [--fb-pan|--fb-copy] [--no-grant] [--user UID[:GID]]
//!          [--pointer none|soft|fake] (WY-11, headless only: no pointer, the software one, a fake hardware cursor)
//!          [--installer SOCKET|none] (MC-9b: bitsinstall's confirmer socket; default /run/bitsos/install.sock)
//!          [--fx-on-software] (WY-19: the gpu effect level on a software rasteriser too: tests, measuring)
//!          [--scanout auto|direct|readback] (WY-17: frames straight into fbdev's windows, `backend::scanout`; auto: GLES with a render node; direct: without asking for one)
//!          [--fake-scanout ok|badfd|nostride|refuse|vsync] (WY-17, headless only: memfds stand in for fbdev's windows; WY-20 vsync: at 60 Hz)
//!          [--main-device auto|render] (WY-17b: what linux-dmabuf names; auto: the render node for a GPU renderer, none for one reading by CPU)
//!          [--watch-node PATH] (WY-21, a test's: the GPU watch looks at PATH instead of the render node)
//!          [--xwayland PATH|none] (WY-3: the Xwayland started by the first X11 client; default /boot/BITSOS2/xwayland/bin/Xwayland)
//!          [--xwayland-glamor auto|off|force:PATH] (WY-3b: auto: glamor and DRI3 when bitscomp draws on a render node that opens; force: a test's, glamor whatever draws, PATH watched as the node)
//!          [--build rescue|gpu] [--gpu-exe PATH] [--config PATH] (WY-14, `flavour`: hand over to the gpu build; default CONFIG.TXT
//!          `bitscomp=`, else rescue (WY-17: `bitscomp=auto`: gles when a render node is up and the check proves it); the gpu build at /boot/BITSOS2/bin/bitscomp-gpu)
//! bitscomp ctl [--socket NAME] COMMAND...
//! bitscomp exec PPID PROGRAM [ARG]... (WY-7, internal: how bitscomp starts a program, `spawn`)
//! bitscomp licenses (the Exo 2 font licence, OFL 1.1)
//! bitscomp check-build (WY-14, internal: the gpu build proves it loads and makes a renderer before the hand-over)
//! bitscomp page ADDRESS (WY-5b: the window of a settings page; bitscomp draws its content, `page_client`)

mod acquire;
mod address;
mod anim;
mod answer;
mod apply;
mod backend;
mod bench;
mod canvas;
mod chrome;
mod chrome_draw;
mod chrome_render;
mod clients;
mod ctl;
mod ctl_desk;
mod desktop_entry;
mod ctl_mouse;
mod cursor;
mod cursor_theme;
mod deco;
mod desk;
mod dmabuf;
mod drm_sync;
mod drop_root;
mod ending;
mod flavour;
mod foot;
mod gpu_watch;
mod grab;
mod grant;
mod handlers;
mod header;
mod hung;
mod cursor_surface;
mod hwcursor;
mod icon_png;
mod input;
mod access_app;
mod access_dialog;
mod install_app;
mod install_dialog;
mod install_draw;
mod install_link;
mod launch;
mod lock;
mod layout;
mod library;
mod library_draw;
mod library_ui;
mod library_view;
mod minimize;
mod pad;
mod pad_app;
mod mcp;
mod menu;
mod menu_draw;
mod menu_input;
mod pacer;
mod page;
mod page_app;
mod page_client;
mod page_confirm;
mod page_draw;
mod page_schema;
mod place;
mod pointer;
mod rundir;
mod paint;
// Used by the GLES renderer only; tested in every build.
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
mod readback;
mod release;
mod fx;
#[cfg(feature = "gpu")]
mod fx_gl;
mod render;
#[cfg(feature = "gpu")]
mod render_gl;
mod reveal;
mod ring;
mod session;
mod settings;
mod settings_write;
mod spawn;
mod spin;
mod spots;
mod start;
mod start_draw;
mod state;
mod stats;
mod status;
mod suggest;
mod syncobj;
mod tabs;
mod terminal;
mod text;
mod theme;
mod veil;
mod win;
mod win_alone;
mod wm;
mod xconn;
mod xproto;
mod xsel;
mod xsel_serve;
mod xwayland;
mod xwm;
mod zones;

use std::io::Write;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::wayland::compositor::CompositorClientState;
use smithay::wayland::socket::ListeningSocketSource;

use backend::Backend;
use state::App;

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    /// The socket duplicate of the epoll workaround (`clients`), shut down when the display drops the client.
    pub watch: Option<clients::Watch>,
    /// SO_PEERCRED's pid at accept (0: unknown).
    pub pid: i32,
    /// That pid's start time at accept (/proc/PID/stat field 22), so `ending` never signals a reused pid.
    pub start: Option<u64>,
    /// bitscomp dropped it on purpose (`ending`), so its end is no surprise worth a line.
    pub ended_by_us: AtomicBool,
}

/// B4: what bitscomp queues for one client that does not read. wayland-backend 0.3.17's rs server writes with
/// MSG_DONTWAIT|MSG_NOSIGNAL (rs/socket.rs send_msg) and keeps the rest in a per-client buffer of at most this
/// many bytes (rs/mod.rs DEFAULT_MAX_BUFFER_SIZE, as libwayland's default); a message that finds it full drops
/// the client (rs/server_impl/client.rs: write_message error -> kill). On top of the socket's own send buffer
/// (SO_SNDBUF, about 208 KiB on Linux and V2). Changing it needs wayland-server's `libwayland_1_23` feature,
/// which pulls in wayland-sys: not worth it, since a hung client gets almost nothing (no frame callbacks,
/// input or configures), so this only ends a client that stops reading while it is still being sent to.
pub const CLIENT_QUEUE_BYTES: usize = 4096;

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        if let Some(w) = &self.watch {
            // B6: a drop the client did not cause by closing its socket.
            if !self.ended_by_us.load(Ordering::Relaxed) {
                match &reason {
                    DisconnectReason::ProtocolError(e) => {
                        eprintln!("bitscomp: client pid {} dropped: protocol error {} on {}: {}", self.pid, e.code, e.object_interface, e.message)
                    }
                    DisconnectReason::ConnectionClosed if !clients::peer_closed(w) => eprintln!(
                        "bitscomp: client pid {} dropped: its output queue ({} KiB) overflowed (it does not read its socket) or it sent a malformed request",
                        self.pid,
                        CLIENT_QUEUE_BYTES / 1024
                    ),
                    DisconnectReason::ConnectionClosed => {}
                }
            }
            clients::hang_up(w);
        }
    }
}

/// How often the loop looks whether a late frame's GPU work has ended (`render_gl::GPU_WAIT` ran out).
const GPU_POLL: Duration = Duration::from_millis(20);

struct Args {
    backend: String,
    fb: String,
    size: (i32, i32),
    socket: String,
    stats_every: Option<u64>,
    run: Vec<String>,
    mcp: String,
    settings: String,
    fixed_status: bool,
    /// `--main-device render` (`dmabuf::advertised`).
    main_render: bool,
    /// WY-17: `--scanout` and `--fake-scanout`.
    scanout: backend::scanout::Want,
    fake_scanout: Option<String>,
    /// WY-19: `--fx-on-software`: the gpu effect level on a software rasteriser too (tests, measuring).
    fx_on_software: bool,
    /// WY-8 (GX-3 CR 2): fbdev draws two windows shown by a pan (`--fb-pan`) instead of one whose damage the
    /// kernel copies (`--fb-copy`, the default until the Mini-PC has compared both: QEMU timing decides nothing).
    fb_pan: bool,
    grant: bool,
    /// WY-7: whom a root bitscomp on fbdev runs as once its devices are open (`drop_root`; 0 keeps root).
    user: (u32, u32),
    /// WY-11, headless: `none` (default), `soft` or `fake` (`backend::headless`).
    pointer: String,
    /// MC-9b: the installer's confirmer socket (`install_link`), or `none`.
    installer: String,
    /// WY-14: `--build rescue|gpu` (`flavour`; unset: CONFIG.TXT `bitscomp=` decides) and where the gpu build is.
    build: Option<String>,
    gpu_exe: String,
    /// `--config PATH`: the CONFIG.TXT to read `bitscomp=` from (host tests; default the stick's).
    config: String,
    /// WY-15: `--renderer pixman|gles` (unset: `--build gles` or CONFIG.TXT `bitscomp=gles` decide; `flavour`).
    renderer: Option<String>,
    /// WY-3: `--xwayland PATH|none`: the Xwayland started on demand (none: no X11 display).
    xwayland: String,
    /// WY-3b: `--xwayland-glamor`.
    xwayland_glamor: xwayland::Glamor,
    /// WY-21: `--watch-node PATH`.
    watch_node: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        backend: if cfg!(feature = "nested") { "winit".into() } else { "fbdev".into() },
        fb: "/dev/fb0".into(),
        size: (1600, 1000),
        socket: "wayland-bits-0".into(),
        stats_every: None,
        run: Vec::new(),
        mcp: mcpreg::DEFAULT_SOCKET.into(),
        settings: std::env::var("SETTINGS_SOCKET").unwrap_or_else(|_| settings::SOCKET.into()),
        fixed_status: false,
        main_render: false,
        scanout: backend::scanout::Want::Auto,
        fake_scanout: None,
        fx_on_software: false,
        fb_pan: false,
        grant: true,
        user: drop_root::DESKTOP,
        pointer: "none".into(),
        installer: install_link::SOCKET.into(),
        build: None,
        gpu_exe: flavour::GPU_EXE.into(),
        config: flavour::CONFIG.into(),
        renderer: None,
        xwayland: xwayland::DEFAULT_EXE.into(),
        xwayland_glamor: xwayland::Glamor::Auto,
        watch_node: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--backend" => a.backend = val()?,
            "--fb" => a.fb = val()?,
            "--socket" => a.socket = val()?,
            "--size" => {
                let v = val()?;
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                a.size = (w.parse().map_err(|_| "--size WxH")?, h.parse().map_err(|_| "--size WxH")?);
            }
            "--fx-on-software" => a.fx_on_software = true,
            "--stats" => a.stats_every = Some(val()?.parse().map_err(|_| "--stats SECS")?),
            "--run" => a.run.push(val()?),
            "--mcp" => a.mcp = val()?,
            "--no-grant" => a.grant = false,
            "--settings" => a.settings = val()?,
            "--installer" => a.installer = val()?,
            "--xwayland" => a.xwayland = val()?,
            "--watch-node" => a.watch_node = Some(val()?),
            "--xwayland-glamor" => a.xwayland_glamor = xwayland::Glamor::parse(&val()?).ok_or("--xwayland-glamor auto|off|force:PATH")?,
            "--build" => a.build = Some(val()?),
            "--gpu-exe" => a.gpu_exe = val()?,
            "--config" => a.config = val()?,
            "--renderer" => {
                let v = val()?;
                flavour::Renderer::parse(&v).ok_or("--renderer pixman|gles")?;
                a.renderer = Some(v);
            }
            "--fixed-status" => a.fixed_status = true,
            "--main-device" => {
                a.main_render = match val()?.as_str() {
                    "auto" => false,
                    "render" => true,
                    _ => return Err("--main-device auto|render".into()),
                }
            }
            "--scanout" => {
                let v = val()?;
                a.scanout = backend::scanout::Want::parse(&v).ok_or("--scanout auto|direct|readback")?;
            }
            "--fake-scanout" => {
                let v = val()?;
                if !matches!(v.as_str(), "ok" | "badfd" | "nostride" | "refuse" | "vsync") {
                    return Err("--fake-scanout ok|badfd|nostride|refuse|vsync".into());
                }
                a.fake_scanout = Some(v);
            }
            "--fb-pan" => a.fb_pan = true,
            "--fb-copy" => a.fb_pan = false,
            "--user" => a.user = drop_root::parse_user(&val()?)?,
            "--pointer" => {
                a.pointer = val()?;
                if !["none", "soft", "fake"].contains(&a.pointer.as_str()) {
                    return Err("--pointer none|soft|fake".into());
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() {
    flavour::set_comm();
    flavour::take_rescue();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some(flavour::CHECK_ARG) {
        std::process::exit(check_build(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("licenses") {
        print!("The interface fonts built into bitscomp (SIL Open Font License 1.1):\n\n{}", text::LICENCE);
        return;
    }
    if argv.first().map(String::as_str) == Some("exec") {
        std::process::exit(spawn::exec_child(&argv[1..]));
    }
    if argv.first().map(String::as_str) == Some("page") {
        std::process::exit(page_client::run(argv.get(1).map_or("einstellungen", String::as_str)));
    }
    if argv.first().map(String::as_str) == Some("ctl") {
        let (socket, rest) = match argv.get(1).map(String::as_str) {
            Some("--socket") => (argv.get(2).cloned().unwrap_or_default(), &argv[3.min(argv.len())..]),
            _ => ("wayland-bits-0".to_string(), &argv[1..]),
        };
        match ctl::send(&socket, &rest.join(" ")) {
            Ok(out) => print!("{out}"),
            Err(e) => {
                eprintln!("bitscomp ctl: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let args = parse_args(&argv).unwrap_or_else(|e| {
        eprintln!("bitscomp: {e}");
        std::process::exit(2);
    });
    // WY-14: before anything is opened, while bitscomp has one thread.
    // WY-21: Some when the gpu build it handed over to gave the GPU up or crashed: this rescue build takes over.
    let takeover = flavour::hand_over(args.build.as_deref(), args.renderer.as_deref(), &args.gpu_exe, &args.config, &argv);
    let renderer = if takeover.is_some() { flavour::Renderer::Pixman } else { choose_renderer(&args) };
    if let Err(e) = run(args, renderer, takeover) {
        eprintln!("bitscomp: {e}");
        std::process::exit(1);
    }
}

/// WY-15: the renderer this process draws with: Pixman in the rescue build; in the gpu build GLES when chosen, its
/// EGL variables set now, while bitscomp has one thread.
fn choose_renderer(args: &Args) -> flavour::Renderer {
    if !flavour::GPU {
        return flavour::Renderer::Pixman;
    }
    let config = std::fs::read_to_string(&args.config).map_err(|_| "unreadable");
    let node = dmabuf::render_device().1.is_some();
    let (r, why) = flavour::renderer_wanted(args.renderer.as_deref(), args.build.as_deref(), config.as_deref().map_err(|e| *e), node);
    if why != "default" {
        eprintln!("bitscomp: renderer {} ({why})", r.name());
    }
    #[cfg(feature = "gpu")]
    if r == flavour::Renderer::Gles {
        match flavour::tree_root() {
            Some(tree) => {
                let (software, gl_why) = flavour::gl_software(&args.config);
                eprintln!("bitscomp: renderer gles: {} ({gl_why})", if software { "the software rasteriser" } else { "the GPU" });
                render_gl::set_env(tree, software);
            }
            None => {
                eprintln!("bitscomp: renderer gles: this gpu build names no Mesa tree; drawing with pixman");
                return flavour::Renderer::Pixman;
            }
        }
    }
    r
}

/// `check-build [--renderer pixman|gles]` (`flavour::check_build`).
fn check_build(args: &[String]) -> i32 {
    match flavour::check_renderer(args) {
        Ok(flavour::Renderer::Pixman) => flavour::check_build::<render::DefaultRenderer>(),
        #[cfg(feature = "gpu")]
        Ok(flavour::Renderer::Gles) => match flavour::tree_root() {
            Some(tree) => {
                render_gl::set_env(tree, flavour::gl_software(flavour::CONFIG).0);
                flavour::check_build::<smithay::backend::renderer::gles::GlesRenderer>()
            }
            None => {
                eprintln!("bitscomp check-build: build=gpu: no Mesa tree named at build time");
                1
            }
        },
        #[cfg(not(feature = "gpu"))]
        Ok(flavour::Renderer::Gles) => {
            eprintln!("bitscomp check-build: build=rescue: no gles renderer in the rescue build");
            1
        }
        Err(e) => {
            eprintln!("bitscomp check-build: {e}");
            2
        }
    }
}

fn run(args: Args, renderer: flavour::Renderer, takeover: Option<i64>) -> Result<(), String> {
    // WY-7: a root start (the console) runs as the desktop user once its devices are open (`drop_root`).
    let drop_to = drop_root::plan(drop_root::euid(), &args.backend, args.user);
    // Before any thread exists (set_var); made and checked after the drop, as the user.
    let rundir = rundir::select(drop_to.map(|(uid, _)| uid));
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    // A plain kill ends like Strg+Alt+Q, so the socket files and the started programs are cleaned up.
    // Blocked here, before any back end starts threads, so every thread inherits the mask.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP]).map_err(|e| e.to_string())?;
    event_loop.handle().insert_source(signals, |_, _, app| app.quit = true).map_err(|e| e.to_string())?;
    let mut backend: Box<dyn Backend> = match args.backend.as_str() {
        #[cfg(feature = "nested")]
        "winit" => Box::new(backend::winit::Winit::new(args.size)?),
        "headless" => Box::new(backend::headless::Headless::new(args.size, &args.pointer, args.fake_scanout.as_deref())),
        "fbdev" => Box::new(backend::fbdev::Fbdev::new(&args.fb, args.fb_pan, &event_loop.handle())?),
        other => return Err(format!("unknown back end {other}")),
    };
    // The grant needs the screen's holder, which the back end just became (and root's fb0 open, when dropping).
    let grant = (args.grant && args.backend == "fbdev").then(|| grant::Grant::request(&args.fb)).flatten();
    if let Some((uid, gid)) = drop_to {
        // Still root: a stale root-owned lock or font cache of an older root run can be probed and cleared.
        rundir::clear_foreign(&rundir, &args.socket, uid);
        drop_root::drop_to(uid, gid).map_err(|e| format!("giving up root for uid {uid}: {e} (--user 0 runs as root on purpose)"))?;
        user_env(uid);
        eprintln!("bitscomp: devices open, now running as uid {uid} gid {gid}");
        flavour::rearm_death_signal();
    }
    rundir::ensure(&rundir)?;

    let mut display: Display<App> = Display::new().map_err(|e| e.to_string())?;
    let dh = display.handle();
    let socket = ListeningSocketSource::with_name(&args.socket).map_err(|e| format!("socket {}: {e}", args.socket))?;
    let handle = event_loop.handle();
    let h2 = handle.clone();
    handle
        .insert_source(socket, move |stream, _, app| {
            let watch = clients::duplicate(&stream);
            let pid = clients::peer_pid(&stream);
            let start = (pid > 0).then(|| ending::start_time(pid)).flatten();
            let data = ClientState { watch: watch.clone(), pid, start, ..Default::default() };
            if let Ok(client) = app.dh.insert_client(stream, Arc::new(data))
                && let Some(w) = watch
            {
                clients::watch(&h2, w, client.id());
            }
        })
        .map_err(|e| e.to_string())?;
    // The display's own epoll wakes the loop too; the clients are dispatched every turn (`clients`).
    let display_fd = display.backend().poll_fd().try_clone_to_owned().map_err(|e| format!("display fd: {e}"))?;
    handle
        .insert_source(Generic::new(display_fd, Interest::READ, Mode::Level), |_, _, app| {
            app.woke.display += 1;
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    if let Some(fd) = backend.wake_fd() {
        handle
            .insert_source(Generic::new(fd, Interest::READ, Mode::Level), |_, _, _| Ok(PostAction::Continue))
            .map_err(|e| e.to_string())?;
    }
    let ctl_path = ctl::listen(&handle, &args.socket).map_err(|e| format!("control socket: {e}"))?;
    let mut pacer = pacer::Pacer::new()?;
    let timer = pacer.fd().try_clone_to_owned().map_err(|e| format!("timerfd: {e}"))?;
    handle
        .insert_source(Generic::new(timer, Interest::READ, Mode::Level), |_, fd, app| {
            app.woke.timer += 1;
            pacer::drain(fd.as_fd());
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;

    let mut app = App::new(dh, args.socket.clone(), backend.size());
    app.fx.on_software = args.fx_on_software;
    app.grant = grant;
    if args.mcp != "none" {
        mcp::start(&handle, args.mcp.clone())?;
    }
    if args.fixed_status {
        app.status = status::Status::fixed();
    }
    app.headless = args.backend == "headless";
    if args.installer != "none" {
        // Only the real socket's peer must be the boot's /bin/bitsinstall as root; another path is a test's.
        let check = args.installer == install_link::SOCKET;
        if !check {
            eprintln!("bitscomp: installer socket {} (a test socket: its peer is not checked)", args.installer);
        }
        install_link::start(&handle, args.installer.clone(), check)?;
    }
    if args.settings != "none" {
        settings::start(&handle, args.settings.clone())?;
        match settings_write::start(&handle, args.settings.clone()) {
            Ok(w) => app.settings_writer = Some(w),
            Err(e) => eprintln!("bitscomp: {e} (the settings page shows values only)"),
        }
    }
    session::start(&handle, &mut app, &rundir)?;
    // PK-L1a: the program index (its own thread; a failure leaves the built-in programs).
    if let Err(e) = library::start(&handle, library::Roots::from_env(), |app: &mut App, ix| {
        app.library = ix;
        app.chrome_dirty();
    }) {
        eprintln!("bitscomp: library: {e} (only the built-in programs)");
    }
    // WY-21: the gpu build's session comes back as it was saved, without the start page's offer.
    app.session.restore_since = takeover;
    app.xwayland = xwayland::setup(&handle, &args.xwayland, args.xwayland_glamor.clone());
    app.draw_pointer = backend.draws_pointer();
    // WY-11: the hardware cursor, if the back end has one; the first image tells whether it works.
    app.cursor.hw = if app.draw_pointer { backend.hw_cursor() } else { None };
    app.cursor_sync();
    app.spawn_env = terminal::prepare(&rundir);
    terminal::cursor_env(|k| std::env::var(k).ok(), &mut app.spawn_env, app.cursor.size);
    app.foot.path = Some(foot::path(&rundir));
    app.foot_sync();
    if !app.spawn_env.is_empty() {
        let set: Vec<String> = app.spawn_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("bitscomp: started programs also get {}", set.join(" "));
    }
    let (mut period, mut phase) = backend.pacing().unwrap_or((pacer::DEFAULT_PERIOD, Duration::ZERO));
    app.refresh = period;
    let mut comp = render::make(&app, renderer)?;
    backend::scanout::start(&mut *backend, &mut *comp, args.scanout, dmabuf::render_device().1.as_deref());
    app.dma.main_render = args.main_render;
    dmabuf::init(&mut app, &*comp, handle.clone());
    let node = args.watch_node.clone().or(dmabuf::render_device().1);
    let mut gpu = GpuSide::new(&*comp, node);
    println!(
        "WAYLAND_DISPLAY={} XDG_RUNTIME_DIR={} control={} pid={} backend={} size={}x{} refresh_us={} build={} renderer={} egl={}",
        args.socket,
        rundir.display(),
        ctl_path.display(),
        std::process::id(),
        args.backend,
        backend.size().0,
        backend.size().1,
        period.as_micros(),
        flavour::name(),
        comp.renderer(),
        flavour::egl_probe()
    );
    let _ = std::io::stdout().flush();
    for cmd in &args.run {
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        app.spawn(&argv);
    }

    let mut next_frame = pacer::now();
    let mut next_stats = Instant::now();
    let mut bench_run: Option<bench::Run> = None;
    let mut bench_due: Option<Duration> = None;
    let mut guard = spin::Guard::default();
    let mut missed = clients::Missed::default();
    let mut gpu_late = false;
    while !app.quit {
        let busy = app.needs_frame || app.layout_dirty;
        // Asleep until a client, an input device, the control socket or the frame timer wakes us, or a ping, a
        // "hung" deadline or a SIGKILL is due; the 1 s cap reaps children.
        let mono_now = pacer::now();
        gpu.turn(mono_now);
        let busy = busy || app.anim.running(mono_now, &app.theme);
        let wake = spin::earliest(&[
            (busy.then_some(next_frame), "frame"),
            (bench_due, "bench"),
            (app.answer_due(), "answer"),
            (app.ending_due(), "ending"),
            (app.status.due(), "status"),
            (app.session.due, "session"),
            (app.notice_tick(mono_now), "notice"),
            (app.veil_next, "veil"),
            (app.repeat_due(), "repeat"),
            (app.pad_due(), "pad"),
            (app.desk.reveal.due(), "reveal"),
            (app.launches.due(), "launches"),
            (app.install_due(), "install"),
            (app.fx.due(), "fx"),
            (gpu_late.then_some(mono_now + GPU_POLL), "gpu"),
        ]);
        if let Some(line) = guard.turn(mono_now, wake, app.stats.frames, app.woke, &mut app.stats) {
            eprintln!("bitscomp: {line}");
        }
        if pacer.arm(wake.map(|(t, _)| t)) {
            app.stats.timer_sets += 1;
        }
        event_loop.dispatch(Some(Duration::from_secs(1)), &mut app).map_err(|e| e.to_string())?;
        if let Err(e) = display.dispatch_clients(&mut app) {
            eprintln!("bitscomp: dispatching clients: {e}");
        }
        clients::read_missed(&mut display, &mut app, &mut missed, mono_now);
        // WY-16: new dma-bufs checked by the renderer, answered before this turn's flush.
        comp.dmabuf_turn(&mut app);
        backend.dispatch(&mut app);
        bench::take(&mut app, &*comp, &mut bench_run);
        bench_due = bench::step(&mut app, &*comp, backend.as_mut(), &mut bench_run, period);
        ctl::run_queue(&mut app, &mut *comp);
        app.lock_sync();
        // WY-11: a new shape (a client's set_cursor, a commit of its cursor surface) goes to the cursor now.
        app.cursor_sync();
        if app.layout_dirty {
            app.apply_layout();
        }
        // WY-3: the X11 windows' places, focus and closes go to Xwayland before the frame.
        xwayland::turn(&mut app);
        let now = Instant::now();
        let mono = pacer::now();
        if app.answer_tick(mono) {
            // WY-13: the ping leaves now, not after a render that may take long (0.67 s were seen).
            let _ = app.dh.flush_clients();
        }
        app.ending_tick(mono);
        if app.status.tick(mono) {
            app.chrome_dirty();
        }
        app.session_tick(mono);
        app.notice_tick(mono);
        app.repeat_tick(mono);
        app.pad_tick(mono);
        app.reveal_tick(mono);
        if app.launches.tick(mono) {
            app.needs_frame = true;
        }
        app.install_tick(mono);
        if app.fx.tick(mono) {
            app.needs_frame = true;
        }
        // A hung window's veil counts its seconds; a "Warten" runs out.
        if app.veil_next.is_some_and(|d| mono >= d) {
            app.needs_frame = true;
        }
        app.veil_next = app.veil_due(mono);
        if app.anim.running(mono, &app.theme) {
            app.needs_frame = true;
        }
        if app.needs_frame && mono >= next_frame {
            app.needs_frame = false;
            // After an idle stretch the frame is drawn at once, with its true time.
            app.frame_time = pacer::frame_time(next_frame, mono, period);
            // WY-17: into the hidden scanout window when frames go there (may wait for the last flip).
            gpu.waiting(true);
            let aim = backend.scanout_frame();
            gpu.waiting(false);
            if !comp.aim(&app, aim) {
                backend.scanout_lost("the picture lost its windows");
                comp.aim(&app, None);
            }
            let damage = comp.render(&mut app);
            // WY-21: the frame callbacks and buffer releases leave before a present that waits for the flip. Sent
            // after it, a client heard of the frame a refresh late, its next buffer missed the frame drawn right after
            // the flip, and on a desktop that draws every refresh it got every second one: 30 a second (glxgears through
            // Xwayland on the Mini-PC, wy/86).
            let _ = app.dh.flush_clients();
            gpu.waiting(true);
            // WY-21: `ctl stall`: this present waits as one stuck in the driver would.
            if let Some(d) = app.stall.take() {
                std::thread::sleep(d);
            }
            if !damage.is_empty() {
                let t0 = Instant::now();
                backend.present(&*comp, &damage);
                app.stats.present += t0.elapsed();
            }
            gpu.waiting(false);
            if backend.scanout_left() {
                comp.aim(&app, None);
                app.needs_frame = true;
            }
            if let Some((p, ph)) = backend.pacing() {
                (period, phase) = (p, ph);
                app.refresh = p;
            }
            next_frame = pacer::next_frame(app.frame_time, pacer::now(), period, phase);
            backend.dispatch(&mut app);
        }
        let t0 = Instant::now();
        gpu.waiting(true);
        backend.flush(&*comp);
        gpu.waiting(false);
        app.stats.present += t0.elapsed();
        app.space.refresh();
        app.popups.cleanup();
        app.reap_children();
        app.prune_pending();
        // WY-16: after the frame (its draw has finished) the replaced buffers go back.
        app.dma.gpu_busy = !comp.gpu_idle();
        // WY-19: a frame that outlasted `GPU_WAIT` was neither read back nor flipped: once the GPU is done the next
        // frame shows it, even when nothing else asks for one (a first blur on a slow rasteriser).
        if gpu_late && !app.dma.gpu_busy {
            app.needs_frame = true;
        }
        gpu_late = app.dma.gpu_busy;
        if let Some(why) = gpu.lost(&mut *comp, gpu_late, pacer::now()) {
            leave_gpu(&mut app, &mut comp, backend.as_mut(), &mut gpu, &why)?;
            gpu_late = false;
        }
        app.give_back();
        let _ = app.dh.flush_clients();
        if let Some(every) = args.stats_every
            && now >= next_stats
        {
            eprintln!("bitscomp stats: {}", app.stats.report());
            next_stats = now + Duration::from_secs(every);
        }
    }
    drop(gpu);
    let _ = std::fs::remove_file(&ctl_path);
    end_children(&mut app.children);
    // WY-11: the hardware cursor off while bitscomp still holds the screen.
    app.cursor.shutdown();
    // The fbdev back end unmaps and gives the console back here.
    drop(backend);
    eprintln!("bitscomp: ended");
    Ok(())
}

/// WY-21: what the loop knows of the GPU it draws on (`gpu_watch`). Nothing for a renderer that draws by CPU.
struct GpuSide {
    watch: Option<gpu_watch::Watch>,
    node: Option<String>,
    late_since: Option<Duration>,
    next_reset_look: Duration,
    /// GLES renderers made after a reset the device lived through: the next one is Pixman.
    rebuilds: u32,
}

/// How many times a reset the device lived through gets a new GLES renderer before Pixman draws for good.
const GPU_REBUILDS: u32 = 2;

impl GpuSide {
    fn new(comp: &dyn render::Composer, node: Option<String>) -> GpuSide {
        let on_gpu = comp.renderer() == "gles";
        let watch = node.as_deref().filter(|_| on_gpu).and_then(gpu_watch::Watch::start);
        GpuSide { watch, node, late_since: None, next_reset_look: Duration::ZERO, rebuilds: 0 }
    }

    fn turn(&self, now: Duration) {
        if let Some(w) = &self.watch {
            w.turn(now);
        }
    }

    /// A wait on the display (the last flip, a present, a flush) begins or ends: `gpu_watch::STALL` counts only these.
    fn waiting(&self, on: bool) {
        if let Some(w) = &self.watch {
            w.waiting(on.then(pacer::now));
        }
    }

    /// Why this renderer cannot draw on, once one of the signs says so.
    fn lost(&mut self, comp: &mut dyn render::Composer, late: bool, now: Duration) -> Option<String> {
        if comp.renderer() != "gles" {
            return None;
        }
        self.late_since = if late { self.late_since.or(Some(now)) } else { None };
        if let Some(why) = self.watch.as_ref().and_then(gpu_watch::Watch::lost) {
            return Some(why);
        }
        if gpu_watch::late(self.late_since, now) {
            return Some(format!("the GPU has not finished a frame for {} s", gpu_watch::LATE.as_secs()));
        }
        if now >= self.next_reset_look {
            self.next_reset_look = now + gpu_watch::LOOK_EVERY;
            return comp.gpu_reset();
        }
        None
    }
}

/// WY-21: the renderer on a lost GPU is replaced in this process, so every client keeps its window: GLES again when the
/// device lived through a reset (its node opens, the GL context alone was lost), else Pixman for good. The old compositor
/// is never dropped: its teardown would wait on a driver that may not answer. A GPU that a frame waits on for ever is
/// `gpu_watch`'s stall, not this.
fn leave_gpu(app: &mut App, comp: &mut Box<dyn render::Composer>, backend: &mut dyn Backend, gpu: &mut GpuSide, why: &str) -> Result<(), String> {
    let recovered = why.starts_with("the GL context")
        && gpu.rebuilds < GPU_REBUILDS
        && gpu.node.as_deref().is_some_and(|n| gpu_watch::look_within(n, Duration::from_secs(1)) == gpu_watch::Look::Opens);
    let next = if recovered { flavour::Renderer::Gles } else { flavour::Renderer::Pixman };
    eprintln!("bitscomp: gpu: {why}: the desktop goes on with {}, the windows stay", if recovered { "a new gles renderer" } else { "pixman" });
    backend.scanout_lost("the GPU is lost");
    let _ = backend.scanout_left();
    let fresh = render::make(app, next)?;
    std::mem::forget(std::mem::replace(comp, fresh));
    comp.aim(app, None);
    if comp.renderer() == "gles" {
        gpu.rebuilds += 1;
    } else {
        gpu.watch = None;
    }
    gpu.late_since = None;
    app.dma.gpu_busy = false;
    dmabuf::renew(app, &**comp);
    app.needs_frame = true;
    eprintln!("bitscomp: gpu: renderer={} from now on", comp.renderer());
    Ok(())
}

/// HOME, USER and LOGNAME of the user a root start dropped to (passwd, else HOME=/ as V2 gives uid 1000), so the
/// started programs and fontconfig do not look into root's home. Called while bitscomp has one thread.
fn user_env(uid: u32) {
    let (name, home) = std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|t| drop_root::passwd_entry(&t, uid))
        .unwrap_or_else(|| (String::new(), "/".into()));
    // SAFETY: called before the event loop, session, settings or MCP threads exist; no thread reads the
    // environment concurrently.
    unsafe {
        std::env::set_var("HOME", &home);
        if !name.is_empty() {
            std::env::set_var("USER", &name);
            std::env::set_var("LOGNAME", &name);
        }
    }
}

/// Started programs end with bitscomp: SIGTERM first (foot then hangs up its shell and exits cleanly), SIGKILL
/// for what still runs after one second, so a hung program cannot hold the end of bitscomp (B4).
fn end_children(children: &mut Vec<(std::process::Child, Instant)>) {
    signal_groups(children, libc::SIGTERM);
    if wait_bounded(children, Duration::from_secs(1)) {
        return;
    }
    for (c, _) in children.iter() {
        eprintln!("bitscomp: pid {} did not end on SIGTERM within 1 s: SIGKILL to its group", c.id());
    }
    signal_groups(children, libc::SIGKILL);
    if !wait_bounded(children, Duration::from_secs(1)) {
        // A process stuck in the kernel does not hold bitscomp's end (B4); it stays a zombie until bitscomp is gone.
        let left: Vec<u32> = children.iter().map(|(c, _)| c.id()).collect();
        eprintln!("bitscomp: pids {left:?} still there 1 s after SIGKILL: not waited for");
    }
}

/// Each started program leads its own process group (wm.rs spawn), so the negative pid reaches the group.
fn signal_groups(children: &[(std::process::Child, Instant)], sig: libc::c_int) {
    for (c, _) in children {
        // SAFETY: kill(2) on the process group of our own unreaped child (its pid is still ours, and as group
        // leader it keeps the pgid alive); no memory is touched. ESRCH for an already empty group is ignored.
        unsafe { libc::kill(-(c.id() as libc::pid_t), sig) };
    }
}

/// Reaps until all are gone or `limit` has passed; true when none is left.
fn wait_bounded(children: &mut Vec<(std::process::Child, Instant)>, limit: Duration) -> bool {
    let end = Instant::now() + limit;
    loop {
        children.retain_mut(|(c, _)| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
        if children.is_empty() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
