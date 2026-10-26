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
//! bitscomp ctl [--socket NAME] COMMAND...
//! bitscomp exec PPID PROGRAM [ARG]... (WY-7, internal: how bitscomp starts a program, `spawn`)
//! bitscomp licenses (the Exo 2 font licence, OFL 1.1)
//! bitscomp page ADDRESS (WY-5b: the window of a settings page; bitscomp draws its content, `page_client`)

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
mod ctl_mouse;
mod cursor;
mod cursor_theme;
mod deco;
mod desk;
mod drop_root;
mod ending;
mod foot;
mod grab;
mod grant;
mod handlers;
mod header;
mod hung;
mod cursor_surface;
mod hwcursor;
mod input;
mod launch;
mod layout;
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
mod render;
mod reveal;
mod ring;
mod session;
mod settings;
mod settings_write;
mod spawn;
mod spots;
mod start;
mod start_draw;
mod state;
mod stats;
mod status;
mod suggest;
mod tabs;
mod terminal;
mod text;
mod theme;
mod veil;
mod wm;
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
use render::Compositor;
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
    /// WY-8 (GX-3 CR 2): fbdev draws two windows shown by a pan (`--fb-pan`) instead of one whose damage the
    /// kernel copies (`--fb-copy`, the default until the Mini-PC has compared both: QEMU timing decides nothing).
    fb_pan: bool,
    grant: bool,
    /// WY-7: whom a root bitscomp on fbdev runs as once its devices are open (`drop_root`; 0 keeps root).
    user: (u32, u32),
    /// WY-11, headless: `none` (default), `soft` or `fake` (`backend::headless`).
    pointer: String,
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
        fb_pan: false,
        grant: true,
        user: drop_root::DESKTOP,
        pointer: "none".into(),
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
            "--stats" => a.stats_every = Some(val()?.parse().map_err(|_| "--stats SECS")?),
            "--run" => a.run.push(val()?),
            "--mcp" => a.mcp = val()?,
            "--no-grant" => a.grant = false,
            "--settings" => a.settings = val()?,
            "--fixed-status" => a.fixed_status = true,
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
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("licenses") {
        print!("Exo 2 (the interface font, built into bitscomp):\n\n{}", text::LICENCE);
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
    if let Err(e) = run(args) {
        eprintln!("bitscomp: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
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
        "headless" => Box::new(backend::headless::Headless::new(args.size, &args.pointer)),
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
        .insert_source(Generic::new(display_fd, Interest::READ, Mode::Level), |_, _, _| Ok(PostAction::Continue))
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
        .insert_source(Generic::new(timer, Interest::READ, Mode::Level), |_, fd, _| {
            pacer::drain(fd.as_fd());
            Ok(PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;

    let mut app = App::new(dh, args.socket.clone(), backend.size());
    app.grant = grant;
    if args.mcp != "none" {
        mcp::start(&handle, args.mcp.clone())?;
    }
    if args.fixed_status {
        app.status = status::Status::fixed();
    }
    if args.settings != "none" {
        settings::start(&handle, args.settings.clone())?;
        match settings_write::start(&handle, args.settings.clone()) {
            Ok(w) => app.settings_writer = Some(w),
            Err(e) => eprintln!("bitscomp: {e} (the settings page shows values only)"),
        }
    }
    session::start(&handle, &mut app, &rundir)?;
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
    let mut comp = Compositor::new(&app);
    println!(
        "WAYLAND_DISPLAY={} XDG_RUNTIME_DIR={} control={} pid={} backend={} size={}x{} refresh_us={}",
        args.socket,
        rundir.display(),
        ctl_path.display(),
        std::process::id(),
        args.backend,
        backend.size().0,
        backend.size().1,
        period.as_micros()
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
    while !app.quit {
        let busy = app.needs_frame || app.layout_dirty;
        // Asleep until a client, an input device, the control socket or the frame timer wakes us, or a ping, a
        // "hung" deadline or a SIGKILL is due; the 1 s cap reaps children.
        let mono_now = pacer::now();
        let busy = busy || app.anim.running(mono_now, &app.theme);
        let desk_due =
            [app.status.due(), app.session.due, app.notice_tick(mono_now), app.veil_next, app.repeat_due(), app.desk.reveal.due(), app.launches.due()];
        let wake = [busy.then_some(next_frame), bench_due, app.answer_due(), app.ending_due()]
            .into_iter()
            .chain(desk_due)
            .flatten()
            .min();
        pacer.arm(wake);
        event_loop.dispatch(Some(Duration::from_secs(1)), &mut app).map_err(|e| e.to_string())?;
        if let Err(e) = display.dispatch_clients(&mut app) {
            eprintln!("bitscomp: dispatching clients: {e}");
        }
        backend.dispatch(&mut app);
        bench::take(&mut app, &comp, &mut bench_run);
        bench_due = bench::step(&mut app, &comp, backend.as_mut(), &mut bench_run, period);
        ctl::run_queue(&mut app, &comp);
        // WY-11: a new shape (a client's set_cursor, a commit of its cursor surface) goes to the cursor now.
        app.cursor_sync();
        if app.layout_dirty {
            app.apply_layout();
        }
        let now = Instant::now();
        let mono = pacer::now();
        app.answer_tick(mono);
        app.ending_tick(mono);
        if app.status.tick(mono) {
            app.chrome_dirty();
        }
        app.session_tick(mono);
        app.notice_tick(mono);
        app.repeat_tick(mono);
        app.reveal_tick(mono);
        if app.launches.tick(mono) {
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
            app.frame_time = if mono > next_frame + period { mono } else { next_frame };
            let damage = comp.render(&mut app);
            if !damage.is_empty() {
                let t0 = Instant::now();
                backend.present(&comp, &damage);
                app.stats.present += t0.elapsed();
            }
            if let Some((p, ph)) = backend.pacing() {
                (period, phase) = (p, ph);
                app.refresh = p;
            }
            next_frame = pacer::next_frame(app.frame_time, pacer::now(), period, phase);
            backend.dispatch(&mut app);
        }
        let t0 = Instant::now();
        backend.flush(&comp);
        app.stats.present += t0.elapsed();
        app.space.refresh();
        app.popups.cleanup();
        app.reap_children();
        app.prune_pending();
        let _ = app.dh.flush_clients();
        if let Some(every) = args.stats_every
            && now >= next_stats
        {
            eprintln!("bitscomp stats: {}", app.stats.report());
            next_stats = now + Duration::from_secs(every);
        }
    }
    let _ = std::fs::remove_file(&ctl_path);
    end_children(&mut app.children);
    // WY-11: the hardware cursor off while bitscomp still holds the screen.
    app.cursor.shutdown();
    // The fbdev back end unmaps and gives the console back here.
    drop(backend);
    eprintln!("bitscomp: ended");
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
