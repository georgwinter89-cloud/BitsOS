//! GPY-PASTE: the clipboard's text for bitscomp's own fields (the assistant's input line, the pages' fields).
//! Its owner is asked in this order: a Wayland program (its data source writes into a pipe), bitscomp itself (the
//! report it copied, `sys_clip`: no pipe needed), an X11 program (bitscomp's bridge, `xsel`, writes the pipe).
//! The pipe is read on a thread of its own with a deadline and a size cap (B4): a program that never writes or
//! writes without end cannot hold the compositor's loop. What is read is no business of the log: lines here say
//! why a paste failed, never what it held.

use std::io::{ErrorKind, PipeReader, Read};
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::{
    SelectionRequestError, current_data_device_selection_userdata, request_data_device_client_selection,
};

use crate::state::App;

/// Without a new byte for this long the owner is taken as stuck.
pub const IDLE: Duration = Duration::from_secs(2);
/// A paste is over after this long however the owner trickles.
pub const TOTAL: Duration = Duration::from_secs(5);
/// A single-line field reads no more than this.
pub const LINE_CAP: usize = 64 << 10;

/// Where the clipboard's text comes from.
pub enum Source {
    /// bitscomp's own copy (`sys_clip`).
    Own(String),
    /// A pipe its owner (a Wayland program, or the X11 bridge for an X program) writes into.
    Pipe(PipeReader),
    /// Nobody owns the clipboard, or not as text.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// No text on the clipboard (no owner, no text kind, nothing written, no UTF-8).
    Empty,
    /// Its owner stopped giving the text (`IDLE`, `TOTAL`).
    Slow,
    /// More than the field takes.
    TooLong,
    Failed,
}

/// How much is read, and what of it is kept.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub cap: usize,
    pub idle: Duration,
    pub total: Duration,
    /// A single-line field: only the first line that is not blank, without the spaces around it.
    pub line: bool,
}

impl Limits {
    pub const LINE: Limits = Limits { cap: LINE_CAP, idle: IDLE, total: TOTAL, line: true };
}

/// Ctrl+V or Shift+Insert (no other modifier with them).
pub fn is_paste(sym: Keysym, mods: &ModifiersState) -> bool {
    match sym {
        Keysym::v | Keysym::V => mods.ctrl && !mods.alt && !mods.logo,
        Keysym::Insert | Keysym::KP_Insert => mods.shift && !mods.ctrl && !mods.alt && !mods.logo,
        _ => false,
    }
}

/// A log line of a paste (in a host test also kept, to prove no text is ever in one).
pub fn log(line: &str) {
    eprintln!("bitscomp: paste: {line}");
    #[cfg(test)]
    LOG.with(|l| l.borrow_mut().push(line.to_string()));
}

#[cfg(test)]
thread_local! {
    pub static LOG: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl App {
    /// The clipboard's owner is asked for its text.
    pub(crate) fn clip_source(&mut self) -> Source {
        for mime in crate::xsel::PREFERRED {
            let Ok((r, w)) = std::io::pipe() else { return Source::None };
            match request_data_device_client_selection(&self.seat, mime.to_string(), OwnedFd::from(w)) {
                Ok(()) => return Source::Pipe(r),
                Err(SelectionRequestError::ServerSideSelection) => {
                    // bitscomp's own copy (its selection has these kinds), else an X11 program's through the bridge.
                    if current_data_device_selection_userdata(&self.seat).is_some()
                        && let Some(t) = self.sys.clip.clone()
                    {
                        return Source::Own(t);
                    }
                    let Ok((r, w)) = std::io::pipe() else { return Source::None };
                    crate::xwayland::send_selection(
                        self,
                        SelectionTarget::Clipboard,
                        mime.to_string(),
                        OwnedFd::from(w),
                    );
                    return Source::Pipe(r);
                }
                Err(SelectionRequestError::NoSelection) => return Source::None,
                Err(SelectionRequestError::InvalidMimetype) => continue,
            }
        }
        Source::None
    }
}

/// The text from the pipe's reading end, within `lim` (`read` on a thread of its own: it waits).
pub fn read(r: PipeReader, lim: Limits) -> Result<String, Why> {
    crate::xsel::nonblocking(&r);
    let mut r = r;
    let (mut data, mut buf) = (Vec::new(), [0u8; 16384]);
    let (start, mut last, mut cut) = (Instant::now(), Instant::now(), false);
    let out = loop {
        if lim.line && line_done(&data) {
            break Ok(());
        }
        match r.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                let room = lim.cap.saturating_sub(data.len());
                data.extend_from_slice(&buf[..n.min(room)]);
                buf[..n].fill(0);
                last = Instant::now();
                if n > room {
                    cut = true;
                    break Ok(());
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                if last.elapsed() > lim.idle || start.elapsed() > lim.total {
                    break Err(Why::Slow);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break Err(Why::Failed),
        }
    };
    let text = out.and_then(|()| keep(&data, lim.line, cut));
    data.fill(0);
    std::hint::black_box(&data);
    text
}

/// The first line that is not blank has ended.
fn line_done(data: &[u8]) -> bool {
    let mut seen = false;
    for &b in data {
        match b {
            b'\n' | b'\r' if seen => return true,
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => seen = true,
        }
    }
    false
}

/// What of the bytes read is the paste: the text (a cut in a character drops its piece), or its first line.
fn keep(data: &[u8], line: bool, cut: bool) -> Result<String, Why> {
    let valid = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&data[..e.valid_up_to()]).unwrap_or_default(),
        Err(_) => return Err(Why::Empty),
    };
    if !line {
        if cut {
            log(&format!("the clipboard holds more than {} KiB: only its start is pasted", data.len() >> 10));
        }
        return if valid.is_empty() { Err(Why::Empty) } else { Ok(valid.to_string()) };
    }
    // A line cut by the cap is not the whole line (a token would be stored broken).
    if cut && !line_done(data) {
        return Err(Why::TooLong);
    }
    first_line(valid).ok_or(Why::Empty)
}

/// The first line that is not blank, without the spaces around it; its tabs become spaces, other control
/// characters go.
pub fn first_line(text: &str) -> Option<String> {
    let l = text.split(['\n', '\r']).map(str::trim).find(|l| !l.is_empty())?;
    Some(l.chars().filter_map(|c| if c == '\t' { Some(' ') } else { (!c.is_control()).then_some(c) }).collect())
}

#[cfg(test)]
#[path = "clip_read_tests.rs"]
mod tests;
