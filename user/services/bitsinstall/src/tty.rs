//! MC-11: the text console's devices for `bitsinstall::console`: the screen is /dev/tty0 (written, and asked with
//! KDGETMODE whether a program has the screen: linux-6.6/drivers/tty/vt/vt_ioctl.c), the keyboards are the
//! evdev nodes /dev/input/event* with keys (linux-6.6/drivers/input/evdev.c). Each device is judged by
//! bitscomp's rule (`console::injected_why`); a real one is grabbed for the dialog (EVIOCGRAB: the VT and its
//! shell get none of its keys), and given back at the dialog's end (EVIOCGRAB 0, then closed). Fixer (MC-11
//! review): a real keyboard also gets EVIOC_BITSOS_PHYSICAL, so events a program writes into its node
//! (crates/console evdevfs `Device::write`, injected) never reach this reader; a keyboard the kernel will not
//! filter counts as injected. Grab and ungrab wait (bounded) until no key is held, so neither the shell nor the
//! dialog is left with a press whose release the other got.

use crate::sys;
use bitsinstall::console::{self, Console, KEY_J, Key, Session};
use bitsinstall::log;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

pub const TTY: &str = "/dev/tty0";
pub const INPUT: &str = "/dev/input";

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | ((b'E' as u64) << 8) | nr
}
const READ: u64 = 2;
const WRITE: u64 = 1;
/// BitsOS's per-reader filter (crates/console/src/evdevfs.rs `EVIOC_BITSOS_PHYSICAL` = iow(0xb0, 4)), as bitscomp
/// sets it (backend/evdev.rs).
const EVIOC_BITSOS_PHYSICAL: u64 = ioc(WRITE, 0xb0, 4);
const fn eviocgkey(len: u64) -> u64 {
    ioc(READ, 0x18, len)
}
/// How long grab and ungrab wait for the held keys to be let go.
const RELEASE_WAIT: Duration = Duration::from_millis(1500);
const EVIOCGID: u64 = ioc(READ, 0x02, 8);
const fn eviocgname(len: u64) -> u64 {
    ioc(READ, 0x06, len)
}
const fn eviocgphys(len: u64) -> u64 {
    ioc(READ, 0x07, len)
}
const EV_KEY: u16 = 1;
/// KEY_MAX + 1 bits (linux-6.6/include/uapi/linux/input-event-codes.h KEY_MAX 0x2ff).
const KEY_BYTES: u64 = 0x300 / 8;
const fn eviocgbit_key(len: u64) -> u64 {
    ioc(READ, 0x20 + EV_KEY as u64, len)
}
const EVIOCGRAB: u64 = (1 << 30) | (4 << 16) | ((b'E' as u64) << 8) | 0x90;
const KDGETMODE: u64 = 0x4B3B;
const KD_TEXT: i32 = 0;
/// `struct input_event` on x86-64: timeval (16), type, code, value.
const EVENT: usize = 24;

struct Kbd {
    f: File,
    name: String,
    physical: bool,
    grabbed: bool,
}

fn text(fd: &File, req: u64, len: usize) -> String {
    let mut b = vec![0u8; len];
    match sys::ioctl_read(fd, req, &mut b) {
        Ok(_) => String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned(),
        Err(_) => String::new(),
    }
}

/// Every evdev node with the J key (keyboards), each judged real or injected.
fn keyboards(dir: &PathBuf) -> Vec<Kbd> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v = Vec::new();
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let Ok(f) = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC).open(e.path()) else { continue };
        let mut bits = vec![0u8; KEY_BYTES as usize];
        if sys::ioctl_read(&f, eviocgbit_key(KEY_BYTES), &mut bits).is_err() || bits[KEY_J as usize / 8] & (1 << (KEY_J % 8)) == 0 {
            continue;
        }
        let mut id = [0u8; 8];
        let bus = if sys::ioctl_read(&f, EVIOCGID, &mut id).is_ok() { u16::from_ne_bytes([id[0], id[1]]) } else { 0 };
        let name = text(&f, eviocgname(256), 256);
        let phys = text(&f, eviocgphys(256), 256);
        let physical = console::injected_why(bus, &name, &phys).is_none();
        v.push(Kbd { f, name, physical, grabbed: false });
    }
    v
}

/// Whether a key of this keyboard is held now (EVIOCGKEY; an error counts as none).
fn held(f: &File) -> bool {
    let mut b = vec![0u8; KEY_BYTES as usize];
    sys::ioctl_read(f, eviocgkey(KEY_BYTES), &mut b).is_ok() && b.iter().any(|&x| x != 0)
}

/// Waits (at most `RELEASE_WAIT`) until no real keyboard has a key held.
fn wait_released(kbds: &[Kbd]) {
    let end = std::time::Instant::now() + RELEASE_WAIT;
    while kbds.iter().any(|k| k.physical && held(&k.f)) && std::time::Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Drops what a keyboard has queued (keys from before the dialog are no answer).
fn discard(f: &mut File) {
    let mut buf = [0u8; EVENT * 32];
    while matches!(f.read(&mut buf), Ok(n) if n > 0) {}
}

fn text_mode(tty: &File) -> bool {
    let mut m = [0u8; 4];
    sys::ioctl_read(tty, KDGETMODE, &mut m).is_ok() && i32::from_ne_bytes(m) == KD_TEXT
}

fn open_tty(path: &PathBuf) -> std::io::Result<File> {
    OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC).open(path)
}

pub struct TextConsole {
    pub tty: PathBuf,
    pub input: PathBuf,
}

impl TextConsole {
    pub fn new() -> TextConsole {
        TextConsole { tty: PathBuf::from(TTY), input: PathBuf::from(INPUT) }
    }
}

impl Console for TextConsole {
    fn usable(&self) -> bool {
        open_tty(&self.tty).is_ok_and(|t| text_mode(&t)) && keyboards(&self.input).iter().any(|k| k.physical)
    }

    fn open(&self) -> Result<Box<dyn Session>, String> {
        let out = open_tty(&self.tty).map_err(|e| format!("{}: {e}", self.tty.display()))?;
        let mut kbds = keyboards(&self.input);
        for k in kbds.iter_mut().filter(|k| k.physical) {
            // Fail closed: a real keyboard whose written events the kernel will not drop counts as injected.
            if let Err(e) = sys::ioctl_int(&k.f, EVIOC_BITSOS_PHYSICAL, 1) {
                log::warn(&format!("text console: keyboard \"{}\": EVIOC_BITSOS_PHYSICAL errno {e}: its keys count as injected", k.name));
                k.physical = false;
            }
        }
        wait_released(&kbds);
        for k in kbds.iter_mut() {
            let how = if !k.physical {
                "injected: its keys are ignored"
            } else if let Err(e) = sys::ioctl_val(&k.f, EVIOCGRAB, 1) {
                log::warn(&format!("text console: keyboard \"{}\": EVIOCGRAB errno {e}: its keys also reach the shell", k.name));
                "real, written events filtered, not grabbed"
            } else {
                k.grabbed = true;
                "real, written events filtered, grabbed for the dialog"
            };
            discard(&mut k.f);
            log::info(&format!("text console: keyboard \"{}\" ({how})", k.name));
        }
        if !kbds.iter().any(|k| k.physical) {
            return Err(String::from("no real keyboard"));
        }
        Ok(Box::new(TtySession { out, kbds, pending: VecDeque::new(), write_failed: false }))
    }
}

struct TtySession {
    out: File,
    kbds: Vec<Kbd>,
    pending: VecDeque<Key>,
    write_failed: bool,
}

impl TtySession {
    /// Reads what one keyboard has; a device that went away is dropped.
    fn drain(&mut self, i: usize) -> bool {
        let mut buf = [0u8; EVENT * 32];
        let k = &mut self.kbds[i];
        loop {
            match k.f.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => {
                    for ev in buf[..n - n % EVENT].chunks_exact(EVENT) {
                        let kind = u16::from_ne_bytes([ev[16], ev[17]]);
                        let code = u16::from_ne_bytes([ev[18], ev[19]]);
                        let value = i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]);
                        // Presses only: a repeat (2) or a release (0) is no new key.
                        if kind == EV_KEY && value == 1 {
                            self.pending.push_back(Key { code, physical: k.physical });
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    log::warn(&format!("text console: keyboard \"{}\": {e}; no longer read", k.name));
                    return false;
                }
            }
        }
    }
}

impl Drop for TtySession {
    /// Gives the keyboards back once no key is held (bounded), so the shell sees no release without its press.
    fn drop(&mut self) {
        wait_released(&self.kbds);
        for k in self.kbds.iter().filter(|k| k.grabbed) {
            if let Err(e) = sys::ioctl_val(&k.f, EVIOCGRAB, 0) {
                log::warn(&format!("text console: keyboard \"{}\": ungrab errno {e} (closing it ungrabs)", k.name));
            }
        }
    }
}

impl Session for TtySession {
    fn show(&mut self, text: &str) {
        if let Err(e) = self.out.write_all(text.as_bytes())
            && !self.write_failed
        {
            self.write_failed = true;
            log::warn(&format!("text console: {TTY}: {e}"));
        }
    }

    fn key(&mut self, wait: Duration) -> Option<Key> {
        if let Some(k) = self.pending.pop_front() {
            return Some(k);
        }
        let fds: Vec<i32> = self.kbds.iter().map(|k| k.f.as_raw_fd()).collect();
        let ready = sys::poll_in(&fds, wait.as_millis().min(i32::MAX as u128) as i32);
        if fds.is_empty() {
            std::thread::sleep(wait);
        }
        for i in (0..ready.len()).rev() {
            if ready[i] && !self.drain(i) {
                self.kbds.remove(i);
            }
        }
        self.pending.pop_front()
    }

    fn visible(&mut self) -> bool {
        text_mode(&self.out)
    }
}
