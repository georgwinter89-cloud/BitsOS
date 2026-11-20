//! bitscomp: BitsOS's Wayland compositor (V2-PLAN §8a.2), on V2's fbdev and evdev (WY-1a). It draws only on the GPU:
//! GLES through the Mesa tree's radeonsi, every frame straight into the screen's scanout windows (WY-17), with all
//! effects (WY-19), the hardware cursor (WY-11) and Xwayland with glamor (WY-3b). A machine without a render node has
//! no desktop: bitscomp ends with one line and the console keeps the screen (`mesa::gpu_or_why`).
//!
//! WY-4: never waits for a client (B4: `hung`, `answer`, `ending`, `veil`, the MCP tools in `mcp`).
//!
//! WY-5: the approved look (plan/spec/desktop.md): chrome, workspaces, start page, theme from settingsd, live.
//!
//! bitscomp [--fb PATH] [--socket NAME] [--stats SECS] [--user UID[:GID]]
//!          Every service link is always on, at the system's sockets: MCP, settingsd, the installer's confirmer
//!          (MC-9b), bitspkgd (PK-7a), bitssnap (REWIND), `assist serve` in the session (CL-3b, only the helper may
//!          answer, `assist_peer`) and the Xwayland started by the first X11 client (WY-3).
//! bitscomp ctl [--socket NAME] COMMAND...
//! bitscomp exec PPID PROGRAM [ARG]... (WY-7, internal: how bitscomp starts a program, `spawn`)
//! bitscomp licenses (the Exo 2 font licence, OFL 1.1)
//! bitscomp page ADDRESS (WY-5b: the window of a settings page, PK-7a: or of `programme/`; bitscomp draws its
//! content, `page_client`)

mod acquire;
mod bootmark;
mod alsa_ctl;
mod address;
mod confirm_note;
mod crash_note;
mod anim;
mod assist;
mod assist_app;
mod assist_client;
mod assist_ctl;
mod assist_desk;
#[cfg(test)]
mod assist_fake;
mod assist_draw;
mod assist_layout;
mod assist_level;
mod assist_menu;
mod assist_menu_app;
mod assist_menu_draw;
mod assist_paste;
mod assist_peer;
mod assist_route;
mod assist_serve;
mod assist_tree;
mod assist_views;
mod answer;
mod apply;
mod backend;
mod canvas;
mod chrome;
mod chrome_draw;
mod chrome_render;
mod clients;
mod clip_read;
mod ctl;
// EINS-D: the control socket's part for `bits` (user/tools/bits).
mod ctl_bits;
mod ctl_bits_card;
mod ctl_bits_clip;
mod ctl_bits_gate;
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
mod feld;
mod feld_app;
mod feld_draw;
mod feld_view;
mod foot;
mod symbols;
mod gpu_watch;
mod grab;
mod grant;
mod handlers;
mod header;
mod helpers;
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
mod ki;
mod ki_app;
mod leistung;
// DB4: the one memory (ctxd): habits, "Gerade passend", the page "Was BitsOS über dich gelernt hat".
mod gelernt;
mod gelernt_app;
mod habit_app;
mod habit_ask;
mod habit_client;
mod habit_hint;
mod habit_vorrang;
// DB3: the glue to foreign programs' trees (C3) and Durchblick's "Erklär mir das".
mod ui_fetch;
mod db_act;
mod desk_paste;
mod desk_write;
mod desk_write_app;
mod db_act_app;
mod db_act_draw;
mod db_app;
mod db_ctl;
mod db_draw;
mod db_input;
mod db_layout;
mod db_question;
mod db_settings;
mod db_shot;
mod db_ui;
mod db_worker;
mod passend;
mod leistung_app;
mod werhatwas;
mod werhatwas_app;
mod update_app;
mod update_page;
// SYS-A: the program "System" (overview, measurements, task manager).
mod sys_act;
mod sys_amdgpu;
mod sys_app;
mod sys_chart;
mod sys_clip;
mod sys_ctl;
mod sys_draw;
mod sys_draw2;
mod sys_group;
mod sys_layout;
mod sys_limit;
mod sys_model;
mod sys_model2;
mod sys_model3;
mod sys_parse;
mod sys_report;
mod sys_ring;
mod sys_rules;
mod sys_sample;
mod sys_snap;
mod sys_sources;
mod sys_text;
mod sys_ui;
mod sys_view;
mod sys_wer;
mod ki_ask;
mod ki_guard;
mod ki_key;
mod ki_kinds;
mod ki_helpers;
mod ki_change;
mod ki_probe;
mod launch;
mod lock;
mod layout;
mod home;
mod min_close;
mod min_row;
mod ws_close;
mod library;
mod library_add;
mod library_facts;
mod library_steam;
mod library_twin;
mod library_draw;
mod library_ui;
mod library_view;
mod minimize;
mod pad;
mod pad_app;
mod mcp;
mod mcp_start;
mod menu;
mod menu_draw;
mod menu_input;
mod mesa;
mod pacer;
mod playtime;
mod page;
mod page_app;
mod page_client;
mod page_confirm;
mod page_draw;
mod page_kind;
mod page_layout;
mod page_schema;
mod page_join;
mod page_paste;
mod page_rail;
mod page_tabs;
mod page_tabs_app;
mod page_view;
mod place;
mod pointer;
mod pkg_client;
mod prog_end;
mod prog_note;
mod prog_note_draw;
mod programme;
mod programme_app;
mod programme_draw;
mod programme_layout;
mod programme_text;
mod programme_view;
mod rewind;
mod rewind_app;
mod rewind_draw;
mod rewind_view;
mod rundir;
mod paint;
mod readback;
mod recent_name;
mod release;
mod fx;
mod fx_gl;
mod render;
mod render_gl;
mod reveal;
mod ring;
mod session;
mod settings;
mod settings_write;
mod snap_client;
mod sofa;
mod sofa_app;
mod sofa_body;
mod sofa_draw;
mod sofa_focus;
mod sofa_header_draw;
mod sofa_io;
mod sofa_layout;
mod sofa_library_draw;
mod sofa_model;
mod sofa_music;
mod sofa_music_serve;
mod bench_serve;
mod sofa_quick;
mod sofa_quick_draw;
mod sofa_search;
mod sofa_search_app;
mod sofa_search_draw;
mod sofa_settings;
mod sofa_settings_draw;
mod sofa_start_draw;
mod sofa_tile_draw;
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
mod text_in;
mod theme;
mod theme_auto;
// DB1 (Durchblick): the desktop as a list (ctl ui, ui-act, ui-history; MCP ui_list, ui_act).
mod ui_act;
mod ui_client;
mod ui_ctl;
mod ui_desk;
mod ui_history;
mod ui_keys;
mod ui_mcp;
mod ui_page;
mod ui_reg;
mod ui_sofa;
mod ui_stack;
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

use backend::fbdev::Fbdev;
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
    fb: String,
    socket: String,
    stats_every: Option<u64>,
    /// WY-7: whom a root bitscomp runs as once its devices are open (`drop_root`; 0 keeps root).
    user: (u32, u32),
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        fb: "/dev/fb0".into(),
        socket: "wayland-bits-0".into(),
        stats_every: None,
        user: drop_root::DESKTOP,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--fb" => a.fb = val()?,
            "--socket" => a.socket = val()?,
            "--stats" => a.stats_every = Some(val()?.parse().map_err(|_| "--stats SECS")?),
            "--user" => a.user = drop_root::parse_user(&val()?)?,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() {
    // GS-1: the start's marks (/dev/kmsg is opened while the console's start is still root).
    bootmark::begin();
    let argv: Vec<String> = std::env::args().skip(1).collect();
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
    // Before anything is opened: without a render node there is no desktop, and the console keeps the screen.
    let node = mesa::gpu_or_why(dmabuf::render_device().1.as_deref(), |n| gpu_watch::look_within(n, Duration::from_secs(1))).unwrap_or_else(|why| {
        eprintln!("bitscomp: {why}");
        std::process::exit(NO_GPU_EXIT);
    });
    // The tree's EGL, while bitscomp has one thread.
    mesa::set_env();
    if let Err(e) = run(args, node) {
        eprintln!("bitscomp: {e}");
        std::process::exit(1);
    }
}

/// bitscomp's exit status on a machine without a GPU render node: it ended before it touched the screen.
const NO_GPU_EXIT: i32 = 69;

fn run(args: Args, node: String) -> Result<(), String> {
    // WY-7: a root start (the console) runs as the desktop user once its devices are open (`drop_root`).
    let drop_to = drop_root::plan(drop_root::euid(), args.user);
    // Before any thread exists (set_var); made and checked after the drop, as the user.
    let rundir = rundir::select(drop_to.map(|(uid, _)| uid));
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| e.to_string())?;
    // A plain kill ends like Strg+Alt+Q, so the socket files and the started programs are cleaned up.
    // Blocked here, before the back end starts threads, so every thread inherits the mask.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP]).map_err(|e| e.to_string())?;
    event_loop.handle().insert_source(signals, |_, _, app| app.quit = true).map_err(|e| e.to_string())?;
    let mut backend = Fbdev::new(&args.fb, &event_loop.handle())?;
    // The grant needs the screen's holder, which the back end just became (and root's fb0 open, when dropping).
    let grant = grant::Grant::request(&args.fb);
    // PE-1: the kernel log for the program lines, while root may still open it.
    prog_note::open_journal();
    if let Some((uid, gid)) = drop_to {
        // Still root: a stale root-owned lock or font cache of an older root run can be probed and cleared.
        rundir::clear_foreign(&rundir, &args.socket, uid);
        // SYS-A: the session's RLIMIT_NICE (the task manager's Vorrang), set while root may still raise it.
        if let Err(e) = sys_limit::session_nice() {
            eprintln!("bitscomp: RLIMIT_NICE {} for the session not set: {e}", sys_limit::SESSION);
        }
        drop_root::drop_to(uid, gid).map_err(|e| format!("giving up root for uid {uid}: {e} (--user 0 runs as root on purpose)"))?;
        user_env(uid);
        eprintln!("bitscomp: devices open, now running as uid {uid} gid {gid}");
    }
    // The shader cache in the desktop user's home on the data volume, before the first EGL display.
    mesa::set_shader_cache(&helpers::home(drop_root::euid()));
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
    app.grant = grant;
    mcp::start(&handle, mcpreg::DEFAULT_SOCKET.into())?;
    // The installer's confirmer: its peer must be the boot's bitsinstall as root.
    install_link::start(&handle, install_link::SOCKET.into())?;
    settings::start(&handle, settings::SOCKET.into())?;
    match settings_write::start(&handle, settings::SOCKET.into()) {
        Ok(w) => app.settings_writer = Some(w),
        Err(e) => eprintln!("bitscomp: {e} (the settings page shows values only)"),
    }
    // PK-7a: the "Programme" page's client (a thread per request; nothing is asked before a page opens).
    app.prog_client = Some(pkg_client::start(&handle, pkg_client::SOCKET.into())?);
    // REWIND: "Zurückspulen"'s client (a thread per request; a list once a minute for the strip's button).
    app.snap_client = Some(snap_client::start(&handle, snap_client::SOCKET.into())?);
    // CL-5: "Testen" on the page "KI" (a thread per test; nothing runs before Georg clicks it).
    match ki_probe::start(&handle, drop_root::euid()) {
        Ok(p) => app.ki.prober = Some(p),
        Err(e) => eprintln!("bitscomp: ki: no channel for the provider tests: {e} (the page says so)"),
    }
    // CL-3b: the assistant's socket (two threads of its own; the sidebar draws from the model they fill), guarded
    // (`assist_peer`): only the helper may answer there.
    let assist_guard = std::sync::Arc::new(assist_peer::Guard::new(std::path::Path::new(assist_serve::EXE), drop_root::euid()));
    // The desktop starts without the assistant too: its sidebar then stays as it was.
    match assist_client::start(&handle, assist_client::socket(&rundir), Some(assist_guard.clone())) {
        Ok(c) => app.assist_client = Some(c),
        Err(e) => eprintln!("bitscomp: {e} (no conversation in the sidebar)"),
    }
    session::start(&handle, &mut app, &rundir)?;
    // PK-L1a: the program index (its own thread; a failure leaves the built-in programs).
    if let Err(e) = library::start(&handle, library::Roots::system(), |app: &mut App, ix| {
        app.library = ix;
        app.chrome_dirty();
    }) {
        eprintln!("bitscomp: library: {e} (only the built-in programs)");
    }
    // GM-6b: the play-time file's thread and the music player's (the sofa mode's Start and Bibliothek).
    app.sofa_body_start(&handle);
    // GM-6c: the quick menu's thread (volume, outputs, the pad's battery, the power request).
    app.sofa_io_start(&handle, true);
    // SELFOPT-1: the page "Leistung"'s data (its own thread).
    app.leistung_start(&handle);
    // DB4: what Georg does goes to ctxd; what it learned comes back.
    app.habit_start(&handle);
    // DB3: foreign programs' trees come from a thread.
    app.ui_fetch_start(&handle);
    app.db_start(&handle);
    app.desk_write_start(&handle);
    app.prog_start_loop(&handle);
    // DB2: the page "Wer hat was gemacht"'s data from bitsaccess (its own thread).
    app.wer_start(&handle);
    // UPDATE: the page "Aktualisieren" talks to bitspkgd on its own thread.
    app.upd_start(&handle, pkg_client::SOCKET.into());
    // VB-1: "Version N läuft. Bestätigen?" while the kernel asks.
    app.confirm_start(&handle, pkg_client::SOCKET.into());
    // GPY-PASTE: a paste into a page's field comes back from its reading thread.
    app.page_paste_start(&handle);
    // SYS-A: the program "System"'s sampler (asleep until a System window opens).
    app.sys_start(&handle);
    // INT-GM6 (GM-6m's change request 1): the music player of this session.
    sofa_music_serve::start(drop_root::euid());
    // SELFOPT-1: the nightly benchmark runner of this session (it measures only when Georg switched it on).
    bench_serve::start(drop_root::euid(), &args.socket);
    // CL-3b: the assistant's helper of this session.
    assist_serve::start(drop_root::euid(), assist_guard);
    app.xwayland = xwayland::setup(&handle, xwayland::DEFAULT_EXE, &node);
    // WY-11: the hardware cursor; the first image tells whether it works (else the frames draw the pointer).
    app.cursor.hw = backend.hw_cursor();
    app.cursor_sync();
    app.spawn_env = terminal::prepare(&rundir);
    app.crash_note(&rundir);
    terminal::cursor_env(|k| std::env::var(k).ok(), &mut app.spawn_env, app.cursor.size);
    app.foot.path = Some(foot::path(&rundir));
    app.foot_sync();
    if !app.spawn_env.is_empty() {
        let set: Vec<String> = app.spawn_env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("bitscomp: started programs also get {}", set.join(" "));
    }
    let (mut period, mut phase) = backend.pacing().unwrap_or((pacer::DEFAULT_PERIOD, Duration::ZERO));
    app.refresh = period;
    let mut comp = render::make(&app)?;
    bootmark::mark("renderer_up");
    backend::scanout::start(&mut backend, &mut *comp).map_err(|e| format!("scanout: {e}"))?;
    dmabuf::init(&mut app, &*comp, handle.clone());
    let mut gpu = GpuSide::new(node);
    println!(
        "WAYLAND_DISPLAY={} XDG_RUNTIME_DIR={} control={} pid={} size={}x{} refresh_us={} renderer={}",
        args.socket,
        rundir.display(),
        ctl_path.display(),
        std::process::id(),
        backend.size().0,
        backend.size().1,
        period.as_micros(),
        comp.renderer()
    );
    let _ = std::io::stdout().flush();

    let mut next_frame = pacer::now();
    let mut next_stats = Instant::now();
    // Why the desktop ends on its own (a lost GPU, windows that cannot be taken again): said at the end.
    let mut ended: Option<String> = None;
    let mut guard = spin::Guard::default();
    let mut missed = clients::Missed::default();
    let mut gpu_late = false;
    while !app.quit {
        // A screenshot waiting for its frame (`ctl::run_queue`) keeps the frame timer on until it is taken.
        let busy = app.needs_frame || app.layout_dirty || app.shot_wait.is_some();
        // Asleep until a client, an input device, the control socket or the frame timer wakes us, or a ping, a
        // "hung" deadline or a SIGKILL is due; the 1 s cap reaps children.
        let mono_now = pacer::now();
        gpu.turn(mono_now);
        let busy = busy || app.anim.running(mono_now, &app.theme);
        let wake = spin::earliest(&[
            (busy.then_some(next_frame), "frame"),
            (app.answer_due(), "answer"),
            (app.ending_due(), "ending"),
            (app.status.due(), "status"),
            (app.session.due, "session"),
            (app.notice_tick(mono_now), "notice"),
            (app.habit_hint_tick(mono_now), "hint"),
            (app.min_close_tick(mono_now), "min-close"),
            (app.veil_next, "veil"),
            (app.repeat_due(), "repeat"),
            (app.assist_due(), "assist"),
            (app.pad_due(), "pad"),
            (app.sofa_due(), "sofa"),
            (app.desk.reveal.due(), "reveal"),
            (app.launches.due(), "launches"),
            (app.install_due(), "install"),
            (app.crash_due(), "crash"),
            (app.confirm_due(), "confirm"),
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
        ctl::run_queue(&mut app, &mut *comp);
        // DB3: the picture Durchblick asked for, from the frame on the screen.
        db_shot::take(&mut app, &mut *comp);
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
        app.theme_auto_tick(mono);
        app.notice_tick(mono);
        app.repeat_tick(mono);
        app.assist_tick(mono);
        app.pad_tick(mono);
        app.sofa_tick(mono);
        app.reveal_tick(mono);
        if app.launches.tick(mono) {
            app.needs_frame = true;
        }
        app.install_tick(mono);
        app.crash_tick(mono);
        app.confirm_tick(mono);
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
            // WY-17: into the hidden scanout window (may wait for the last flip).
            if let Err(e) = aim_frame(&app, &mut backend, &mut *comp, &gpu) {
                ended = Some(format!("scanout: {e}"));
                break;
            }
            let damage = comp.render(&mut app);
            // WY-21: the frame callbacks and buffer releases leave before a present that waits for the flip. Sent
            // after it, a client heard of the frame a refresh late, its next buffer missed the frame drawn right after
            // the flip, and on a desktop that draws every refresh it got every second one: 30 a second (glxgears through
            // Xwayland on the Mini-PC, wy/86).
            let _ = app.dh.flush_clients();
            gpu.waiting(true);
            if !damage.is_empty() {
                let t0 = Instant::now();
                backend.present(&damage);
                app.stats.present += t0.elapsed();
                bootmark::presented(comp.renderer());
            }
            gpu.waiting(false);
            // The windows ended with this flip: the next frame takes new ones and draws them whole.
            if backend.scanout_left() {
                app.needs_frame = true;
            }
            if let Some((p, ph)) = backend.pacing() {
                (period, phase) = (p, ph);
                app.refresh = p;
            }
            next_frame = pacer::next_frame(app.frame_time, pacer::now(), period, phase);
        }
        app.space.refresh();
        app.popups.cleanup();
        app.reap_children();
        app.prune_pending();
        // WY-16: after the frame (its draw has finished) the replaced buffers go back.
        app.dma.gpu_busy = !comp.gpu_idle();
        // WY-19: a frame that outlasted `GPU_WAIT` was not flipped: once the GPU is done the next frame shows it, even
        // when nothing else asks for one (a first blur on a cold shader cache).
        if gpu_late && !app.dma.gpu_busy {
            app.needs_frame = true;
        }
        gpu_late = app.dma.gpu_busy;
        if let Some(why) = gpu.lost(&mut *comp, gpu_late, pacer::now()) {
            if let Err(e) = leave_gpu(&mut app, &mut comp, &mut backend, &mut gpu, &why) {
                ended = Some(e);
                break;
            }
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
    // The fbdev back end gives the console back here.
    drop(backend);
    if let Some(why) = ended {
        // A compositor on a lost GPU is never dropped: its teardown would wait on a driver that may not answer.
        std::mem::forget(comp);
        return Err(why);
    }
    eprintln!("bitscomp: ended");
    Ok(())
}

/// WY-17: the window this frame is drawn into. When the windows of this hold ended (a refused pan, a screen that went
/// back) or the picture has none (a new one after a reset or a mode change), they are taken again (`scanout::start`);
/// an error when that fails, since frames have no other way to the screen.
fn aim_frame(app: &App, backend: &mut Fbdev, comp: &mut dyn render::Composer, gpu: &GpuSide) -> Result<(), String> {
    for _ in 0..2 {
        gpu.waiting(true);
        let aim = backend.scanout_frame();
        gpu.waiting(false);
        if aim.is_some_and(|a| comp.aim(app, a)) {
            return Ok(());
        }
        backend.end_direct("the picture has no windows of this hold");
        let _ = backend.scanout_left();
        backend::scanout::start(backend, comp)?;
    }
    Err("the windows just taken again have ended at once".into())
}

/// WY-21: what the loop knows of the GPU it draws on (`gpu_watch`).
struct GpuSide {
    watch: Option<gpu_watch::Watch>,
    node: String,
    late_since: Option<Duration>,
    next_reset_look: Duration,
    /// GLES renderers made after a reset the device lived through.
    rebuilds: u32,
}

/// How many times a reset the device lived through gets a new GLES renderer before the desktop ends.
const GPU_REBUILDS: u32 = 2;

impl GpuSide {
    fn new(node: String) -> GpuSide {
        let watch = gpu_watch::Watch::start(&node);
        GpuSide { watch, node, late_since: None, next_reset_look: Duration::ZERO, rebuilds: 0 }
    }

    fn turn(&self, now: Duration) {
        if let Some(w) = &self.watch {
            w.turn(now);
        }
    }

    /// A wait on the display (the last flip, a present) begins or ends: `gpu_watch::STALL` counts only these.
    fn waiting(&self, on: bool) {
        if let Some(w) = &self.watch {
            w.waiting(on.then(pacer::now));
        }
    }

    /// Why this renderer cannot draw on, once one of the signs says so.
    fn lost(&mut self, comp: &mut dyn render::Composer, late: bool, now: Duration) -> Option<String> {
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

/// WY-21: a reset the device lived through (its node opens, the GL context alone was lost) gets a new GLES renderer in
/// this process, so every client keeps its window; the windows are taken again at the next frame (`aim_frame`). Any
/// other loss, or one reset too many, ends the desktop with the line why. The old compositor is never dropped: its
/// teardown would wait on a driver that may not answer. A GPU that a frame waits on for ever is `gpu_watch`'s stall.
fn leave_gpu(app: &mut App, comp: &mut Box<dyn render::Composer>, backend: &mut Fbdev, gpu: &mut GpuSide, why: &str) -> Result<(), String> {
    let recovered = why.starts_with("the GL context") && gpu.rebuilds < GPU_REBUILDS && gpu_watch::look_within(&gpu.node, Duration::from_secs(1)) == gpu_watch::Look::Opens;
    if !recovered {
        return Err(format!("gpu: {why}: the desktop ends, the console takes the screen back"));
    }
    eprintln!("bitscomp: gpu: {why}: the desktop goes on with a new gles renderer, the windows stay");
    backend.end_direct("the GPU was reset");
    let _ = backend.scanout_left();
    let fresh = render::make(app)?;
    std::mem::forget(std::mem::replace(comp, fresh));
    gpu.rebuilds += 1;
    gpu.late_since = None;
    app.dma.gpu_busy = false;
    dmabuf::renew(app, &**comp);
    app.needs_frame = true;
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
