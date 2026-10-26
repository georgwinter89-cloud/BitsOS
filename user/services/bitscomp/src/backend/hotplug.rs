//! Input hot plug: an inotify watch on `/dev/input` (IN_CREATE, IN_ATTRIB, IN_DELETE, IN_MOVED_TO) wakes the
//! scan when a node comes, changes its mode or goes; V2 has inotify since FS-14. A node that failed to open is
//! tried again by a one-shot timer while tries are left (no event may follow an EBUSY). Without inotify (the
//! call fails, `/dev/input` is missing) the scan runs once a second instead.

use std::cell::Cell;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::rc::Rc;
use std::time::Duration;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use crate::state::App;

/// The fallback scan's period, and the retry delay of a node that failed to open.
pub const RESCAN: Duration = Duration::from_secs(1);
const MASK: u32 = libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE | libc::IN_MOVED_TO | libc::IN_ONLYDIR;

/// Starts watching `dir`: `scan` runs on every change and returns whether a node waits for another try.
pub fn watch(handle: &LoopHandle<'static, App>, dir: &str, scan: Rc<dyn Fn() -> bool>) -> Result<(), String> {
    match inotify(dir) {
        Ok(file) => {
            let retry = Rc::new(Cell::new(false));
            let (h, s, r) = (handle.clone(), scan.clone(), retry.clone());
            let fd = file.as_raw_fd();
            handle
                .insert_source(Generic::new(file, Interest::READ, Mode::Level), move |_, _, _| {
                    let gone = drain(fd);
                    if s() {
                        retry_later(&h, &s, &r);
                    }
                    if gone {
                        eprintln!("bitscomp: input: the watch on /dev/input ended; looking once a second instead");
                        fallback(&h, s.clone());
                        return Ok(PostAction::Remove);
                    }
                    Ok(PostAction::Continue)
                })
                .map_err(|e| format!("input watch: {e}"))?;
            // Once more now the watch is in place: a node made after the caller's first scan is not missed.
            if scan() {
                retry_later(handle, &scan, &retry);
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("bitscomp: input: no inotify on {dir} ({e}); looking once a second instead");
            scan();
            fallback(handle, scan);
            Ok(())
        }
    }
}

fn inotify(dir: &str) -> std::io::Result<File> {
    // SAFETY: plain syscalls; the fd is owned by the File from here on.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just made and is not owned elsewhere.
    let file = unsafe { File::from_raw_fd(fd) };
    let path = CString::new(dir).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `path` is a valid C string for the call's duration.
    if unsafe { libc::inotify_add_watch(fd, path.as_ptr(), MASK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

/// Reads every queued event (their content does not matter: the scan looks at the directory); true when the
/// watch ended (IN_IGNORED: the directory went away).
fn drain(fd: i32) -> bool {
    let mut buf = [0u8; 4096];
    let mut gone = false;
    loop {
        // SAFETY: `buf` is writable for its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            return gone;
        }
        gone |= ignored(&buf[..n as usize]);
    }
}

/// Whether the records in `b` hold an IN_IGNORED event.
pub fn ignored(b: &[u8]) -> bool {
    let mut at = 0;
    while at + 16 <= b.len() {
        let mask = u32::from_ne_bytes([b[at + 4], b[at + 5], b[at + 6], b[at + 7]]);
        let len = u32::from_ne_bytes([b[at + 12], b[at + 13], b[at + 14], b[at + 15]]) as usize;
        if mask & libc::IN_IGNORED != 0 {
            return true;
        }
        at += 16 + len;
    }
    false
}

/// One more scan after `RESCAN` (one timer at a time) for nodes that failed to open.
fn retry_later(handle: &LoopHandle<'static, App>, scan: &Rc<dyn Fn() -> bool>, armed: &Rc<Cell<bool>>) {
    if armed.replace(true) {
        return;
    }
    let (s, a) = (scan.clone(), armed.clone());
    let r = handle.insert_source(Timer::from_duration(RESCAN), move |_, _, _| {
        if s() {
            return TimeoutAction::ToDuration(RESCAN);
        }
        a.set(false);
        TimeoutAction::Drop
    });
    if r.is_err() {
        armed.set(false);
    }
}

fn fallback(handle: &LoopHandle<'static, App>, scan: Rc<dyn Fn() -> bool>) {
    let r = handle.insert_source(Timer::from_duration(RESCAN), move |_, _, _| {
        scan();
        TimeoutAction::ToDuration(RESCAN)
    });
    if let Err(e) = r {
        eprintln!("bitscomp: input: no rescan timer either: {e}");
    }
}

#[cfg(test)]
mod tests {
    fn rec(mask: u32, name: &[u8]) -> Vec<u8> {
        let len = if name.is_empty() { 0 } else { (name.len() + 1).div_ceil(16) * 16 };
        let mut v = Vec::new();
        for x in [1i32 as u32, mask, 0, len as u32] {
            v.extend_from_slice(&x.to_ne_bytes());
        }
        v.extend_from_slice(name);
        v.resize(16 + len, 0);
        v
    }
    #[test]
    fn ignored_found_among_records() {
        let mut b = rec(libc::IN_CREATE, b"event3");
        assert!(!super::ignored(&b));
        b.extend(rec(libc::IN_IGNORED, b""));
        assert!(super::ignored(&b));
        assert!(!super::ignored(&b[..10]), "a cut record is not read");
    }
}
