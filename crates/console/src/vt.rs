//! The VT itself: the one text screen on the display (`write`), and its input thread (`vt-input`, Interactive):
//! evdev events -> held-key union -> keymap -> line discipline (echo drawn here), key repeat by its own timed
//! wait, Shift+PageUp/PageDown scrollback and the mouse pointer.

use crate::TextGrid;
use crate::evdev::{self, EV_ABS, EV_KEY, EV_REL, EV_SYN, InputEvent, KeyUnion};
use crate::font::{Face, Font};
use crate::keymap::{self, Key, Mods, code};
use crate::render::{self, Pointer};
use crate::repeat::Repeat;
use crate::screen::{self, Screen};
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use ksync::SpinLock;
use crate::latency::Path;
use display::Rect;

/// The face used until CONFIG or the monitor's `font` picks another: Noto (182x45 at 1440 lines). MCP `screen`
/// text mode reads the VT's own text grid since INT-A1 (IN-3 change request 2), so it no longer needs VGA.
pub const DEFAULT_FACE: Face = Face::Noto;

struct Vt {
    screen: Screen,
    font: Font,
    pointer: Pointer,
    last_cursor: Option<(u32, u32)>,
    width: u32,
    height: u32,
}

static VT: SpinLock<Option<Vt>> = SpinLock::named("console.vt", None);
static FACE: AtomicU8 = AtomicU8::new(0);

fn face() -> Face {
    match FACE.load(Ordering::Acquire) {
        1 => Face::Vga,
        2 => Face::Noto,
        _ => DEFAULT_FACE,
    }
}

/// Runs `f` on the VT (made at the frame's size on first use); None without a display.
fn with_vt<R>(f: impl FnOnce(&mut Vt) -> R) -> Option<R> {
    let mut g = VT.lock();
    if g.is_none() {
        let (w, h) = display::with_frame(|fr| (fr.width, fr.height))?;
        let font = Font::for_mode(face(), w, h);
        let (cols, rows) = (w / font.w, h / font.h);
        if cols == 0 || rows == 0 {
            return None;
        }
        let mut p = Pointer::new();
        p.scale = (font.h / 16).max(1);
        p.x = w / 2;
        p.y = h / 2;
        *g = Some(Vt { screen: Screen::new(cols, rows), font, pointer: p, last_cursor: None, width: w, height: h });
    }
    g.as_mut().map(f)
}

/// Draws what changed (pointer kept on top) into the frame. Caller presents after releasing `console.vt`. While
/// a program has the screen (KD_GRAPHICS) nothing is drawn: the model keeps changing and is drawn whole when the
/// VT takes the screen back.
fn redraw(v: &mut Vt) {
    if graphics() {
        v.screen.full = true;
        return;
    }
    let Vt { screen, font, pointer, last_cursor, .. } = v;
    display::with_frame(|f| {
        pointer.hide(f);
        render::render(f, font, screen, last_cursor);
        pointer.show(f);
    });
}

/// Hands the frame to the display, unless a program has the screen. On the input thread the frame is the
/// reaction to the input it just took (IN-6 latency); a frame held back by another scanout owner shows nothing.
fn present() {
    present_as(Path::Console);
}

/// `present` of a frame on latency path `path`. Whether it was held back is read from DP's held-skip counter
/// around the call: another thread's held present in between can only make a shown frame look held (its input
/// then stays pending for the next frame or is forgotten at the batch's end), never the other way round.
fn present_as(path: Path) {
    if !graphics() {
        let stamp = crate::latency::leaving();
        let held = stamp.map(|_| display::stats().held_skips);
        display::present();
        match held {
            Some(h) if display::stats().held_skips != h => crate::latency::not_shown(stamp),
            _ => crate::latency::left(stamp, path),
        }
    }
}

// ---- the screen and keyboard owner (IN-4: KDSETMODE, KDSKBMODE; DP-2's hand-over)

static GRAPHICS: AtomicBool = AtomicBool::new(false);
/// The process that set KD_GRAPHICS (0 = the kernel or none): when it ends, the VT takes the screen back.
static KD_OWNER: AtomicU32 = AtomicU32::new(0);
static KB_MODE: AtomicU8 = AtomicU8::new(crate::termios::ioc::K_UNICODE as u8);
static KB_OWNER: AtomicU32 = AtomicU32::new(0);

/// A program has the screen (KD_GRAPHICS): the VT draws nothing and types nothing for the keyboard.
pub fn graphics() -> bool {
    GRAPHICS.load(Ordering::Acquire)
}

/// KDGETMODE.
pub fn kd_mode() -> u64 {
    if graphics() { crate::termios::ioc::KD_GRAPHICS } else { crate::termios::ioc::KD_TEXT }
}

/// Changes of the screen and keyboard owners: an owner and its mode change together, so `reclaim` (the VT thread)
/// can never take back what a new program got between its check and its reset.
static OWNERS: SpinLock<()> = SpinLock::named("vt.owners", ());

/// KDSETMODE (and DP-2's `hand_over`/`take_back`): KD_GRAPHICS gives the screen to a fullscreen program, KD_TEXT
/// takes it back and draws the console again. `owner`: the process that asked (the VT takes the screen back
/// once it ends, as the fullscreen program's crash must not leave the screen dark).
pub fn set_kd_mode(on: bool, owner: Option<u32>) {
    set_kd_mode_if(on, owner, None);
}

/// `set_kd_mode` only while the owner is still `expect` (Some): false when someone else took it meanwhile.
fn set_kd_mode_if(on: bool, owner: Option<u32>, expect: Option<u32>) -> bool {
    let changed = {
        let _g = OWNERS.lock();
        if expect.is_some_and(|e| KD_OWNER.load(Ordering::Acquire) != e) {
            return false;
        }
        KD_OWNER.store(if on { owner.unwrap_or(0) } else { 0 }, Ordering::Release);
        GRAPHICS.swap(on, Ordering::AcqRel) != on
    };
    if !changed {
        return true;
    }
    klog::event!(Info, "console", "vt.mode", mode = if on { "graphics" } else { "text" }, owner = owner.unwrap_or(0));
    if !on {
        // The arrow drawn over the program's picture: that spot comes back from the console frame too.
        if let Some(r) = crate::pointer_over::forget() {
            display::with_frame(|f| f.mark(r));
        }
        with_vt(|v| {
            v.screen.full = true;
            v.last_cursor = None;
            redraw(v);
        });
        present();
    }
    true
}

/// DP-2: a fullscreen program (fbdev) takes the screen; `owner` its pid.
pub fn hand_over(owner: u32) {
    set_kd_mode(true, Some(owner));
}
/// DP-2: the console takes the screen back.
pub fn take_back() {
    set_kd_mode(false, None);
}

/// KDGKBMODE.
pub fn kb_mode() -> u64 {
    KB_MODE.load(Ordering::Acquire) as u64
}
/// KDSKBMODE (K_RAW, K_XLATE, K_MEDIUMRAW, K_UNICODE, K_OFF).
pub fn set_kb_mode(m: u8, owner: Option<u32>) {
    set_kb_mode_if(m, owner, None);
}

fn set_kb_mode_if(m: u8, owner: Option<u32>, expect: Option<u32>) -> bool {
    let k = crate::termios::ioc::K_UNICODE as u8;
    let changed = {
        let _g = OWNERS.lock();
        if expect.is_some_and(|e| KB_OWNER.load(Ordering::Acquire) != e) {
            return false;
        }
        KB_OWNER.store(if m != k && m != crate::termios::ioc::K_XLATE as u8 { owner.unwrap_or(0) } else { 0 }, Ordering::Release);
        KB_MODE.swap(m, Ordering::AcqRel) != m
    };
    if changed {
        klog::event!(Info, "console", "vt.kbmode", mode = m, owner = owner.unwrap_or(0));
    }
    true
}

/// The owners of KD_GRAPHICS and of a raw or off keyboard ended: back to the console (the VT thread asks every
/// second; Linux leaves this to the next program, BitsOS does not leave Georg without a console). Each reset
/// happens only if the owner is still the one found dead.
pub fn reclaim() {
    let p = crate::jobs::procs();
    let o = KD_OWNER.load(Ordering::Acquire);
    if o != 0 && !p.alive(o) && set_kd_mode_if(false, None, Some(o)) {
        klog::event!(Info, "console", "vt.reclaimed", what = "screen", owner = o; "the program that had the screen ended");
    }
    let k = KB_OWNER.load(Ordering::Acquire);
    if k != 0 && !p.alive(k) && set_kb_mode_if(crate::termios::ioc::K_UNICODE as u8, None, Some(k)) {
        klog::event!(Info, "console", "vt.reclaimed", what = "keyboard", owner = k; "the program that set the keyboard mode ended");
    }
    crate::vtioctl::reclaim(&|pid| p.alive(pid));
}

/// The end of a character a program wrote only partly (UTF-8 split across two writes): kept until the next write.
static UTF8_CARRY: SpinLock<crate::utf8::Carry> = SpinLock::named("console.utf8", crate::utf8::Carry::new());

/// A program's output (and the terminal's echo) after the line discipline's output processing: a newline here is
/// a line feed only (the VT's LF; ONLCR already added the carriage return when it is on), drawn with the
/// screen's `\x0b` (Linux's VT treats LF, VT and FF alike).
pub fn write_program(bytes: &[u8]) {
    let text = UTF8_CARRY.lock().feed(bytes);
    if text.is_empty() {
        return;
    }
    let text = text.replace('\n', "\u{b}");
    write(&text, screen::FG_INDEX);
}

/// Reports a program asked the screen for (cursor position, status, identity), for the terminal's input.
pub fn take_reply() -> alloc::vec::Vec<u8> {
    with_vt(|v| v.screen.take_reply()).unwrap_or_default()
}

pub fn write(s: &str, attr_fg: u8) {
    let drew = with_vt(|v| {
        let keep = v.screen.attr;
        v.screen.attr = attr_fg & 15 | keep & 0xF0;
        v.screen.write(s);
        // ANSI SGR inside `s` may have changed the attribute; the next writer starts from the default again.
        v.screen.attr = keep;
        redraw(v);
    });
    if drew.is_some() {
        present();
    }
}

/// A log line: goes above the open line (the prompt and what Georg is typing, or a program's unfinished line),
/// which is drawn again below it (`Screen::write_above`). Everything happens in the model under `console.vt`, the
/// lock every key's echo takes too, so a key typed meanwhile lands either before or after, never inside.
pub fn write_line(line: &str, attr_fg: u8) {
    let drew = with_vt(|v| {
        v.screen.write_above(line, attr_fg & 15);
        redraw(v);
    });
    if drew.is_some() {
        present();
    }
}

/// Panic/NMI path: no wait, no allocation, the model untouched. Draws at the cursor straight into the frame,
/// scrolling the pixels when the bottom is reached.
pub fn write_emergency(line: &str, attr_fg: u8) {
    let Some(mut g) = VT.try_lock() else { return };
    let Some(v) = g.as_mut() else { return };
    let (font, cols, rows) = (v.font, v.screen.cols, v.screen.rows);
    let (mut col, mut row) = (0u32, v.screen.row);
    let attr = attr_fg & 15;
    let drew = display::try_with_frame(|f| {
        v.pointer.hide(f);
        let newline = |row: &mut u32, f: &mut display::RamFrame| {
            if *row + 1 < rows {
                *row += 1;
            } else {
                f.scroll_up(font.h, screen::PALETTE[0]);
            }
        };
        if v.screen.col != 0 {
            newline(&mut row, f);
        }
        for c in line.chars() {
            if col >= cols {
                col = 0;
                newline(&mut row, f);
            }
            render::draw_cell(f, &font, col, row, screen::Cell { ch: c, attr }, false);
            col += 1;
        }
        newline(&mut row, f);
    });
    if drew.is_some() {
        // The model no longer matches the pixels; the next normal write redraws everything.
        v.screen.full = true;
        v.screen.row = row;
        v.screen.col = 0;
    }
    drop(g);
    display::try_present();
}

pub fn size() -> (u32, u32) {
    with_vt(|v| (v.screen.cols, v.screen.rows)).unwrap_or((0, 0))
}

pub fn text_grid() -> Option<TextGrid> {
    with_vt(|v| (v.screen.cols, v.screen.rows, v.screen.text_lines(), (v.screen.col, v.screen.row)))
}

/// The font in use (face, cell width, cell height).
pub fn font() -> Option<Font> {
    with_vt(|v| v.font)
}

/// Switches the face: the text stays (cut to the new width), the whole screen is drawn again.
pub fn set_face(f: Face) {
    FACE.store(if f == Face::Vga { 1 } else { 2 }, Ordering::Release);
    let changed = with_vt(|v| {
        let font = Font::for_mode(f, v.width, v.height);
        if font == v.font {
            return None;
        }
        v.font = font;
        v.screen.resize(v.width / font.w, v.height / font.h);
        v.pointer.scale = (font.h / 16).max(1);
        v.last_cursor = None;
        redraw(v);
        Some((font.w, font.h, v.screen.cols, v.screen.rows))
    })
    .flatten();
    if let Some((w, h, cols, rows)) = changed {
        present();
        klog::event!(Info, "console", "font", face = f.name(), cell_w = w, cell_h = h, cols = cols, rows = rows);
        // The grid changed: a size a program set is dropped, and the foreground group hears of it.
        if crate::tty().set_winsize(None, crate::ttyioctl::vt_winsize()) || crate::tty().winsize().is_none() {
            crate::ttyioctl::signal_fg(crate::tty(), crate::jobs::procs(), crate::jobs::SIGWINCH);
        }
    }
}

/// GX-4 (CR 1): the display shows a new size (a mode switch; `display::resize` made the frame again): the VT is
/// refitted to it as `set_face` refits to a font: the font for the new mode, the text kept (cut or padded),
/// the pointer kept inside, the whole screen drawn again; the foreground group hears SIGWINCH (Linux
/// `vc_do_resize` -> `tty_do_resize`, linux-6.6/drivers/tty/vt/vt.c:1144, drivers/tty/tty_io.c:2326).
pub fn refit() {
    let Some((w, h)) = display::with_frame(|fr| (fr.width, fr.height)) else { return };
    let changed = with_vt(|v| {
        if (v.width, v.height) == (w, h) {
            return None;
        }
        let font = Font::for_mode(face(), w, h);
        let (cols, rows) = (w / font.w, h / font.h);
        if cols == 0 || rows == 0 {
            return None;
        }
        v.width = w;
        v.height = h;
        v.font = font;
        v.screen.resize(cols, rows);
        v.pointer.scale = (font.h / 16).max(1);
        v.pointer.x = v.pointer.x.min(w - 1);
        v.pointer.y = v.pointer.y.min(h - 1);
        v.last_cursor = None;
        redraw(v);
        Some((font.w, font.h, cols, rows))
    })
    .flatten();
    if let Some((cw, ch, cols, rows)) = changed {
        present();
        klog::event!(Info, "console", "refit", width = w, height = h, cell_w = cw, cell_h = ch, cols = cols, rows = rows);
        if crate::tty().set_winsize(None, crate::ttyioctl::vt_winsize()) || crate::tty().winsize().is_none() {
            crate::ttyioctl::signal_fg(crate::tty(), crate::jobs::procs(), crate::jobs::SIGWINCH);
        }
    }
}

/// The share of free memory the scrollback may take at most (`free / SCROLLBACK_SHARE`).
pub const SCROLLBACK_SHARE: u64 = 8;

/// Heap bytes `rows` scrollback rows of `cols` cells take (cells plus the row's own vector).
pub fn scrollback_bytes(rows: usize, cols: u32) -> u64 {
    let row = cols as u64 * core::mem::size_of::<screen::Cell>() as u64 + core::mem::size_of::<alloc::vec::Vec<screen::Cell>>() as u64;
    (rows as u64).saturating_mul(row)
}

/// Sets the scrollback window. Refused when the rows (at the widest face's width, so a later `font` switch
/// cannot grow past it) would take more than an eighth of the free memory: the window fills up as text scrolls,
/// and an allocation failure later in a log line must not be possible to ask for.
pub fn set_scrollback(rows: usize) -> Result<(), alloc::string::String> {
    // Asked before `console.vt` is taken (no allocator statistics under the VT's lock).
    let room = mm_core::frames().map(|fr| fr.stats().free_bytes / SCROLLBACK_SHARE);
    let r = with_vt(|v| {
        let cols = [Face::Vga, Face::Noto].iter().map(|&f| v.width / Font::for_mode(f, v.width, v.height).w.max(1)).max().unwrap_or(v.screen.cols);
        let need = scrollback_bytes(rows, cols.max(v.screen.cols));
        if let Some(room) = room
            && need > room
        {
            return Err((need, room));
        }
        v.screen.set_limit(rows);
        redraw(v);
        Ok(())
    });
    match r {
        Some(Err((need, room))) => {
            klog::event!(Warn, "console", "scrollback.refused", rows = rows, need_kib = need / 1024, room_kib = room / 1024;
                "the scrollback would take more than an eighth of the free memory");
            Err(alloc::format!("{rows} rows need {} KiB, more than an eighth of the free memory ({} KiB)", need / 1024, room / 1024))
        }
        Some(Ok(())) => {
            present();
            klog::event!(Info, "console", "scrollback.set", rows = rows);
            Ok(())
        }
        None => Err(alloc::string::String::from("no display")),
    }
}

/// Scrollback rows allowed and rows in use.
pub fn scrollback() -> (usize, u32) {
    with_vt(|v| (v.screen.limit, v.screen.scrollback_len())).unwrap_or((0, 0))
}

pub fn clear() {
    with_vt(|v| {
        v.screen.clear();
        redraw(v);
    });
    present();
}

/// Moves the scrollback view (positive = back); returns the rows looked back now.
pub fn scroll_view(delta: i64) -> u32 {
    let r = with_vt(|v| {
        if v.screen.scroll_view(delta) {
            redraw(v);
        }
        v.screen.view
    });
    present();
    r.unwrap_or(0)
}

fn view_to_bottom() {
    let moved = with_vt(|v| {
        let m = v.screen.view_to_bottom();
        if m {
            redraw(v);
        }
        m
    });
    if moved == Some(true) {
        present();
    }
}

/// Pointer position (frame pixels) and whether it is shown.
pub fn pointer() -> Option<(u32, u32, bool)> {
    with_vt(|v| (v.pointer.x, v.pointer.y, v.pointer.visible))
}

/// Where a pointer move went.
enum Moved {
    No,
    /// Drawn into the console frame: present it.
    Console,
    /// A program has the screen: drawn over its picture (`pointer_over`), outside `console.vt`.
    /// `epoch`: `pointer_over::generation()` before KD_GRAPHICS was seen (a KD_TEXT meanwhile cancels the move).
    OverProgram { x: u32, y: u32, scale: u32, w: u32, h: u32, stale: Option<Rect>, epoch: u64 },
}

/// Moves the pointer by (dx, dy) or to (x, y), clamped to the screen, and shows it. While a program has the screen
/// (KD_GRAPHICS) the kernel draws it over the program's picture (IN-6, B4: the pointer moves without any program).
fn move_pointer(to: impl FnOnce(i64, i64, i64, i64) -> (i64, i64)) {
    let moved = with_vt(|v| {
        let (w, h) = (v.width as i64, v.height as i64);
        let (x, y) = to(v.pointer.x as i64, v.pointer.y as i64, w, h);
        let (x, y) = (x.clamp(0, w - 1) as u32, y.clamp(0, h - 1) as u32);
        if (x, y) == (v.pointer.x, v.pointer.y) && v.pointer.visible {
            return Moved::No;
        }
        let epoch = crate::pointer_over::generation();
        if graphics() {
            // The console frame is not shown: its arrow comes out (drawn again when the VT takes the screen back).
            let stale = v.pointer.drawn_at();
            if stale.is_some() {
                display::with_frame(|f| v.pointer.hide(f));
            }
            v.pointer.x = x;
            v.pointer.y = y;
            v.pointer.visible = true;
            return Moved::OverProgram { x, y, scale: v.pointer.scale, w: v.width, h: v.height, stale, epoch };
        }
        display::with_frame(|f| {
            v.pointer.hide(f);
            v.pointer.x = x;
            v.pointer.y = y;
            v.pointer.visible = true;
            v.pointer.show(f);
        });
        Moved::Console
    });
    match moved {
        Some(Moved::Console) => present_as(Path::Pointer),
        Some(Moved::OverProgram { x, y, scale, w, h, stale, epoch }) => crate::pointer_over::move_to(x, y, scale, w, h, stale, epoch),
        _ => {}
    }
}

// ---- input

static STARTED: AtomicBool = AtomicBool::new(false);

/// (min, max) of ABS_X and of ABS_Y.
type AbsRange = ((i32, i32), (i32, i32));

/// What the input side keeps between events.
pub struct Input {
    pub union: KeyUnion,
    pub repeat: Repeat,
    /// Per device: (min, max) of ABS_X and ABS_Y, for scaling absolute pointers to the screen.
    abs: BTreeMap<u32, AbsRange>,
    /// Pointer motion of the report being read, applied at its SYN_REPORT (one redraw per report, not per axis).
    motion: Motion,
}

/// Collected pointer motion: relative steps and absolute targets (frame pixels).
#[derive(Clone, Copy, Default)]
struct Motion {
    dx: i64,
    dy: i64,
    x: Option<i64>,
    y: Option<i64>,
}

impl Input {
    pub fn new() -> Input {
        Input { union: KeyUnion::new(), repeat: Repeat::new(), abs: BTreeMap::new(), motion: Motion::default() }
    }

    pub fn mods(&self) -> Mods {
        let locks = evdev::sink().locks();
        let h = |c| self.union.held(c);
        Mods {
            shift: h(code::LEFTSHIFT) || h(code::RIGHTSHIFT),
            ctrl: h(code::LEFTCTRL) || h(code::RIGHTCTRL),
            alt: h(code::LEFTALT),
            altgr: h(code::RIGHTALT),
            caps: locks & (1 << evdev::LED_CAPSL) != 0,
            num: locks & (1 << evdev::LED_NUML) != 0,
        }
    }

    /// One event from the sink.
    pub fn event(&mut self, ev: &InputEvent, now: u64) {
        match ev.kind {
            EV_KEY if ev.code >= 0x100 => {} // mouse buttons: nothing on the console yet
            EV_KEY => match self.union.apply(ev) {
                Some(true) if raw_keyboard() => self.raw_edge(ev.code, true, ev.injected, now),
                Some(false) if raw_keyboard() => {
                    self.raw_edge(ev.code, false, ev.injected, now);
                    self.repeat.release(ev.code);
                }
                Some(true) => self.press(ev.code, ev.injected, now),
                Some(false) => self.repeat.release(ev.code),
                None => {}
            },
            EV_REL => match ev.code {
                0 => self.motion.dx = self.motion.dx.saturating_add(ev.value as i64),
                1 => self.motion.dy = self.motion.dy.saturating_add(ev.value as i64),
                _ => {}
            },
            EV_ABS if ev.code <= 1 => {
                let (rx, ry) = self.abs_range(ev);
                let (w, h) = with_vt(|v| (v.width as i64, v.height as i64)).unwrap_or((1, 1));
                let (r, size) = if ev.code == 0 { (rx, w) } else { (ry, h) };
                let span = (r.1 as i64 - r.0 as i64).max(1);
                let at = (ev.value as i64 - r.0 as i64) * (size - 1) / span;
                if ev.code == 0 {
                    self.motion.x = Some(at);
                } else {
                    self.motion.y = Some(at);
                }
            }
            EV_SYN => self.flush_motion(),
            _ => {}
        }
    }

    /// Moves the pointer by what the report collected (also called when a batch of events ends without a
    /// SYN_REPORT, e.g. an injected event).
    pub fn flush_motion(&mut self) {
        let m = core::mem::take(&mut self.motion);
        if m.dx == 0 && m.dy == 0 && m.x.is_none() && m.y.is_none() {
            return;
        }
        move_pointer(|x, y, _, _| (m.x.unwrap_or(x).saturating_add(m.dx), m.y.unwrap_or(y).saturating_add(m.dy)));
    }

    /// The ABS_X/ABS_Y ranges of the event's device. MCP's injected pointer reports frame pixels (MC-3).
    fn abs_range(&mut self, ev: &InputEvent) -> AbsRange {
        if ev.injected {
            let (w, h) = with_vt(|v| (v.width, v.height)).unwrap_or((1, 1));
            return ((0, w as i32 - 1), (0, h as i32 - 1));
        }
        if let Some(r) = self.abs.get(&ev.device) {
            return *r;
        }
        let dev = evdev::devices().into_iter().find(|d| d.number == ev.device);
        let get = |c| dev.as_ref().and_then(|d| d.info.caps.abs.get(&c).map(|a| (a.min, a.max))).unwrap_or((0, 32767));
        let r = (get(0), get(1));
        self.abs.insert(ev.device, r);
        r
    }

    /// K_RAW / K_MEDIUMRAW: every key edge goes to the terminal as codes (a program reads the keyboard itself).
    fn raw_edge(&mut self, c: u16, down: bool, injected: bool, now: u64) {
        let mut b = [0u8; 8];
        let n = if kb_mode() == crate::termios::ioc::K_RAW {
            let alt = self.union.held(code::LEFTALT) || self.union.held(code::RIGHTALT);
            crate::rawkeys::raw(c, down, alt, &mut b)
        } else {
            crate::rawkeys::mediumraw(c, down, &mut b)
        };
        if n > 0 {
            crate::tty().input(&b[..n], injected, &mut |e| write_program(e));
        }
        if down {
            self.repeat_settings();
            self.repeat.press(c, injected, now);
        }
    }

    fn press(&mut self, c: u16, injected: bool, now: u64) {
        // Focus (IN-4): a program that took the screen (KD_GRAPHICS) or switched the keyboard off (K_OFF) gets
        // the keys some other way (evdev, a grab); the VT types nothing meanwhile.
        if graphics() || kb_mode() == crate::termios::ioc::K_OFF {
            self.repeat.stop();
            return;
        }
        let m = self.mods();
        if keymap::is_modifier(c) {
            return;
        }
        if m.shift && (c == code::PAGEUP || c == code::PAGEDOWN) {
            // The newest key owns the repeat (V1, Linux): scrolling repeats nothing, so the repeat stops.
            self.repeat.stop();
            let rows = size().1.max(2) as i64 / 2;
            scroll_view(if c == code::PAGEUP { rows } else { -rows });
            return;
        }
        let k = keymap::translate(c, &m);
        if k == Key::None {
            // A newer key that types nothing still takes the repeat over from the key held before.
            self.repeat.stop();
            return;
        }
        view_to_bottom();
        self.repeat_settings();
        self.repeat.press(c, injected, now);
        type_key(k, injected);
    }

    /// When the input thread must look again at the latest (the next repeat).
    pub fn deadline(&self) -> Option<u64> {
        self.repeat.deadline()
    }

    /// KDKBDREP's settings for the next repeat.
    fn repeat_settings(&mut self) {
        let (d, r) = crate::repeat::rate_ms();
        self.repeat.delay_ns = d as u64 * 1_000_000;
        self.repeat.rate_ns = r as u64 * 1_000_000;
    }

    /// The repeat due now, if any.
    pub fn tick(&mut self, now: u64) {
        if let Some(c) = self.repeat.due(now) {
            if raw_keyboard() {
                let mut b = [0u8; 8];
                let n = if kb_mode() == crate::termios::ioc::K_RAW {
                    crate::rawkeys::raw(c, true, self.mods().alt, &mut b)
                } else {
                    crate::rawkeys::mediumraw(c, true, &mut b)
                };
                crate::tty().input(&b[..n], false, &mut |e| write_program(e));
                return;
            }
            if graphics() || kb_mode() == crate::termios::ioc::K_OFF {
                self.repeat.stop();
                return;
            }
            let k = keymap::translate(c, &self.mods());
            if k != Key::None {
                type_key(k, false);
            }
        }
    }
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

/// The keyboard sends key codes (K_RAW, K_MEDIUMRAW) instead of characters.
fn raw_keyboard() -> bool {
    let m = kb_mode();
    m == crate::termios::ioc::K_RAW || m == crate::termios::ioc::K_MEDIUMRAW
}

/// A typed key into the line discipline; its echo onto the screen, drawn before a finished line (or Ctrl+C)
/// reaches the reader, so the monitor's answer never lands on the command's own line.
fn type_key(k: Key, injected: bool) {
    crate::tty().key_with(k, injected, &mut |echo| write_program(echo));
}

static CLIENT: ksync::Once<alloc::sync::Arc<evdev::Client>> = ksync::Once::new();

/// Events waiting for the VT (tests wait for 0).
pub fn input_pending() -> usize {
    CLIENT.get().map(|c| c.pending()).unwrap_or(0)
}

fn input_thread() {
    crate::latency::set_input_thread();
    let sink = evdev::sink();
    let client = CLIENT.get_or_init(|| sink.subscribe("vt")).clone();
    // Num Lock on at start (the keypad types digits), as most PCs leave it after POST.
    sink.set_locks(sink.locks() | 1 << evdev::LED_NUML);
    let mut st = Input::new();
    klog::event!(Info, "console", "vt.started", face = face().name());
    loop {
        let now = time::now_ns();
        let wait = st.deadline().map(|d| d.saturating_sub(now)).unwrap_or(u64::MAX).clamp(1_000_000, 1_000_000_000);
        client.wait(wait);
        if client.take_resync() {
            // Events were dropped: rebuild the held keys from the sink's own union (exact) and stop repeating.
            st.union = sink.union_snapshot();
            st.repeat.stop();
            klog::event!(Warn, "console", "vt.resync"; "input events were dropped before the VT read them; held keys rebuilt");
        }
        while let Some(ev) = client.pop() {
            let now = time::now_ns();
            crate::latency::consumed(ev.time_ns, now);
            st.event(&ev, now);
        }
        st.flush_motion();
        crate::latency::batch_done();
        st.tick(time::now_ns());
    }
}

/// How often `vt-house` looks after the evdev nodes and the owners of screen and keyboard.
const HOUSE_NS: u64 = 100_000_000;

/// IN-4's housekeeping, out of the input thread since IN-6 (that one is KernelRt and must not touch the VFS or
/// the process table): the evdev nodes once devtmpfs is there (cheap when nothing changed), and the screen and
/// keyboard back from a program that ended without giving them back.
fn house_thread() {
    loop {
        crate::evdevfs::sync();
        reclaim();
        sched::sleep_ns(HOUSE_NS);
    }
}

/// Starts the input thread and the monitor (Service level, after the drivers' init calls).
pub fn start() -> Result<(), &'static str> {
    if STARTED.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    // Normally attached by `configure` already (then this does nothing).
    crate::attach_log();
    // IN-6: keep the kernel's arrow on top of a program that keeps presenting, and time the program path.
    display::set_owner_present_hook(&(crate::pointer_over::on_owner_present as display::OwnerPresentHook));
    // B4 (IN-6): the input thread is KernelRt: it only waits on its client (bounded) and draws.
    if let Err(e) = sched::spawn("vt-input", sched::Class::KernelRt, input_thread) {
        klog::event!(Error, "console", "vt.spawn_failed", error = alloc::format!("{e:?}"));
        return Err("vt-input thread");
    }
    if let Err(e) = sched::spawn("vt-house", sched::Class::Normal, house_thread) {
        // Without it dead owners keep the screen until a KD_TEXT and new evdev nodes wait for the next sync.
        klog::event!(Error, "console", "vt.spawn_failed", thread = "vt-house", error = alloc::format!("{e:?}"));
    }
    // Key repeat for evdev readers; without it programs only lose repeats, the console keeps working.
    let _ = crate::evrepeat::start();
    crate::monitor::start()
}
kreg::initcall!(kreg::Level::Service, "console-vt", start);
