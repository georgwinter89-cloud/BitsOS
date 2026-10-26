//! Input on V2 without udev or libinput (WY-1a decision, notes wy-WY-1a.md): bitscomp opens `/dev/input/event*`
//! itself, probes each with EVIOCGBIT/EVIOCGABS (linux-6.6/drivers/input/evdev.c `evdev_do_ioctl`), grabs it
//! (EVIOCGRAB: the VT gets nothing while bitscomp runs), reads whole `struct input_event`s and turns them into
//! `InputEvent`s through `evdev_map`. Hot plug: a node that goes away answers ENODEV (its held keys are released,
//! the source removed); new nodes are found by an inotify watch on `/dev/input` (`hotplug`; once a second where
//! inotify is missing). Timestamps are asked on CLOCK_MONOTONIC (EVIOCSCLOCKID), bitscomp's one
//! clock, so `stats` can say how old an event was when bitscomp handled it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};

use super::evdev_map::{Caps, Decoder, EV_ABS, EV_KEY, EV_REL, KEY_MAX, Out, Range, SYN_DROPPED, moved, placed};
use crate::input::InputEvent;
use crate::state::App;

const DIR: &str = "/dev/input";

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (0x45 << 8) | nr
}
const fn eviocgbit(ev: u16, len: usize) -> u64 {
    ioc(2, 0x20 + ev as u64, len as u64)
}
const fn eviocgabs(abs: u16) -> u64 {
    ioc(2, 0x40 + abs as u64, 24)
}
const fn eviocgname(len: usize) -> u64 {
    ioc(2, 0x06, len as u64)
}
const EVIOCGRAB: u64 = ioc(1, 0x90, 4);
const EVIOCSCLOCKID: u64 = ioc(1, 0xa0, 4);

/// A node by path and inode: a re-made node is a new device.
pub type Key = (PathBuf, u64);

/// How often a node that fails to open or probe (EACCES, EBUSY, a device still being set up) is tried again.
pub const MAX_TRIES: u32 = 5;
/// Reads per wake-up of one device, 64 events each: a flooding device cannot hold the loop (B4); the level-
/// triggered source fires again next turn for the rest.
pub const MAX_READS: usize = 8;

/// The nodes bitscomp is done with (open, left alone, given up on, or gone by ENODEV while the node still shows)
/// and the failed tries of the others. A node leaves both when it disappears from `/dev/input`.
#[derive(Default)]
pub struct Seen {
    done: HashSet<Key>,
    tries: HashMap<Key, u32>,
}

impl Seen {
    pub fn wants(&self, k: &Key) -> bool {
        !self.done.contains(k)
    }
    pub fn settle(&mut self, k: Key) {
        self.tries.remove(&k);
        self.done.insert(k);
    }
    /// A failed open or probe; true when this was the last try.
    pub fn failed(&mut self, k: Key) -> bool {
        let t = self.tries.entry(k.clone()).or_insert(0);
        *t += 1;
        if *t >= MAX_TRIES {
            self.settle(k);
            return true;
        }
        false
    }
    /// A node failed to open and has tries left.
    pub fn pending(&self) -> bool {
        !self.tries.is_empty()
    }
    pub fn retain(&mut self, now: &HashSet<Key>) {
        self.done.retain(|k| now.contains(k));
        self.tries.retain(|k, _| now.contains(k));
    }
}

type Shared = Rc<RefCell<Seen>>;

/// Opens every keyboard and pointer now and watches `/dev/input` for more.
pub fn start(handle: &LoopHandle<'static, App>) -> Result<(), String> {
    let seen: Shared = Rc::default();
    let opened = scan(handle, &seen);
    eprintln!("bitscomp: input: {opened} devices opened from {DIR}");
    let h = handle.clone();
    super::hotplug::watch(handle, DIR, Rc::new(move || {
        scan(&h, &seen);
        seen.borrow().pending()
    }))
}

fn scan(handle: &LoopHandle<'static, App>, seen: &Shared) -> usize {
    let Ok(rd) = std::fs::read_dir(DIR) else { return 0 };
    let mut now = HashSet::new();
    let mut opened = 0;
    for e in rd.flatten() {
        let path = e.path();
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let key = (path.clone(), meta.ino());
        now.insert(key.clone());
        if !seen.borrow().wants(&key) {
            continue;
        }
        match open(&path) {
            Ok(Some((file, caps, name))) => {
                seen.borrow_mut().settle(key.clone());
                if add(handle, file, caps, &name, key) {
                    opened += 1;
                }
            }
            Ok(None) => seen.borrow_mut().settle(key),
            Err(e) => {
                let last = seen.borrow_mut().failed(key);
                eprintln!("bitscomp: input: {}: {e} ({})", path.display(), if last { "left out" } else { "tried again" });
            }
        }
    }
    // Nodes that went away are forgotten, so a node made again under the same name is opened again.
    seen.borrow_mut().retain(&now);
    opened
}

fn ioctl_buf(fd: i32, req: u64, buf: &mut [u8]) -> std::io::Result<i32> {
    // SAFETY: `buf` is valid for writes of its whole length, the size the request encodes.
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, buf.as_mut_ptr()) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(r) }
}

/// For requests that read an int through the pointer (EVIOCSCLOCKID: `get_user`).
fn ioctl_int(fd: i32, req: u64, v: i32) -> std::io::Result<()> {
    let arg = v;
    // SAFETY: the _IOW(int) requests read one int from the pointer, which lives across the call.
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, &arg as *const i32) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// For requests that take the value itself: EVIOCGRAB grabs on any non-zero argument and releases on 0
/// (linux-6.6/drivers/input/evdev.c:1086), so a pointer would always grab.
fn ioctl_val(fd: i32, req: u64, v: libc::c_ulong) -> std::io::Result<()> {
    // SAFETY: the argument is a plain value; the kernel dereferences nothing.
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, v) };
    if r < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Opens and probes one node: Some when it is a keyboard or pointer bitscomp uses.
fn open(path: &Path) -> std::io::Result<Option<(File, Caps, String)>> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: a plain open of a NUL-terminated path.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nobody else.
    let file = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let mut keys = vec![0u8; KEY_MAX as usize / 8 + 1];
    let mut rel = [0u8; 2];
    let mut abs = [0u8; 8];
    ioctl_buf(fd, eviocgbit(EV_KEY, keys.len()), &mut keys)?;
    // A device without relative or absolute axes answers with zero bytes: the bitmaps stay empty.
    let _ = ioctl_buf(fd, eviocgbit(EV_REL, rel.len()), &mut rel);
    let _ = ioctl_buf(fd, eviocgbit(EV_ABS, abs.len()), &mut abs);
    let range = |axis: u16| -> Option<Range> {
        let mut info = [0u8; 24];
        ioctl_buf(fd, eviocgabs(axis), &mut info).ok()?;
        let at = |i: usize| i32::from_le_bytes([info[i], info[i + 1], info[i + 2], info[i + 3]]);
        Some(Range { min: at(4), max: at(8) })
    };
    let ranges = range(0).zip(range(1));
    let caps = Caps::from_bits(&keys, &rel, &abs, ranges);
    let mut nb = [0u8; 128];
    let n = ioctl_buf(fd, eviocgname(nb.len()), &mut nb).unwrap_or(0).max(0) as usize;
    let name = String::from_utf8_lossy(&nb[..n.min(nb.len())]).trim_end_matches('\0').to_string();
    if !caps.wanted() {
        return Ok(None);
    }
    let _ = ioctl_int(fd, EVIOCSCLOCKID, libc::CLOCK_MONOTONIC);
    Ok(Some((file, caps, name)))
}

fn add(handle: &LoopHandle<'static, App>, file: File, caps: Caps, name: &str, key: Key) -> bool {
    let fd = file.as_raw_fd();
    let grab = match ioctl_val(fd, EVIOCGRAB, 1) {
        Ok(()) => "grabbed".to_string(),
        Err(e) => format!("not grabbed: {e}"),
    };
    let what = match (caps.keyboard, caps.pointer()) {
        (true, true) => "keyboard+pointer",
        (true, false) => "keyboard",
        _ => "pointer",
    };
    eprintln!("bitscomp: input: {} \"{name}\" {what} ({grab})", key.0.display());
    let mut dec = Decoder::new(caps);
    let label = key.0.display().to_string();
    let r = handle.insert_source(Generic::new(file, Interest::READ, Mode::Level), move |_, _, app| {
        let mut out = Vec::new();
        let alive = pump(fd, &mut dec, &mut out, &mut app.stats);
        for o in out {
            deliver(app, o);
        }
        if alive {
            return Ok(PostAction::Continue);
        }
        // The key stays in `Seen` until the node is gone from /dev/input: no re-open of a dying node.
        eprintln!("bitscomp: input: {label} went away (held keys released)");
        Ok(PostAction::Remove)
    });
    r.is_ok()
}

/// Reads what the device has, at most `MAX_READS` times; false when it is gone (ENODEV, end of file, any other
/// error), and then every key and button it held is released in `out`.
pub fn pump(fd: i32, dec: &mut Decoder, out: &mut Vec<Out>, stats: &mut crate::stats::Stats) -> bool {
    let alive = read_some(fd, dec, out, stats);
    if !alive {
        dec.release_all(out);
    }
    alive
}

fn read_some(fd: i32, dec: &mut Decoder, out: &mut Vec<Out>, stats: &mut crate::stats::Stats) -> bool {
    let mut buf = [0u8; 24 * 64];
    for _ in 0..MAX_READS {
        // SAFETY: `buf` is valid for writes of its length; `fd` is the source's open file.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINTR) => true,
                _ => {
                    eprintln!("bitscomp: input: read: {e}");
                    false
                }
            };
        }
        if n == 0 {
            return false;
        }
        let now = mono_us();
        for ev in buf[..n as usize].chunks_exact(24) {
            let sec = i64::from_le_bytes(ev[0..8].try_into().unwrap_or_default());
            let usec = i64::from_le_bytes(ev[8..16].try_into().unwrap_or_default());
            let ty = u16::from_le_bytes([ev[16], ev[17]]);
            let code = u16::from_le_bytes([ev[18], ev[19]]);
            let value = i32::from_le_bytes([ev[20], ev[21], ev[22], ev[23]]);
            if ty == 0 && code == 0 {
                let at = sec.saturating_mul(1_000_000).saturating_add(usec);
                stats.input_events += 1;
                stats.input_age_us += now.saturating_sub(at).max(0) as u64;
            }
            if ty == 0 && code == SYN_DROPPED {
                stats.input_dropped += 1;
                eprintln!("bitscomp: input: SYN_DROPPED on fd {fd}: the kernel's ring overflowed, events were lost");
            }
            if ty == 1 && value == 1 {
                stats.key_presses += 1;
            }
            dec.feed(ty, code, value, out);
        }
    }
    true
}

fn mono_us() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1000
}

fn deliver(app: &mut App, o: Out) {
    let mode = app.output.current_mode().map(|m| m.size).unwrap_or_default();
    let size = (mode.w, mode.h);
    let s = app.theme.scale;
    let ev = match o {
        Out::Key { code, pressed } => InputEvent::Key { code, pressed },
        Out::Button { code, pressed } => InputEvent::Button { code, pressed },
        Out::Rel { dx, dy } => {
            let (x, y) = moved((app.pointer.x * s, app.pointer.y * s), (dx, dy), size);
            InputEvent::Motion { x, y }
        }
        Out::Abs { fx, fy } => {
            let (x, y) = placed(fx, fy, size);
            InputEvent::Motion { x, y }
        }
        Out::Wheel { steps } => InputEvent::Wheel { steps },
        Out::HWheel { steps } => InputEvent::HWheel { steps },
    };
    app.handle_input(ev);
}
