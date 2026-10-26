//! wyclient: a test window for bitscomp (WY-1a, host/tests/wy/40-v2.sh). Maps an xdg toplevel whose wl_shm
//! buffer is a known picture (a solid colour by `--pattern N` with a white 64x64 square in the middle, 1:1 buffer
//! pixels) and prints one line per keyboard and pointer event it receives, so a test sees what reached it.
//!
//! wyclient [--title T] [--pattern N] [--animate] [--hang MS] [--stop MS] [--noread MS] [--ignore-term]
//! Lines: `ready`, `configure W H`, `keymap bytes=N symbols=S`, `enter keyboard`, `key CODE pressed|released`
//! (evdev code), `mods D L K G`, `pointer enter X Y`, `motion X Y`, `button CODE pressed|released`,
//! `axis vertical|horizontal V`, `close`.
//! WY-4: `--animate` draws a new frame on every frame callback from two buffers it reuses only after the
//! compositor released them, and every 2 s prints `frames n=N p50_ms=A p99_ms=B max_ms=C released=R starved=S`
//! (the callback intervals it saw; starved = callbacks with both buffers still held). `--hang MS`: from MS ms
//! after start it stops answering pings (prints `ignoring pings` once) but keeps reading; `--stop MS`: at MS ms it
//! prints `stopping` and stops itself with SIGSTOP (reads nothing any more); `--noread MS`: at MS ms it prints
//! `not reading` and never reads its socket again, but keeps running (B4's bounded output queue);
//! `--ignore-term`: ignores SIGTERM, so only SIGKILL ends it.
//! WY-8: after every `configure W H` a line `states fullscreen=F maximized=M activated=A` (0/1, the xdg states of
//! that configure). As a browser does it, F11 asks for fullscreen and back, Esc leaves fullscreen and F10 asks to
//! be maximised and back; `--fullscreen` asks for it before the first commit (the first configure must then be
//! fullscreen already). WY-8 fixer: `--fullscreen-at MS` asks for fullscreen MS ms after start (a window without
//! the focus, or a hung one); `--unfullscreen` sends unset_fullscreen before the first commit; F9 asks 20 times for
//! fullscreen and back at once (ends with back).
//! WY-11: `--cursor NAME|hidden|surface` sets the pointer on every enter (`cursor`).

mod cursor;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// The fill colours of the patterns (XRGB), the square is always white.
pub const COLOURS: [u32; 4] = [0x00c0_3030, 0x0030_a040, 0x0030_50c0, 0x00c0_a020];
pub const SQUARE: i32 = 64;

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    xdg: Option<xdg_surface::XdgSurface>,
    size: (i32, i32),
    pending: (i32, i32),
    buffer: Option<wl_buffer::WlBuffer>,
    colour: u32,
    done: bool,
    anim: Option<Anim>,
    hang_at: Option<Instant>,
    ignoring: bool,
    top: Option<xdg_toplevel::XdgToplevel>,
    /// (fullscreen, maximized, activated) of the configure being received, then of the last one.
    pending_states: (bool, bool, bool),
    states: (bool, bool, bool),
    /// WY-11 `--cursor`: what to set on enter, the shape manager, our own cursor surface.
    cursor: Option<String>,
    shape_mgr: Option<wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    cursor_surface: Option<wl_surface::WlSurface>,
}

/// `--animate`: two buffers, each used again only after its release.
struct Anim {
    bufs: Vec<(wl_buffer::WlBuffer, bool)>,
    next: usize,
    last: Option<Instant>,
    gaps_ms: Vec<f64>,
    report_at: Instant,
    released: u64,
    starved: u64,
}

/// p50, p99 and max of the intervals (ms); zeros for none.
pub fn percentiles(v: &mut [f64]) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[(((v.len() - 1) as f64) * q).round() as usize];
    (at(0.5), at(0.99), v[v.len() - 1])
}

/// The picture: `colour` everywhere, a white square in the middle (host-tested shape).
pub fn picture(w: i32, h: i32, colour: u32) -> Vec<u32> {
    let mut px = vec![colour; (w * h) as usize];
    let (x0, y0) = ((w - SQUARE) / 2, (h - SQUARE) / 2);
    for y in y0.max(0)..(y0 + SQUARE).min(h) {
        for x in x0.max(0)..(x0 + SQUARE).min(w) {
            px[(y * w + x) as usize] = 0x00ff_ffff;
        }
    }
    px
}

fn memfd(len: usize) -> std::io::Result<OwnedFd> {
    // SAFETY: memfd_create with a NUL-terminated name; the fd is checked and owned below.
    let fd = unsafe { libc::memfd_create(c"wyclient".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just made and is owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate on our own fd.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

impl State {
    /// Two pictures (the square shifted by 16 pixels in the second) in one pool, for `--animate`.
    fn make_anim_buffers(&mut self, qh: &QueueHandle<State>) -> std::io::Result<()> {
        let (w, h) = self.size;
        let one = (w * h * 4) as usize;
        let fd = memfd(2 * one)?;
        for k in 0..2 {
            let mut px = picture(w, h, self.colour);
            if k == 1 {
                px.rotate_right(16);
            }
            let bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
            // SAFETY: pwrite from our buffer into our own memfd at an offset inside its size.
            let n = unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), (k * one) as libc::off_t) };
            if n != bytes.len() as isize {
                return Err(std::io::Error::other(format!("pwrite {n} of {}", bytes.len())));
            }
        }
        let shm = self.shm.as_ref().ok_or_else(|| std::io::Error::other("no wl_shm"))?;
        let pool = shm.create_pool(fd.as_fd(), (2 * one) as i32, qh, ());
        let anim = self.anim.as_mut().unwrap();
        for (b, _) in anim.bufs.drain(..) {
            b.destroy();
        }
        for k in 0..2u8 {
            anim.bufs.push((pool.create_buffer((k as usize * one) as i32, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, k), false));
        }
        pool.destroy();
        Ok(())
    }

    /// One animation frame: the next free buffer (none free: only the callback), a new frame callback, commit.
    fn frame(&mut self, qh: &QueueHandle<State>) {
        let (w, h) = self.size;
        let s = self.surface.clone().unwrap();
        let anim = self.anim.as_mut().unwrap();
        let free = (0..anim.bufs.len()).map(|i| (anim.next + i) % anim.bufs.len()).find(|&i| !anim.bufs[i].1);
        match free {
            Some(i) => {
                anim.bufs[i].1 = true;
                anim.next = i + 1;
                s.attach(Some(&anim.bufs[i].0), 0, 0);
                s.damage_buffer(0, 0, w, h);
            }
            None => anim.starved += 1,
        }
        s.frame(qh, ());
        s.commit();
    }

    fn callback_done(&mut self, qh: &QueueHandle<State>) {
        let now = Instant::now();
        let anim = self.anim.as_mut().unwrap();
        if let Some(t) = anim.last.replace(now) {
            anim.gaps_ms.push((now - t).as_secs_f64() * 1e3);
        }
        if now >= anim.report_at {
            let n = anim.gaps_ms.len();
            let (p50, p99, max) = percentiles(&mut anim.gaps_ms);
            println!("frames n={n} p50_ms={p50:.1} p99_ms={p99:.1} max_ms={max:.1} released={} starved={}", anim.released, anim.starved);
            anim.gaps_ms.clear();
            anim.report_at = now + Duration::from_secs(2);
        }
        self.frame(qh);
    }

    fn draw(&mut self, qh: &QueueHandle<State>) -> std::io::Result<()> {
        if self.anim.is_some() {
            let first = self.anim.as_ref().unwrap().bufs.is_empty();
            self.make_anim_buffers(qh)?;
            if first {
                self.frame(qh);
            }
            return Ok(());
        }
        let (w, h) = self.size;
        let len = (w * h * 4) as usize;
        let fd = memfd(len)?;
        let px = picture(w, h, self.colour);
        let bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
        // SAFETY: pwrite from our buffer into our own memfd.
        let n = unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
        if n != bytes.len() as isize {
            return Err(std::io::Error::other(format!("pwrite {n} of {}", bytes.len())));
        }
        let shm = self.shm.as_ref().ok_or_else(|| std::io::Error::other("no wl_shm"))?;
        let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
        let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        let s = self.surface.as_ref().unwrap();
        s.attach(Some(&buffer), 0, 0);
        s.damage_buffer(0, 0, w, h);
        s.commit();
        if let Some(old) = self.buffer.replace(buffer) {
            old.destroy();
        }
        Ok(())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut title = "wyclient".to_string();
    let mut pattern = 0usize;
    let (mut animate, mut hang, mut stop, mut noread, mut ignore_term) = (false, None, None, None, false);
    let (mut fullscreen, mut unfullscreen, mut fs_at) = (false, false, None);
    let mut cursor = None;
    let mut it = args.iter();
    let ms = |v: &String| v.parse::<u64>().ok().map(Duration::from_millis);
    while let Some(a) = it.next() {
        if a == "--animate" {
            animate = true;
            continue;
        }
        if a == "--ignore-term" {
            ignore_term = true;
            continue;
        }
        if a == "--fullscreen" {
            fullscreen = true;
            continue;
        }
        if a == "--unfullscreen" {
            unfullscreen = true;
            continue;
        }
        match (a.as_str(), it.next()) {
            ("--title", Some(v)) => title = v.clone(),
            ("--pattern", Some(v)) => pattern = v.parse().unwrap_or(0),
            ("--hang", Some(v)) if ms(v).is_some() => hang = ms(v),
            ("--stop", Some(v)) if ms(v).is_some() => stop = ms(v),
            ("--noread", Some(v)) if ms(v).is_some() => noread = ms(v),
            ("--fullscreen-at", Some(v)) if ms(v).is_some() => fs_at = ms(v),
            ("--cursor", Some(v)) if ["hidden", "surface", "big"].contains(&v.as_str()) || cursor::shape(v).is_some() => cursor = Some(v.clone()),
            _ => {
                eprintln!("usage: wyclient [--title T] [--pattern N] [--animate] [--hang MS] [--stop MS] [--noread MS] [--ignore-term] [--fullscreen] [--unfullscreen] [--fullscreen-at MS] [--cursor NAME|hidden|surface|big]");
                std::process::exit(2);
            }
        }
    }
    let start = Instant::now();
    if ignore_term {
        // SAFETY: sets SIGTERM's disposition to SIG_IGN; no handler code, no memory touched.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }
    if let Some(d) = stop {
        std::thread::spawn(move || {
            std::thread::sleep(d);
            println!("stopping");
            // SAFETY: kill(2) of our own process with SIGSTOP; no memory is touched.
            unsafe { libc::kill(libc::getpid(), libc::SIGSTOP) };
        });
    }
    let conn = Connection::connect_to_env().unwrap_or_else(|e| {
        eprintln!("wyclient: no Wayland compositor: {e}");
        std::process::exit(1);
    });
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State { colour: COLOURS[pattern % COLOURS.len()], size: (400, 300), ..State::default() };
    st.hang_at = hang.map(|d| start + d);
    st.cursor = cursor;
    st.anim = animate.then(|| Anim {
        bufs: Vec::new(),
        next: 0,
        last: None,
        gaps_ms: Vec::new(),
        report_at: Instant::now() + Duration::from_secs(2),
        released: 0,
        starved: 0,
    });
    if let Err(e) = queue.roundtrip(&mut st) {
        eprintln!("wyclient: {e}");
        std::process::exit(1);
    }
    let (Some(comp), Some(wm)) = (st.compositor.clone(), st.wm.clone()) else {
        eprintln!("wyclient: the compositor has no wl_compositor or xdg_wm_base");
        std::process::exit(1);
    };
    let surface = comp.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    top.set_title(title);
    top.set_app_id("wyclient".into());
    if fullscreen {
        top.set_fullscreen(None);
    }
    if unfullscreen {
        top.unset_fullscreen();
    }
    st.top = Some(top);
    surface.commit();
    st.surface = Some(surface);
    st.xdg = Some(xdg);
    println!("ready");
    let noread_at = noread.map(|d| start + d);
    let mut ask_at = fs_at.map(|d| start + d);
    while !st.done {
        if noread_at.is_some_and(|t| Instant::now() >= t) {
            println!("not reading");
            // Alive but deaf: the socket fills up on bitscomp's side until it drops this client.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        if let (Some(t), Some(top)) = (ask_at, &st.top)
            && Instant::now() >= t
        {
            ask_at = None;
            top.set_fullscreen(None);
            println!("asked fullscreen (timer)");
        }
        let r = match [noread_at, ask_at].into_iter().flatten().min() {
            // Wake by the deadline even when nothing arrives.
            Some(t) => {
                let _ = conn.flush();
                match queue.prepare_read() {
                    Some(g) => {
                        let ms = t.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32;
                        let mut pfd = libc::pollfd { fd: g.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
                        // SAFETY: poll on one pollfd, a local that outlives the call.
                        if unsafe { libc::poll(&mut pfd, 1, ms) } > 0 { let _ = g.read(); } else { drop(g); }
                        queue.dispatch_pending(&mut st).map(|_| ())
                    }
                    None => queue.dispatch_pending(&mut st).map(|_| ()),
                }
            }
            None => queue.blocking_dispatch(&mut st).map(|_| ()),
        };
        if let Err(e) = r {
            eprintln!("wyclient: {e}");
            std::process::exit(1);
        }
    }
    println!("close");
}

impl State {
    /// WY-8: the keys a browser keeps for itself (evdev codes): F11 fullscreen and back, Esc out of fullscreen,
    /// F10 maximised and back.
    fn own_key(&self, key: u32) {
        let Some(top) = &self.top else { return };
        let (fullscreen, maximized, _) = self.states;
        match key {
            87 if fullscreen => top.unset_fullscreen(),
            87 => top.set_fullscreen(None),
            1 if fullscreen => top.unset_fullscreen(),
            67 => {
                for _ in 0..20 {
                    top.set_fullscreen(None);
                    top.unset_fullscreen();
                }
                println!("asked toggle 20");
                return;
            }
            68 if maximized => top.unset_maximized(),
            68 => top.set_maximized(),
            _ => return,
        }
        println!("asked {}", match key { 87 if fullscreen => "unfullscreen", 87 => "fullscreen", 1 => "unfullscreen", 68 if maximized => "unmaximize", _ => "maximize" });
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => st.wm = Some(reg.bind(name, 1, qh, ())),
                "wl_seat" => st.seat = Some(reg.bind(name, version.min(5), qh, ())),
                "wp_cursor_shape_manager_v1" => st.shape_mgr = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(st: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            if st.hang_at.is_some_and(|t| Instant::now() >= t) {
                if !st.ignoring {
                    st.ignoring = true;
                    println!("ignoring pings");
                }
                return;
            }
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(st: &mut Self, xs: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            xs.ack_configure(serial);
            let (w, h) = st.pending;
            if w > 0 && h > 0 {
                st.size = (w, h);
            }
            println!("configure {} {}", st.size.0, st.size.1);
            st.states = st.pending_states;
            let b = |v: bool| v as u8;
            println!("states fullscreen={} maximized={} activated={}", b(st.states.0), b(st.states.1), b(st.states.2));
            if let Err(e) = st.draw(qh) {
                eprintln!("wyclient: draw: {e}");
                std::process::exit(1);
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(st: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Configure { width, height, states } => {
                st.pending = (width, height);
                let has = |v: xdg_toplevel::State| states.chunks_exact(4).any(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) == v as u32);
                st.pending_states =
                    (has(xdg_toplevel::State::Fullscreen), has(xdg_toplevel::State::Maximized), has(xdg_toplevel::State::Activated));
            }
            xdg_toplevel::Event::Close => st.done = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(_: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(c) } = ev {
            if c.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if c.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
    }
}

fn state(s: WEnum<wl_keyboard::KeyState>) -> &'static str {
    match s {
        WEnum::Value(wl_keyboard::KeyState::Pressed) => "pressed",
        _ => "released",
    }
}

/// The `xkb_symbols "..."` name of a keymap text (which layout the compositor sent).
pub fn symbols_name(keymap: &str) -> &str {
    keymap.split("xkb_symbols").nth(1).and_then(|r| r.split('"').nth(1)).unwrap_or("-")
}

fn read_keymap(fd: OwnedFd, size: u32) -> String {
    let mut buf = vec![0u8; size as usize];
    // SAFETY: pread into our buffer of `size` bytes from the keymap fd we own.
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
    buf.truncate(n.max(0) as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(st: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                let text = read_keymap(fd, size);
                println!("keymap bytes={} symbols={}", text.len(), symbols_name(&text));
            }
            wl_keyboard::Event::Enter { .. } => println!("enter keyboard"),
            wl_keyboard::Event::Leave { .. } => println!("leave keyboard"),
            wl_keyboard::Event::Key { key, state: s, .. } => {
                println!("key {key} {}", state(s));
                if s == WEnum::Value(wl_keyboard::KeyState::Pressed) {
                    st.own_key(key);
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                println!("mods {mods_depressed} {mods_latched} {mods_locked} {group}")
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => println!("repeat rate={rate} delay={delay}"),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(st: &mut Self, p: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                println!("pointer enter {surface_x:.0} {surface_y:.0}");
                cursor::on_enter(st, p, serial, qh);
            }
            wl_pointer::Event::Leave { .. } => println!("pointer leave"),
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => println!("motion {surface_x:.0} {surface_y:.0}"),
            wl_pointer::Event::Button { button, state: s, .. } => {
                let p = matches!(s, WEnum::Value(wl_pointer::ButtonState::Pressed));
                println!("button {button} {}", if p { "pressed" } else { "released" });
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let a = if matches!(axis, WEnum::Value(wl_pointer::Axis::HorizontalScroll)) { "horizontal" } else { "vertical" };
                println!("axis {a} {value:.1}");
            }
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);

impl Dispatch<wl_buffer::WlBuffer, u8> for State {
    fn event(st: &mut Self, _: &wl_buffer::WlBuffer, ev: wl_buffer::Event, k: &u8, _: &Connection, _: &QueueHandle<Self>) {
        if let (wl_buffer::Event::Release, Some(a)) = (ev, st.anim.as_mut()) {
            a.released += 1;
            if let Some(b) = a.bufs.get_mut(*k as usize) {
                b.1 = false;
            }
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(st: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = ev
            && st.anim.is_some()
        {
            st.callback_done(qh);
        }
    }
}
delegate_noop!(State: ignore wl_surface::WlSurface);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_has_square_in_the_middle() {
        let px = picture(200, 100, COLOURS[1]);
        assert_eq!(px[0], COLOURS[1]);
        assert_eq!(px[50 * 200 + 100], 0x00ff_ffff);
        assert_eq!(px[50 * 200 + 100 - 33], COLOURS[1]);
    }

    #[test]
    fn frame_percentiles() {
        assert_eq!(percentiles(&mut []), (0.0, 0.0, 0.0));
        let mut v: Vec<f64> = (1..=100).map(f64::from).rev().collect();
        assert_eq!(percentiles(&mut v), (51.0, 99.0, 100.0));
        assert_eq!(percentiles(&mut [16.7]), (16.7, 16.7, 16.7));
    }

    #[test]
    fn keymap_symbols() {
        assert_eq!(symbols_name("xkb_keymap {\n xkb_symbols \"pc+de+inet(evdev)\" {"), "pc+de+inet(evdev)");
        assert_eq!(symbols_name("nothing"), "-");
    }
}
