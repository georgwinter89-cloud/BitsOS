//! WY-3: the Wayland clipboard served to X clients (the half of xsel.rs going towards X11): bitscomp's WM window
//! owns CLIPBOARD for a Wayland client's selection, answers TARGETS/TIMESTAMP itself and fetches text from the
//! Wayland client when an X client converts it, in INCR pieces when it is large. STRING answers are Latin-1.

use std::cell::Cell;
use std::io::{ErrorKind, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::time::Instant;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::request_data_device_client_selection;

use crate::state::App;
use crate::xproto::{self as xp, u32_at};
use crate::xsel::{MAX_TRANSFER, PREFERRED, TRANSFER_IDLE, idle_deadline, nonblocking};
use crate::xwm::Xwm;

/// One property write of an INCR transfer (and what an X client may take without BIG-REQUESTS, with room).
const CHUNK: usize = 64 * 1024;

/// An answer to an X client that goes in pieces (INCR).
pub struct Outgoing {
    pub(crate) requestor: u32,
    property: u32,
    target: u32,
    data: Vec<u8>,
    at: usize,
    last: Instant,
}

impl Outgoing {
    /// The next piece; an empty one ends the transfer (ICCCM 2.7.2).
    fn piece(&mut self) -> Vec<u8> {
        let end = (self.at + CHUNK).min(self.data.len());
        let p = self.data[self.at..end].to_vec();
        self.at = end;
        self.last = Instant::now();
        p
    }
}

/// UTF-8 text as STRING; what Latin-1 cannot hold becomes '?' (invalid UTF-8 too).
fn utf8_to_latin1(b: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(b).chars().map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' }).collect()
}

/// An X client took a piece of an INCR answer (it deleted the property): the next one.
pub fn took_piece(wm: &mut Xwm, win: u32, atom: u32, deleted: bool) {
    let Some(i) = wm.sel.outgoing.iter().position(|o| o.requestor == win && o.property == atom) else { return };
    if !deleted {
        return;
    }
    let o = &mut wm.sel.outgoing[i];
    let piece = o.piece();
    let (r, p, t) = (o.requestor, o.property, o.target);
    if piece.is_empty() {
        wm.sel.outgoing.remove(i);
    }
    wm.send(xp::change_property(r, p, t, 8, &piece), None);
}

/// A window went: INCR answers to it end (nobody will delete their property any more).
pub fn requestor_gone(wm: &mut Xwm, win: u32) {
    wm.sel.outgoing.retain(|o| o.requestor != win);
}

/// Once per WM turn: INCR answers whose requestor stopped taking pieces end.
pub fn turn(wm: &mut Xwm) {
    let before = wm.sel.outgoing.len();
    wm.sel.outgoing.retain(|o| o.last.elapsed() < TRANSFER_IDLE);
    if wm.sel.outgoing.len() != before {
        eprintln!("bitscomp: xsel: an X11 program stopped taking the clipboard for {} s: given up", TRANSFER_IDLE.as_secs());
    }
}

/// A Wayland client set (or cleared) the clipboard: bitscomp's WM window owns it for X clients.
pub fn wl_selection(wm: &mut Xwm, ty: SelectionTarget, mimes: Option<Vec<String>>) {
    if ty != SelectionTarget::Clipboard || !wm.ready {
        return;
    }
    let a = wm.atoms.clone();
    match mimes {
        Some(m) => {
            wm.sel.offered = false;
            wm.sel.owned = true;
            wm.sel.wl_mimes = m;
            wm.send(xp::set_selection_owner(wm.wm_win, a.CLIPBOARD, 0), None);
        }
        None if wm.sel.owned => {
            wm.sel.owned = false;
            wm.send(xp::set_selection_owner(0, a.CLIPBOARD, 0), None);
        }
        None => {}
    }
}

fn notify(wm: &mut Xwm, requestor: u32, selection: u32, target: u32, property: u32, time: u32) {
    let ev = xp::event(xp::SELECTION_NOTIFY, &[(4, time), (8, requestor), (12, selection), (16, target), (20, property)], 0);
    wm.send(xp::send_event(requestor, 0, &ev), None);
}

/// An X client asks bitscomp's WM window for the Wayland selection.
pub fn request(wm: &mut Xwm, app: &mut App, e: &[u8]) {
    let (time, requestor, selection, target) = (u32_at(e, 4), u32_at(e, 12), u32_at(e, 16), u32_at(e, 20));
    // ICCCM 2.2: an obsolete client names no property; the target is used then.
    let property = match u32_at(e, 24) {
        0 => target,
        p => p,
    };
    let a = wm.atoms.clone();
    let text = PREFERRED.iter().find(|m| wm.sel.wl_mimes.iter().any(|w| w == *m)).map(|m| m.to_string());
    if selection != a.CLIPBOARD || !wm.sel.owned {
        return notify(wm, requestor, selection, target, 0, time);
    }
    if target == a.TARGETS {
        let mut list = vec![a.TARGETS, a.TIMESTAMP];
        if text.is_some() {
            list.extend([a.UTF8_STRING, a.TEXT, xp::ATOM_STRING, a.MIME_TEXT_UTF8, a.MIME_TEXT]);
        }
        wm.send(xp::change_property32(requestor, property, xp::ATOM_ATOM, &list), None);
        return notify(wm, requestor, selection, target, property, time);
    }
    if target == a.TIMESTAMP {
        wm.send(xp::change_property32(requestor, property, xp::ATOM_CARDINAL, &[time]), None);
        return notify(wm, requestor, selection, target, property, time);
    }
    let texts = [a.UTF8_STRING, a.TEXT, xp::ATOM_STRING, a.MIME_TEXT_UTF8, a.MIME_TEXT];
    let (Some(mime), true) = (text, texts.contains(&target)) else {
        return notify(wm, requestor, selection, target, 0, time);
    };
    let Some((read, write)) = pipe() else { return notify(wm, requestor, selection, target, 0, time) };
    if let Err(e) = request_data_device_client_selection(&app.seat, mime.clone(), write) {
        eprintln!("bitscomp: xsel: the Wayland selection cannot give {mime}: {e}");
        return notify(wm, requestor, selection, target, 0, time);
    }
    // TEXT is answered as UTF8_STRING (ICCCM lets the owner choose the type).
    let ty = if target == a.TEXT { a.UTF8_STRING } else { target };
    let Some(h) = wm.sel.handle.clone() else { return };
    nonblocking(&read);
    let mut file = std::fs::File::from(read);
    let Ok(watch) = file.try_clone() else { return notify(wm, requestor, selection, target, 0, time) };
    let mut data = Vec::new();
    let last = Rc::new(Cell::new(Some(Instant::now())));
    let seen = last.clone();
    let r = h.insert_source(Generic::new(watch, Interest::READ, Mode::Level), move |_, _, app: &mut App| {
        let mut buf = [0u8; 16384];
        let ok = loop {
            match file.read(&mut buf) {
                Ok(0) => break true,
                Ok(n) if data.len() + n > MAX_TRANSFER => {
                    eprintln!("bitscomp: xsel: the Wayland clipboard is larger than {} MiB: refused to the X11 program", MAX_TRANSFER >> 20);
                    break false;
                }
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    seen.set(Some(Instant::now()));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PostAction::Continue),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    eprintln!("bitscomp: xsel: reading the Wayland clipboard failed: {e}");
                    break false;
                }
            }
        };
        seen.set(None);
        if let Some(wm) = app.xwayland.as_mut().and_then(|x| x.wm.as_mut()) {
            if ok {
                let data = std::mem::take(&mut data);
                let data = if ty == xp::ATOM_STRING { utf8_to_latin1(&data) } else { data };
                answer(wm, requestor, selection, target, ty, property, time, data);
            } else {
                notify(wm, requestor, selection, target, 0, time);
            }
            wm.conn.flush();
        }
        Ok(PostAction::Remove)
    });
    match r {
        Ok(t) => idle_deadline(&h, last, move |h| {
            eprintln!("bitscomp: xsel: a Wayland program gave no clipboard data for {} s: refused to the X11 program", TRANSFER_IDLE.as_secs());
            h.remove(t);
            // The X requestor learns it at once instead of waiting for ever (the reply is a timer's, not a WM call).
            let h = h.clone();
            h.insert_idle(move |app| {
                if let Some(wm) = app.xwayland.as_mut().and_then(|x| x.wm.as_mut()) {
                    notify(wm, requestor, selection, target, 0, time);
                    wm.conn.flush();
                }
            });
        }),
        Err(_) => notify(wm, requestor, selection, target, 0, time),
    }
}

#[allow(clippy::too_many_arguments)]
fn answer(wm: &mut Xwm, requestor: u32, selection: u32, target: u32, ty: u32, property: u32, time: u32, data: Vec<u8>) {
    if data.len() <= CHUNK {
        wm.send(xp::change_property(requestor, property, ty, 8, &data), None);
    } else {
        // INCR (ICCCM 2.7.2): the size first, then a piece each time the client deletes the property. A top level
        // keeps the WM's mask (its end comes through the root); another window tells its own end.
        let mask = if wm.knows(requestor) { xp::PROPERTY_CHANGE | xp::FOCUS_CHANGE } else { xp::PROPERTY_CHANGE | xp::STRUCTURE_NOTIFY };
        wm.send(xp::change_window_attributes(requestor, xp::CW_EVENT_MASK, &[mask]), None);
        let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
        wm.send(xp::change_property32(requestor, property, wm.atoms.INCR, &[size]), None);
        wm.sel.outgoing.retain(|o| !(o.requestor == requestor && o.property == property));
        wm.sel.outgoing.push(Outgoing { requestor, property, target: ty, data, at: 0, last: Instant::now() });
    }
    notify(wm, requestor, selection, target, property, time);
}

fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: pipe2 writes two fds into the array on success; each is owned exactly once below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    // SAFETY: both fds were just created by pipe2 and belong to nobody else.
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_answers_are_latin1() {
        assert_eq!(utf8_to_latin1("Grüße €".as_bytes()), b"Gr\xfc\xdfe ?");
        assert_eq!(utf8_to_latin1(b"a\xffb"), b"a?b");
    }

    #[test]
    fn an_incr_answer_goes_in_chunks_and_ends_with_an_empty_piece() {
        let data: Vec<u8> = (0..(2 * CHUNK + 17)).map(|i| i as u8).collect();
        let mut o = Outgoing { requestor: 1, property: 2, target: 3, data: data.clone(), at: 0, last: Instant::now() };
        let mut got = Vec::new();
        let mut pieces = 0;
        loop {
            let p = o.piece();
            if p.is_empty() {
                break;
            }
            assert!(p.len() <= CHUNK);
            got.extend_from_slice(&p);
            pieces += 1;
        }
        assert_eq!((pieces, got), (3, data));
        assert!(o.piece().is_empty());
    }
}
