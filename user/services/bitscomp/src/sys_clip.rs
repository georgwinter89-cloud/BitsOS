//! SYS-A: bitscomp itself as the clipboard's owner, for "Bericht für Claude kopieren": the selection is set server
//! side with the text kinds, a program that pastes gets the text written into its pipe on a thread of its own (B4:
//! a reader that never reads cannot hold the loop). Any other owner (a client's copy, an X11 program through the
//! bridge) replaces it, and bitscomp forgets its text then.

use std::io::Write;
use std::os::fd::OwnedFd;

use smithay::wayland::selection::data_device::set_data_device_selection;

use crate::state::App;

pub const MIMES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "TEXT"];

impl App {
    pub(crate) fn sys_clip_set(&mut self, text: String) {
        eprintln!("bitscomp: system: the report ({} bytes) is the clipboard now", text.len());
        self.sys.clip = Some(text);
        set_data_device_selection(&self.dh, &self.seat, MIMES.iter().map(|m| m.to_string()).collect(), ());
    }

    /// Another owner took the clipboard.
    pub(crate) fn sys_clip_lost(&mut self) {
        self.sys.clip = None;
    }
}

/// Serves a paste of bitscomp's own text; gives the pipe back when bitscomp does not own the clipboard.
pub fn serve(app: &App, mime: &str, fd: OwnedFd) -> Option<OwnedFd> {
    let Some(text) = app.sys.clip.clone().filter(|_| MIMES.contains(&mime)) else { return Some(fd) };
    let spawned = std::thread::Builder::new().name("bitscomp-clip".into()).spawn(move || {
        let mut f = std::fs::File::from(fd);
        let _ = f.write_all(text.as_bytes());
    });
    if let Err(e) = spawned {
        eprintln!("bitscomp: system: no thread to hand the report over: {e}");
    }
    None
}
