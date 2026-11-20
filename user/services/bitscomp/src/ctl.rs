//! Control socket `$XDG_RUNTIME_DIR/<wayland socket>.ctl`: one command line per connection, one answer.
//! The desk's check lists (MCP `run`), `bits` and the assistant's tree drive the compositor through it; every input
//! from it counts as injected. It is also the seed of the MCP tools of WY-4 (`window_kill`) and the settings hook of
//! WY-5.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::input::InputEvent;
use crate::layout::Place;
use crate::render::Picture;
use crate::state::App;
use crate::wm::Action;

pub fn path(socket_name: &str) -> PathBuf {
    crate::rundir::dir().join(format!("{socket_name}.ctl"))
}

/// The longest command line a peer may send; longer ones are dropped with their connection.
const MAX_LINE: usize = 4096;
/// A peer that has not sent its line by then is dropped.
const LINE_DEADLINE: Duration = Duration::from_secs(2);

pub fn listen(handle: &LoopHandle<'static, App>, socket_name: &str) -> std::io::Result<PathBuf> {
    let p = path(socket_name);
    // A running bitscomp with the same socket name keeps its control socket: a second one refuses to start.
    if UnixStream::connect(&p).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("{} answers: another bitscomp runs with this socket name", p.display()),
        ));
    }
    let _ = std::fs::remove_file(&p);
    let listener = UnixListener::bind(&p)?;
    listener.set_nonblocking(true)?;
    let h = handle.clone();
    handle
        .insert_source(Generic::new(listener, Interest::READ, Mode::Level), move |_, l, _| {
            while let Ok((stream, _)) = l.accept() {
                read_line_later(&h, stream);
            }
            Ok(PostAction::Continue)
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(p)
}

/// Collects one connection's command line without ever blocking the loop: a source of its own reads what has
/// arrived, queues the line at its newline, and a timer drops a peer that is too slow or sends too much.
fn read_line_later(handle: &LoopHandle<'static, App>, stream: UnixStream) {
    if stream.set_nonblocking(true).is_err() {
        return;
    }
    let mut buf = Vec::new();
    let token = handle.insert_source(Generic::new(stream, Interest::READ, Mode::Level), move |_, s, app| {
        let mut chunk = [0u8; 512];
        loop {
            match std::io::Read::read(&mut &**s, &mut chunk) {
                Ok(0) => return Ok(PostAction::Remove),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(PostAction::Continue),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Ok(PostAction::Remove),
            }
            if let Some(end) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..end]).trim().to_string();
                if let Ok(answer) = s.try_clone() {
                    let _ = answer.set_nonblocking(false);
                    // Answers are short lines; a peer that does not read them costs at most this.
                    let _ = answer.set_write_timeout(Some(Duration::from_millis(100)));
                    app.ctl_queue.push((line, answer));
                }
                return Ok(PostAction::Remove);
            }
            if buf.len() > MAX_LINE {
                return Ok(PostAction::Remove);
            }
        }
    });
    let Ok(token) = token else { return };
    let h = handle.clone();
    // Removing a source that already ended is a no-op for calloop (the token's generation is checked).
    let _ = handle.insert_source(Timer::from_duration(LINE_DEADLINE), move |_, _, _| {
        h.remove(token);
        TimeoutAction::Drop
    });
}

/// Client side: `bitscomp ctl <command...>`.
pub fn send(socket_name: &str, cmd: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(path(socket_name))?;
    s.write_all(format!("{cmd}\n").as_bytes())?;
    let mut out = String::new();
    std::io::Read::read_to_string(&mut s, &mut out)?;
    Ok(out)
}

/// The longest a screenshot waits for a frame that is due (`run_queue`).
const SHOT_FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

pub fn run_queue(app: &mut App, comp: &mut dyn crate::render::Composer) {
    // A screenshot waits for the start animation and an unfolding menu (WY-5), so it shows the settled look.
    // It also waits for a frame that is due and not drawn yet (a click that opened the library a turn ago would
    // picture the desktop without it), at most SHOT_FRAME_WAIT, so clients that draw all the time do not hold it off.
    let moving = app.anim.running(crate::pacer::now(), &app.theme);
    let shot_queued = app.ctl_queue.iter().any(|(c, _)| c.split_whitespace().next() == Some("shot"));
    let frame_due = shot_queued && (app.needs_frame || app.layout_dirty) && app.shot_wait.get_or_insert_with(std::time::Instant::now).elapsed() < SHOT_FRAME_WAIT;
    if !frame_due {
        app.shot_wait = None;
    }
    let (waiting, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut app.ctl_queue).into_iter().partition(|(c, _)| {
        (moving || frame_due) && c.split_whitespace().next() == Some("shot")
    });
    app.ctl_queue = waiting;
    for (cmd, stream) in rest {
        // EINS-D: a line of the assistant's tree is judged (one gate, `ctl_bits`), and bits's own verbs run, before
        // anything else.
        let Some((cmd, stream)) = crate::ctl_bits::take(app, comp, cmd, stream) else { continue };
        // DB3: `ui` waits for a foreign program's provider on a thread, `ui-act` on such an element too.
        let Some(mut stream) = app.ui_defer_ctl(&cmd, stream) else { continue };
        // WY-17: frames go straight to the scanout and leave no copy; a screenshot reads the one on the screen.
        if cmd.split_whitespace().any(|w| w == "shot") {
            comp.sync_pixels();
        }
        // WY-19: `fx` asks the renderer too; the other commands see only the finished picture.
        let words: Vec<&str> = cmd.split_whitespace().collect();
        let result = if words.first() == Some(&"fx") { crate::fx::ctl::execute(app, &*comp, &words) } else { execute(app, &*comp, &cmd) };
        let answer = result.unwrap_or_else(|e| format!("error: {e}"));
        let _ = stream.write_all(format!("{answer}\n").as_bytes());
    }
}

/// R1 does not apply to a test command's argument; this only keeps one `flood` from eating memory (Smithay keeps
/// each unacknowledged configure until the client goes).
const FLOOD_MAX: u32 = 200_000;

fn execute(app: &mut App, comp: &dyn Picture, cmd: &str) -> Result<String, String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let num = |i: usize| -> Result<f64, String> {
        words.get(i).ok_or("missing number")?.parse::<f64>().map_err(|e| e.to_string())
    };
    // MC-9b: the control socket's input is always injected and can never answer the installer.
    // CL-5: while Georg types a provider's key, no command moves the keyboard, the focus or a setting.
    if app.ki_ctl_waits(&words) {
        return Err(crate::ki_guard::BUSY.into());
    }
    if words.first() == Some(&"install") {
        return app.install_ctl(&words);
    }
    if let Some(r) = crate::pad_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::sofa_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = app.assist_ctl(&words, cmd) {
        return r;
    }
    if let Some(r) = crate::feld_app::ctl(app, &words, cmd) {
        return r;
    }
    if let Some(r) = crate::rewind_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::leistung_app::ctl(app, &words) {
        return r;
    }
    // DB3: Durchblick's panel.
    if let Some(r) = crate::db_ctl::ctl(app, &words, cmd) {
        return r;
    }
    // DB1: ui, ui-act, ui-history.
    if let Some(r) = crate::ui_ctl::ctl(app, &words, cmd) {
        return r;
    }
    if let Some(r) = crate::habit_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::werhatwas_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::update_app::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::confirm_note::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::sys_ctl::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::prog_note::ctl(app, &words) {
        return r;
    }
    if let Some(r) = crate::ctl_mouse::execute(app, &words) {
        return r;
    }
    if let Some(r) = crate::ctl_desk::execute(app, &words, cmd) {
        return r;
    }
    // GS-1: the start's summary line (`boot.desktop_ready`), or the marks so far.
    if let Some(r) = crate::bootmark::ctl(&words) {
        return r;
    }
    match words.first().copied().unwrap_or("") {
        "list" => Ok(list(app)),
        // WY-3: the X11 display: its number, Xwayland's state and pid, X11 windows, the clipboard bridge.
        "xwayland" => Ok(app.xwayland.as_ref().map_or_else(|| "xwayland off".to_string(), |x| x.describe())),
        // WY-11: the pointer's mode (hw/software), shape, image, counters and the input-to-move delay.
        "cursor" => Ok(app.cursor.report()),
        "stats" => {
            // WY-7: how programs start and how many still wait for their window (read here only, no loop cost).
            let how = if crate::spawn::trampoline().is_some() { "trampoline" } else { "fork" };
            let r = format!("{} output={}x{} launching={} spawn={how} renderer={}", app.stats.report(), comp.size().0, comp.size().1, app.launches.count(), comp.renderer());
            if words.get(1) == Some(&"reset") {
                app.stats = Default::default();
            }
            Ok(r)
        }
        // WY-17b: what the status bar's clock shows, and why (QEMU's check; the picture alone cannot say it).
        "clock" => Ok(format!("clock {} {}", app.status.clock, crate::status::zone_report())),
        "key" => {
            for tok in &words[1..] {
                let (code, press, release) = match tok.as_bytes()[0] {
                    b'+' => (&tok[1..], true, false),
                    b'-' => (&tok[1..], false, true),
                    _ => (&tok[..], true, true),
                };
                let code: u32 = code.parse().map_err(|_| format!("bad key {tok}"))?;
                if press {
                    app.handle_input(InputEvent::Key { code, pressed: true });
                }
                if release {
                    app.handle_input(InputEvent::Key { code, pressed: false });
                }
            }
            Ok("ok".into())
        }
        "float" => run(app, Action::Float),
        // WY-5c: maximise the focused window to the whole screen, and back.
        "max" => run(app, Action::Maximise),
        "stack" => run(app, Action::Stack),
        "unstack" => run(app, Action::Unstack),
        "flip" => run(app, Action::Flip(num(1).unwrap_or(1.0) as i32)),
        "focus" => run(app, Action::Focus(num(1).unwrap_or(1.0) as i32)),
        "grow" => run(app, Action::Grow(0.25)),
        "shrink" => run(app, Action::Grow(-0.25)),
        // close [ID]: asks a window to close (the focused one without ID).
        "close" => match words.get(1) {
            Some(id) => {
                crate::xwm::close(&window(app, id)?);
                Ok("ok".into())
            }
            None => run(app, Action::Close),
        },
        // WY-12: minimize [ID] (the focused window without ID), unminimize ID, switch [-1] (Alt+Tab).
        "minimize" => {
            let w = match words.get(1) {
                Some(id) => window(app, id)?,
                None => app.ws.focused().cloned().ok_or("no focused window")?,
            };
            if app.minimize(&w, "ctl") { Ok("ok".into()) } else { Err(format!("window {} is not shown", win_id(&w))) }
        }
        "unminimize" => {
            let w = window(app, words.get(1).ok_or("usage: unminimize ID")?)?;
            if app.unminimize(&w, "ctl") { Ok("ok".into()) } else { Err(format!("window {} is not minimised here", win_id(&w))) }
        }
        "switch" => run(app, Action::Switch(num(1).unwrap_or(1.0) as i32)),
        // kill [ID]: end a window's program (the focused one without ID), as "Programm beenden" does.
        "kill" => {
            let w = match words.get(1) {
                Some(id) => {
                    let id: u64 = id.parse().map_err(|_| format!("bad window id {id}"))?;
                    app.all_windows().into_iter().find(|w| win_id(w) == id).ok_or(format!("no window {id}"))?
                }
                None => app.ws.focused().cloned().ok_or("no focused window")?,
            };
            app.end_window(&w, "ctl kill")
        }
        // flood ID N: N configures to window ID at once, ignoring "hung" (test of B4's bounded output queue: a
        // client that does not read is dropped, never waited for). Stops when the client is gone.
        "flood" => {
            let id: u64 = words.get(1).and_then(|v| v.parse().ok()).ok_or("usage: flood ID N")?;
            let n = words.get(2).and_then(|v| v.parse::<u32>().ok()).ok_or("usage: flood ID N")?.min(FLOOD_MAX);
            let w = app.all_windows().into_iter().find(|w| win_id(w) == id).ok_or(format!("no window {id}"))?;
            let (top, client) = (w.toplevel().ok_or("not a toplevel")?.clone(), app.client_of(&w).ok_or("no client")?);
            // A client the backend has given up on is skipped by with_all_clients (until the next dispatch).
            let alive = |app: &App| {
                let mut found = false;
                app.dh.backend_handle().with_all_clients(|c| found |= c == client.id());
                found
            };
            let mut sent = 0;
            while sent < n && (sent % 64 != 0 || alive(app)) {
                top.send_configure();
                sent += 1;
            }
            let _ = app.dh.flush_clients();
            let alive = alive(app);
            Ok(format!("sent={sent} client={}", if alive { "connected" } else { "dropped" }))
        }
        "quit" => run(app, Action::Quit),
        "size" => {
            app.requested_size = Some((num(1)? as i32, num(2)? as i32));
            Ok("ok".into())
        }
        // CL-6: a program of the library by its name (mcp_start.rs), for BitsOS's assistant.
        "program-start" => app.ctl_program_start(cmd.trim_start().strip_prefix("program-start").unwrap_or("")),
        // Which entry a name means, before anybody is asked whether it may start: `<id>\t<name>`.
        "program-find" => app.ctl_program_find(cmd.trim_start().strip_prefix("program-find").unwrap_or("")),
        "spawn" => {
            let argv: Vec<&str> = words[1..].to_vec();
            app.spawn(&argv).map(|pid| format!("pid {pid}")).ok_or_else(|| "spawn failed".into())
        }
        "shot" => {
            let p = words.get(1).ok_or("missing path")?;
            write_ppm(comp, p).map_err(|e| e.to_string())?;
            Ok(format!("{}x{}", comp.size().0, comp.size().1))
        }
        other => Err(format!("unknown command '{other}'")),
    }
}

fn window(app: &App, id: &str) -> Result<crate::win::Window, String> {
    let n: u64 = id.parse().map_err(|_| format!("bad window id {id}"))?;
    app.all_windows().into_iter().find(|w| win_id(w) == n).ok_or(format!("no window {n}"))
}

fn run(app: &mut App, a: Action) -> Result<String, String> {
    app.act(a);
    Ok(format!("{a:?}"))
}

pub(crate) fn list(app: &App) -> String {
    let mut out = String::new();
    for p in app.placements() {
        let (app_id, title) = names(&p.win);
        let place = match p.place {
            Place::Tiled { tile, index, depth, .. } => format!("tile={tile} page={index}/{depth}"),
            Place::Floating { index } => {
                format!("float={index}{}", if app.ws.floating[index].restore.is_some() { " max" } else { "" })
            }
        };
        // WY-8: a fullscreen window's frame lies around the screen; its client part is what it covers.
        let (r, fs) = if p.fullscreen { (app.content_rect(p.frame), " fullscreen") } else { (p.frame, "") };
        out.push_str(&format!(
            "{place}{fs} visible={} focused={} frame={},{} {}x{} id={} app_id={app_id} title={title:?} pid={} hung={} ws={}{}\n",
            p.visible,
            p.focused,
            r.x,
            r.y,
            r.w,
            r.h,
            win_id(&p.win),
            app.pid_of(&p.win),
            app.is_hung(&p.win),
            app.desk.active + 1,
            p.win.x11().map(|x| format!(" x11=0x{:x}", x.xid)).unwrap_or_default()
        ));
    }
    out.push_str(&app.minimized_lines());
    out.push_str("end");
    out
}

struct WinId(u64);

/// A number for a window that stays for its life and is never reused (protocol ids are per client).
pub fn win_id(w: &crate::win::Window) -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    w.user_data().get_or_insert(|| WinId(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))).0
}

pub fn names(w: &crate::win::Window) -> (String, String) {
    // WY-3: an X11 window's app id is its WM_CLASS class, its title _NET_WM_NAME (else WM_NAME).
    let Some(t) = w.toplevel() else {
        let x = w.x11().expect("a window is a toplevel or an X11 window");
        return (x.class.borrow().clone(), x.title.borrow().clone());
    };
    smithay::wayland::compositor::with_states(t.wl_surface(), |s| {
        let d = s.data_map.get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>().unwrap().lock().unwrap();
        (d.app_id.clone().unwrap_or_default(), d.title.clone().unwrap_or_default())
    })
}

fn write_ppm(comp: &dyn Picture, path: &str) -> std::io::Result<()> {
    let (w, h) = comp.size();
    let (px, stride) = comp.pixels();
    let mut buf = format!("P6\n{w} {h}\n255\n").into_bytes();
    for y in 0..h as usize {
        for &p in &px[y * stride..y * stride + w as usize] {
            buf.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    std::fs::write(path, buf)
}
