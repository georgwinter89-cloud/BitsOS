//! WY-3: the clipboard between X11 and Wayland (ICCCM 2: CLIPBOARD, TARGETS, INCR). An X owner's text is offered
//! to Wayland clients as bitscomp's own selection and fetched from X when one reads it; a Wayland client's text is
//! offered to X clients with bitscomp's WM window as the owner and fetched from the client when an X client asks.
//! Text only for now (UTF8_STRING, TEXT, STRING and the text/plain types; STRING is Latin-1 and converted both
//! ways); PRIMARY is not bridged (decision in plan/notes/wy-WY-3.md). Every transfer goes through calloop sources:
//! bitscomp never waits for a client, and a transfer that makes no progress for TRANSFER_IDLE or grows past
//! MAX_TRANSFER is given up (B4).

use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{ErrorKind, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::wayland::selection::data_device::{clear_data_device_selection, set_data_device_selection};

use crate::state::App;
use crate::xproto::{self as xp, u32_at};
use crate::xwm::{Atoms, Want as XWant, Xwm};

/// The largest clipboard bridged in either direction (64 MiB); GetProperty asks for that many bytes at once.
pub(crate) const MAX_TRANSFER: usize = 64 << 20;
const READ_UNITS: u32 = (MAX_TRANSFER / 4) as u32;
/// A transfer whose other side does nothing for this long is given up.
pub(crate) const TRANSFER_IDLE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum Want {
    Targets(u32),
    Name(u32, u32),
    /// The reply for the X-to-Wayland transfer with this id (a reply of a given-up one is dropped).
    Data(u64),
    Incr(u64),
}

#[derive(Debug, PartialEq)]
enum Step {
    Idle,
    /// TARGETS of this owner generation asked.
    Targets(u32),
    /// The data in this target asked.
    Data(u32),
}

/// A Wayland client waiting for X data.
struct Reading {
    id: u64,
    fd: OwnedFd,
    data: Vec<u8>,
    incr: bool,
    target: u32,
    last: Instant,
}

pub struct Sel {
    pub handle: Option<LoopHandle<'static, App>>,
    generation: u32,
    /// The time of the current X owner's change (XFixes): bitscomp's conversions carry it (ICCCM 2.4).
    time: u32,
    step: Step,
    /// Bitscomp offers an X owner's selection to Wayland: its mime types and the X targets behind them.
    pub(crate) offered: bool,
    targets: Vec<(String, u32)>,
    naming: usize,
    /// Wayland clients waiting for X data (one conversion at a time).
    reads: VecDeque<(u32, OwnedFd)>,
    reading: Option<Reading>,
    next_id: u64,
    /// Bitscomp's WM window owns CLIPBOARD for a Wayland client's selection with these mime types.
    pub(crate) owned: bool,
    pub(crate) wl_mimes: Vec<String>,
    pub(crate) outgoing: Vec<crate::xsel_serve::Outgoing>,
}

impl Default for Sel {
    fn default() -> Self {
        Sel {
            handle: None,
            generation: 0,
            time: 0,
            step: Step::Idle,
            offered: false,
            targets: Vec::new(),
            naming: 0,
            reads: VecDeque::new(),
            reading: None,
            next_id: 0,
            owned: false,
            wl_mimes: Vec::new(),
            outgoing: Vec::new(),
        }
    }
}

impl Sel {
    pub fn watches(&self, win: u32) -> bool {
        self.outgoing.iter().any(|o| o.requestor == win)
    }

    /// One line for `ctl xwayland`.
    pub fn describe(&self) -> String {
        let dir = if self.offered { "x11->wayland" } else if self.owned { "wayland->x11" } else { "none" };
        format!("clipboard={dir} targets={} transfers={}", self.targets.len(), self.reads.len() + self.reading.is_some() as usize + self.outgoing.len())
    }

    /// The X owner's transfer stops: its reader gets EOF (its fd closes), the queue goes, nothing is waited for.
    fn drop_reads(&mut self) {
        self.reading = None;
        self.reads.clear();
        self.step = Step::Idle;
    }
}

pub(crate) const PREFERRED: [&str; 5] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT", "STRING"];

/// The Wayland mime types one X target atom is offered as. STRING (Latin-1) is converted to UTF-8 on the way.
fn mimes_for(a: &Atoms, atom: u32) -> Vec<&'static str> {
    if atom == a.UTF8_STRING {
        vec!["text/plain;charset=utf-8", "UTF8_STRING", "TEXT"]
    } else if atom == xp::ATOM_STRING {
        vec!["text/plain;charset=utf-8", "text/plain"]
    } else if atom == a.MIME_TEXT_UTF8 {
        vec!["text/plain;charset=utf-8"]
    } else if atom == a.MIME_TEXT {
        vec!["text/plain"]
    } else {
        Vec::new()
    }
}

/// The owner's TARGETS as (mime, atom) pairs, and the atoms whose names must be asked (perhaps mime types).
/// A text type the owner also has natively replaces the converted STRING (UTF8_STRING is preferred).
fn map_targets(a: &Atoms, list: &[u32]) -> (Vec<(String, u32)>, Vec<u32>) {
    let mut targets: Vec<(String, u32)> = Vec::new();
    let mut unnamed = Vec::new();
    let skip = [a.TARGETS, a.TIMESTAMP, a.INCR, a.TEXT];
    for &atom in list {
        let known = mimes_for(a, atom);
        if known.is_empty() {
            if !skip.contains(&atom) && !unnamed.contains(&atom) {
                unnamed.push(atom);
            }
            continue;
        }
        for m in known {
            match targets.iter_mut().find(|(n, _)| n == m) {
                Some(t) if t.1 == xp::ATOM_STRING && atom != xp::ATOM_STRING => t.1 = atom,
                Some(_) => {}
                None => targets.push((m.to_string(), atom)),
            }
        }
    }
    (targets, unnamed)
}

/// ICCCM 2.6.2: STRING is ISO 8859-1.
fn latin1_to_utf8(b: &[u8]) -> Vec<u8> {
    b.iter().map(|&c| c as char).collect::<String>().into_bytes()
}

pub fn event(wm: &mut Xwm, app: &mut App, e: &[u8]) {
    let code = e[0] & 0x7f;
    let a = wm.atoms.clone();
    if wm.xfixes_event != 0 && code == wm.xfixes_event {
        let (owner, selection, time) = (u32_at(e, 8), u32_at(e, 12), u32_at(e, 16));
        if selection != a.CLIPBOARD || owner == wm.wm_win {
            return;
        }
        // Another owner (or none): replies, names and transfers of the old one count no more.
        wm.sel.generation = wm.sel.generation.wrapping_add(1);
        wm.sel.naming = 0;
        wm.sel.time = time;
        if wm.sel.reading.is_some() || !wm.sel.reads.is_empty() {
            eprintln!("bitscomp: xsel: the X clipboard changed during a paste: that paste ends empty");
        }
        wm.sel.drop_reads();
        if owner == 0 {
            if wm.sel.offered {
                clear_data_device_selection(&app.dh, &app.seat);
                wm.sel.offered = false;
            }
            wm.sel.targets.clear();
            return;
        }
        wm.sel.step = Step::Targets(wm.sel.generation);
        wm.sel.owned = false;
        wm.send(xp::convert_selection(wm.wm_win, a.CLIPBOARD, a.TARGETS, a._BITSCOMP_SELECTION, time), None);
        return;
    }
    match code {
        xp::SELECTION_NOTIFY => {
            let (time, requestor, selection, target, property) = (u32_at(e, 4), u32_at(e, 8), u32_at(e, 12), u32_at(e, 16), u32_at(e, 20));
            if requestor != wm.wm_win || selection != a.CLIPBOARD {
                return;
            }
            // Only the answer to the conversion asked now: a late one of a given-up conversion or an older owner
            // names another target or time (owners echo the request's time; 0 is taken as "not echoed").
            let want = match wm.sel.step {
                Step::Targets(g) if target == a.TARGETS => Want::Targets(g),
                Step::Data(t) if target == t => match wm.sel.reading.as_ref() {
                    Some(r) => Want::Data(r.id),
                    None => return,
                },
                _ => return,
            };
            if time != 0 && time != wm.sel.time {
                return;
            }
            if property == 0 {
                eprintln!("bitscomp: xsel: the X owner refused the conversion ({want:?})");
                failed(wm, app, want);
                return;
            }
            wm.send(xp::get_property(true, wm.wm_win, property, READ_UNITS), Some(XWant::Sel(want)));
        }
        xp::SELECTION_REQUEST => crate::xsel_serve::request(wm, app, e),
        xp::SELECTION_CLEAR if u32_at(e, 12) == a.CLIPBOARD => wm.sel.owned = false,
        _ => {}
    }
}

fn current(wm: &Xwm, id: u64) -> bool {
    wm.sel.reading.as_ref().is_some_and(|r| r.id == id)
}

pub fn failed(wm: &mut Xwm, _app: &mut App, want: Want) {
    match want {
        Want::Data(id) | Want::Incr(id) if current(wm, id) => finish(wm, false),
        Want::Data(_) | Want::Incr(_) => {}
        Want::Targets(g) if g == wm.sel.generation => {
            wm.sel.step = Step::Idle;
            next(wm);
        }
        Want::Targets(_) => {}
        Want::Name(g, _) => {
            if g == wm.sel.generation {
                wm.sel.naming = wm.sel.naming.saturating_sub(1);
            }
        }
    }
}

pub fn reply(wm: &mut Xwm, app: &mut App, want: Want, b: &[u8]) {
    match want {
        Want::Targets(g) if g == wm.sel.generation => {
            let p = xp::parse_property(b);
            wm.sel.naming = 0;
            wm.sel.step = Step::Idle;
            let (targets, unnamed) = map_targets(&wm.atoms, &p.u32s());
            wm.sel.targets = targets;
            for atom in unnamed {
                wm.sel.naming += 1;
                wm.send(xp::get_atom_name(atom), Some(XWant::Sel(Want::Name(g, atom))));
            }
            if wm.sel.naming == 0 {
                publish(wm, app);
            }
            next(wm);
        }
        Want::Name(g, atom) if g == wm.sel.generation => {
            let n = xp::u16_at(b, 8) as usize;
            let name = String::from_utf8_lossy(b.get(32..32 + n).unwrap_or(&[])).into_owned();
            // A mime type (it has a slash) is offered under its own name; X's own atoms are not.
            if name.contains('/') && !wm.sel.targets.iter().any(|(m, _)| *m == name) {
                wm.sel.targets.push((name, atom));
            }
            wm.sel.naming = wm.sel.naming.saturating_sub(1);
            if wm.sel.naming == 0 {
                publish(wm, app);
            }
        }
        Want::Data(id) if current(wm, id) => {
            let p = xp::parse_property(b);
            if p.ty == wm.atoms.INCR {
                // The owner sends pieces now that the property is gone (GetProperty deleted it).
                if let Some(r) = wm.sel.reading.as_mut() {
                    r.incr = true;
                    r.last = Instant::now();
                }
                return;
            }
            let ok = grow(wm, &p.value);
            finish(wm, ok);
        }
        Want::Incr(id) if current(wm, id) => {
            let p = xp::parse_property(b);
            if p.value.is_empty() {
                finish(wm, true);
            } else if !grow(wm, &p.value) {
                finish(wm, false);
            }
        }
        _ => {}
    }
}

/// Adds a piece to the X-to-Wayland transfer; false when it would pass MAX_TRANSFER.
fn grow(wm: &mut Xwm, piece: &[u8]) -> bool {
    let Some(r) = wm.sel.reading.as_mut() else { return false };
    if r.data.len().saturating_add(piece.len()) > MAX_TRANSFER {
        eprintln!("bitscomp: xsel: the X clipboard is larger than {} MiB: the paste is refused", MAX_TRANSFER >> 20);
        return false;
    }
    r.data.extend_from_slice(piece);
    r.last = Instant::now();
    true
}

fn publish(wm: &mut Xwm, app: &mut App) {
    if wm.sel.targets.is_empty() {
        return;
    }
    let mimes: Vec<String> = wm.sel.targets.iter().map(|(m, _)| m.clone()).collect();
    eprintln!("bitscomp: xsel: an X11 program owns the clipboard; offered to Wayland as {}", mimes.join(" "));
    set_data_device_selection(&app.dh, &app.seat, mimes, ());
    wm.sel.offered = true;
}

pub fn property_notify(wm: &mut Xwm, _app: &mut App, win: u32, atom: u32, deleted: bool) {
    if win == wm.wm_win {
        if atom == wm.atoms._BITSCOMP_SELECTION
            && !deleted
            && let Some(r) = wm.sel.reading.as_ref().filter(|r| r.incr)
        {
            let id = r.id;
            wm.send(xp::get_property(true, wm.wm_win, atom, READ_UNITS), Some(XWant::Sel(Want::Incr(id))));
        }
        return;
    }
    crate::xsel_serve::took_piece(wm, win, atom, deleted);
}

/// A Wayland client wants the X owner's data as `mime`: queued, converted one after the other.
pub fn send_selection(wm: &mut Xwm, mime: &str, fd: OwnedFd) {
    let Some(&(_, atom)) = wm.sel.targets.iter().find(|(m, _)| m == mime).filter(|_| wm.sel.offered) else { return };
    wm.sel.reads.push_back((atom, fd));
    next(wm);
}

fn next(wm: &mut Xwm) {
    if wm.sel.step != Step::Idle {
        return;
    }
    let Some((atom, fd)) = wm.sel.reads.pop_front() else { return };
    let id = wm.sel.next_id;
    wm.sel.next_id += 1;
    wm.sel.reading = Some(Reading { id, fd, data: Vec::new(), incr: false, target: atom, last: Instant::now() });
    wm.sel.step = Step::Data(atom);
    let a = wm.atoms.clone();
    let time = wm.sel.time;
    wm.send(xp::convert_selection(wm.wm_win, a.CLIPBOARD, atom, a._BITSCOMP_SELECTION, time), None);
    // The owner may never answer, or stop in the middle of INCR: the paste ends then (its reader gets EOF).
    if let Some(h) = wm.sel.handle.clone() {
        let r = h.insert_source(Timer::from_duration(TRANSFER_IDLE), move |_, _, app: &mut App| {
            let Some(wm) = app.xwayland.as_mut().and_then(|x| x.wm.as_mut()) else { return TimeoutAction::Drop };
            let Some(idle) = wm.sel.reading.as_ref().filter(|r| r.id == id).map(|r| r.last.elapsed()) else {
                return TimeoutAction::Drop;
            };
            if idle < TRANSFER_IDLE {
                return TimeoutAction::ToDuration(TRANSFER_IDLE - idle);
            }
            eprintln!("bitscomp: xsel: the X clipboard owner did not answer for {} s: the paste ends empty", TRANSFER_IDLE.as_secs());
            finish(wm, false);
            wm.conn.flush();
            TimeoutAction::Drop
        });
        if let Err(e) = r {
            eprintln!("bitscomp: xsel: no deadline for a paste ({e})");
        }
    }
}

/// The X-to-Wayland transfer ends: its data into the reader's pipe (`ok`), or nothing (the fd closes: EOF).
fn finish(wm: &mut Xwm, ok: bool) {
    if let Some(r) = wm.sel.reading.take()
        && ok
    {
        let data = if r.target == xp::ATOM_STRING { latin1_to_utf8(&r.data) } else { r.data };
        write_out(wm.sel.handle.as_ref(), r.fd, data);
    }
    wm.sel.step = Step::Idle;
    next(wm);
}

/// The data into a Wayland client's pipe as fast as it reads; the fd closes at the end (its EOF). A reader that
/// takes nothing for TRANSFER_IDLE loses the rest.
fn write_out(handle: Option<&LoopHandle<'static, App>>, fd: OwnedFd, data: Vec<u8>) {
    let Some(h) = handle else { return };
    nonblocking(&fd);
    let mut at = 0;
    let mut file = std::fs::File::from(fd);
    let Ok(watch) = file.try_clone() else { return };
    let last = Rc::new(Cell::new(Some(Instant::now())));
    let seen = last.clone();
    let token = h.insert_source(Generic::new(watch, Interest::WRITE, Mode::Level), move |_, _, _| {
        while at < data.len() {
            match file.write(&data[at..]) {
                Ok(n) => {
                    at += n;
                    seen.set(Some(Instant::now()));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PostAction::Continue),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        seen.set(None);
        Ok(PostAction::Remove)
    });
    match token {
        Ok(t) => idle_deadline(h, last, move |h| {
            eprintln!("bitscomp: xsel: a Wayland program stopped reading a paste: given up");
            h.remove(t);
        }),
        Err(e) => eprintln!("bitscomp: xsel: cannot watch a clipboard pipe: {e}"),
    }
}

/// Calls `expire` once `last` (None: done) has stood still for TRANSFER_IDLE.
pub(crate) fn idle_deadline(h: &LoopHandle<'static, App>, last: Rc<Cell<Option<Instant>>>, expire: impl FnOnce(&LoopHandle<'static, App>) + 'static) {
    let mut expire = Some(expire);
    let me = h.clone();
    let r = h.insert_source(Timer::from_duration(TRANSFER_IDLE), move |_, _, _| {
        let Some(at) = last.get() else { return TimeoutAction::Drop };
        let idle = at.elapsed();
        if idle < TRANSFER_IDLE {
            return TimeoutAction::ToDuration(TRANSFER_IDLE - idle);
        }
        if let Some(f) = expire.take() {
            f(&me);
        }
        TimeoutAction::Drop
    });
    if let Err(e) = r {
        eprintln!("bitscomp: xsel: no deadline for a clipboard transfer ({e})");
    }
}

pub(crate) fn nonblocking(fd: &impl AsRawFd) {
    // SAFETY: fcntl on an fd this function's caller owns; only the file status flags change.
    unsafe {
        let fl = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

/// Xwayland ended: an offer of its data is withdrawn.
pub fn forget(wm: &mut Xwm, app: &mut App) {
    if wm.sel.offered {
        clear_data_device_selection(&app.dh, &app.seat);
    }
    wm.sel = Sel { handle: wm.sel.handle.take(), ..Default::default() };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> Atoms {
        Atoms { UTF8_STRING: 300, TARGETS: 301, TIMESTAMP: 302, INCR: 303, TEXT: 304, MIME_TEXT_UTF8: 305, MIME_TEXT: 306, ..Default::default() }
    }

    #[test]
    fn utf8_string_wins_over_string_and_unknown_atoms_are_named() {
        let a = atoms();
        let (t, unnamed) = map_targets(&a, &[a.TARGETS, xp::ATOM_STRING, 900, a.UTF8_STRING, a.TIMESTAMP, 900, a.MIME_TEXT]);
        let of = |m: &str| t.iter().find(|(n, _)| n == m).map(|x| x.1);
        assert_eq!(of("text/plain;charset=utf-8"), Some(a.UTF8_STRING));
        assert_eq!(of("text/plain"), Some(a.MIME_TEXT));
        assert_eq!(of("UTF8_STRING"), Some(a.UTF8_STRING));
        assert_eq!(of("STRING"), None);
        assert_eq!(unnamed, vec![900]);
        // STRING alone is still offered (converted on the way).
        let (t, _) = map_targets(&a, &[xp::ATOM_STRING]);
        assert_eq!(t.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["text/plain;charset=utf-8", "text/plain"]);
    }

    #[test]
    fn string_is_read_as_latin1() {
        assert_eq!(latin1_to_utf8(b"Gr\xfc\xdfe"), "Grüße".as_bytes());
    }

}
