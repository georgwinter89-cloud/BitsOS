//! Input on V2 without udev or libinput (WY-1a decision, notes wy-WY-1a.md): bitscomp opens `/dev/input/event*`
//! itself, probes each with EVIOCGBIT/EVIOCGABS (linux-6.6/drivers/input/evdev.c `evdev_do_ioctl`), grabs keyboards
//! and pointers (EVIOCGRAB: the VT gets nothing while bitscomp runs; PK-L1c: pads are never grabbed), reads whole `struct input_event`s and turns them into
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

use super::evdev_map::{Caps, Decoder, EV_ABS, EV_KEY, EV_REL, KEY_MAX, Out, Range, SYN_DROPPED, placed};
use crate::input::{InputEvent, Origin};
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
const fn eviocgphys(len: usize) -> u64 {
    ioc(2, 0x07, len as u64)
}
const EVIOCGID: u64 = ioc(2, 0x02, 8);
const EVIOCGRAB: u64 = ioc(1, 0x90, 4);
/// MC-9b (in the kernel since INT-W17, crates/console/src/evdevfs.rs): with 1, this reader no longer gets the events
/// programs write into the device (evdev_write, marked injected by the kernel), so everything it reads from a real
/// device is real. Linux has no such request (nr 0xb0 is free in 'E'); a kernel without it answers an error.
const EVIOC_BITSOS_PHYSICAL: u64 = ioc(1, 0xb0, 4);
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
            Ok(Some(p)) => {
                seen.borrow_mut().settle(key.clone());
                let ok = if !p.caps.grabs() { add_pad(handle, p, key) } else { add(handle, p.file, p.caps, &p.name, (p.origin, p.why), key) };
                if ok {
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

/// Opens and probes one node: Some when it is a keyboard, pointer or pad bitscomp uses.
/// The device's origin and, for an injected one, why (the log line says it).
type Counts = (Origin, String);

/// What `open` learnt of a device bitscomp uses.
struct Probe {
    file: File,
    caps: Caps,
    name: String,
    origin: Origin,
    why: String,
    /// PK-L1c, pads only: EV_KEY codes, vendor:product, the left stick's axes.
    keys: Vec<u16>,
    id: (u16, u16),
    stick: (Option<crate::pad::Axis>, Option<crate::pad::Axis>),
    /// The values of the stick's X/Y and the hat's X/Y when opened.
    rest: (Option<i32>, Option<i32>, (i32, i32)),
}

fn open(path: &Path) -> std::io::Result<Option<Probe>> {
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
    // struct input_absinfo: value, minimum, maximum, fuzz, flat, resolution.
    let absfull = |axis: u16| -> Option<(crate::pad::Axis, i32)> {
        let mut info = [0u8; 24];
        ioctl_buf(fd, eviocgabs(axis), &mut info).ok()?;
        let at = |i: usize| i32::from_le_bytes([info[i], info[i + 1], info[i + 2], info[i + 3]]);
        Some((crate::pad::Axis { min: at(4), max: at(8), flat: at(16) }, at(0)))
    };
    let absinfo = |axis: u16| absfull(axis).map(|(a, _)| a);
    let range = |axis: u16| absinfo(axis).map(|a| Range { min: a.min, max: a.max });
    let ranges = range(0).zip(range(1));
    let caps = Caps::from_bits(&keys, &rel, &abs, ranges);
    let mut nb = [0u8; 128];
    let n = ioctl_buf(fd, eviocgname(nb.len()), &mut nb).unwrap_or(0).max(0) as usize;
    let name = String::from_utf8_lossy(&nb[..n.min(nb.len())]).trim_end_matches('\0').to_string();
    if !caps.wanted() {
        return Ok(None);
    }
    let _ = ioctl_int(fd, EVIOCSCLOCKID, libc::CLOCK_MONOTONIC);
    let mut id = [0u8; 8];
    // A device that does not say its bus counts as injected (the safe side), and the log line says why: a passing
    // EINTR/EAGAIN is asked again a few times first, so a real keyboard is not marked injected by a hiccup.
    let mut got = ioctl_buf(fd, EVIOCGID, &mut id);
    for _ in 0..3 {
        match &got {
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) => got = ioctl_buf(fd, EVIOCGID, &mut id),
            _ => break,
        }
    }
    let mut pb = [0u8; 64];
    // Linux answers ENOENT for a device without phys (evdev.c str_to_user): an empty phys.
    let n = ioctl_buf(fd, eviocgphys(pb.len()), &mut pb).unwrap_or(0).max(0) as usize;
    let phys = String::from_utf8_lossy(&pb[..n.min(pb.len())]).trim_end_matches('\0').to_string();
    let (origin, why) = match got {
        Ok(_) => {
            let bus = u16::from_le_bytes([id[0], id[1]]);
            let why = super::evdev_map::injected_why(bus, &name, &phys);
            (super::evdev_map::origin_of(bus, &name, &phys), why.map_or(String::new(), |w| format!("{w}, bus {bus:#04x}")))
        }
        Err(e) => {
            let old = matches!(e.raw_os_error(), Some(libc::ENOTTY | libc::EINVAL));
            (Origin::Injected, format!("no EVIOCGID ({e}{})", if old { ", the kernel lacks it" } else { "" }))
        }
    };
    let has = |a: u16| abs.get(a as usize / 8).is_some_and(|b| b & (1 << (a % 8)) != 0);
    let (pad_keys, stick, rest) = if caps.pad {
        let (x, y) = (has(0).then(|| absfull(0)).flatten(), has(1).then(|| absfull(1)).flatten());
        let hat = |a: u16| has(a).then(|| absfull(a)).flatten().map_or(0, |(_, v)| v);
        let rest = (x.map(|(_, v)| v), y.map(|(_, v)| v), (hat(crate::pad::ABS_HAT0X), hat(crate::pad::ABS_HAT0Y)));
        (super::evdev_map::codes(&keys), (x.map(|(a, _)| a), y.map(|(a, _)| a)), rest)
    } else {
        (Vec::new(), (None, None), (None, None, (0, 0)))
    };
    let ids = (u16::from_le_bytes([id[2], id[3]]), u16::from_le_bytes([id[4], id[5]]));
    Ok(Some(Probe { file, caps, name, origin, why, keys: pad_keys, id: ids, stick, rest }))
}

fn add(handle: &LoopHandle<'static, App>, file: File, caps: Caps, name: &str, (origin, why): Counts, key: Key) -> bool {
    let fd = file.as_raw_fd();
    let grab = match ioctl_val(fd, EVIOCGRAB, 1) {
        Ok(()) => "grabbed".to_string(),
        Err(e) => format!("not grabbed: {e}"),
    };
    // MC-9b: what this device's input counts as for the installer's dialog (R8c).
    let counts = match origin {
        Origin::Injected => format!("counts as injected ({why})"),
        Origin::Physical => match ioctl_int(fd, EVIOC_BITSOS_PHYSICAL, 1) {
            Ok(()) => "real, written events filtered".to_string(),
            Err(e) => format!("real; the kernel does not filter written events ({e})"),
        },
    };
    let what = match (caps.keyboard, caps.pointer()) {
        (true, true) => "keyboard+pointer",
        (true, false) => "keyboard",
        _ => "pointer",
    };
    eprintln!("bitscomp: input: {} \"{name}\" {what} ({grab}), {counts}", key.0.display());
    let mut dec = Decoder::new(caps);
    let label = key.0.display().to_string();
    let r = handle.insert_source(Generic::new(file, Interest::READ, Mode::Level), move |_, _, app| {
        let mut out = Vec::new();
        let alive = pump(fd, &mut dec, &mut out, &mut app.stats);
        for o in out {
            deliver(app, o, origin);
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

/// PK-L1c: a pad is read but never grabbed (Steam, games and SDL read the same node; the kernel gives every
/// ungrabbed reader every event, crates/console/src/evdev.rs). Its raw events go to `App::pad_feed`; its state
/// lives in `App::pads` (registered on the loop's next turn, or by its first events, whichever comes first).
fn add_pad(handle: &LoopHandle<'static, App>, p: Probe, key: Key) -> bool {
    let fd = p.file.as_raw_fd();
    let counts = match p.origin {
        Origin::Injected => format!("counts as injected ({})", p.why),
        Origin::Physical => match ioctl_int(fd, EVIOC_BITSOS_PHYSICAL, 1) {
            Ok(()) => "real, written events filtered".to_string(),
            Err(e) => format!("real; the kernel does not filter written events ({e})"),
        },
    };
    let label = key.0.display().to_string();
    let id = crate::pad_app::next_id();
    let mut dev = crate::pad_app::PadDev::new(&label, &p.name, p.id, &p.keys, p.stick, p.origin);
    dev.pad.seed(p.rest.0, p.rest.1, p.rest.2);
    eprintln!("bitscomp: input: {label} \"{}\" pad (not grabbed: programs read it too), {counts}", p.name);
    let plug = Rc::new(RefCell::new(Some(dev)));
    let (first, unread) = (plug.clone(), plug.clone());
    let r = handle.insert_source(Generic::new(p.file, Interest::READ, Mode::Level), move |_, _, app| {
        if let Some(d) = plug.borrow_mut().take() {
            app.pad_plug(id, d);
        }
        let mut evs = Vec::new();
        let alive = read_some(fd, &mut |ty, code, value| evs.push((ty, code, value)), &mut app.stats);
        app.pad_feed(id, &evs);
        if alive {
            return Ok(PostAction::Continue);
        }
        app.pad_gone(id);
        Ok(PostAction::Remove)
    });
    if let Err(e) = r {
        unread.borrow_mut().take();
        eprintln!("bitscomp: input: {label}: pad not read: {e}");
        return false;
    }
    handle.insert_idle(move |app| {
        if let Some(d) = first.borrow_mut().take() {
            app.pad_plug(id, d);
        }
    });
    true
}

/// Reads what the device has, at most `MAX_READS` times; false when it is gone (ENODEV, end of file, any other
/// error), and then every key and button it held is released in `out`.
pub fn pump(fd: i32, dec: &mut Decoder, out: &mut Vec<Out>, stats: &mut crate::stats::Stats) -> bool {
    let alive = read_some(fd, &mut |ty, code, value| dec.feed(ty, code, value, out), stats);
    if !alive {
        dec.release_all(out);
    }
    alive
}

fn read_some(fd: i32, feed: &mut dyn FnMut(u16, u16, i32), stats: &mut crate::stats::Stats) -> bool {
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
            feed(ty, code, value);
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

fn deliver(app: &mut App, o: Out, origin: Origin) {
    let mode = app.output.current_mode().map(|m| m.size).unwrap_or_default();
    let size = (mode.w, mode.h);
    let ev = match o {
        Out::Key { code, pressed } => InputEvent::Key { code, pressed },
        Out::Button { code, pressed } => InputEvent::Button { code, pressed },
        Out::Rel { dx, dy } => crate::lock::relative_event(app, dx, dy),
        Out::Abs { fx, fy } => {
            let (x, y) = placed(fx, fy, size);
            InputEvent::Motion { x, y }
        }
        Out::Wheel { steps } => InputEvent::Wheel { steps },
        Out::HWheel { steps } => InputEvent::HWheel { steps },
    };
    app.handle_input_as(ev, origin);
}
