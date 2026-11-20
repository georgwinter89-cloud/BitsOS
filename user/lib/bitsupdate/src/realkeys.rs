//! The console's install step takes only Georg's real keyboard (Georg 15.11.: "keyboard-only for the install
//! step"), by bitsinstall's rule (MC-11): only keyboards that are no MCP or virtual device count
//! (`bitsinstall::console::injected_why`), each with EVIOC_BITSOS_PHYSICAL so events a program writes into it never
//! reach this reader, grabbed while the question stands, and the answer is a code of random letters the screen
//! shows, armed 2 s after it appeared. Typing it is "yes"; Esc or N is "no".

use bitsinstall::console::{self, ARM, CODE_LETTERS, KEY_ESC, KEY_N};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

const fn ioc(dir: u32, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | ((b'E' as u32) << 8) | nr
}
const EVIOCGID: u32 = ioc(2, 0x02, 8);
const EVIOCGNAME: u32 = ioc(2, 0x06, 256);
const EVIOCGPHYS: u32 = ioc(2, 0x07, 256);
const EVIOCGBIT_KEY: u32 = ioc(2, 0x21, 0x300 / 8);
const EVIOCGRAB: u32 = ioc(1, 0x90, 4);
/// crates/console/src/evdevfs.rs EVIOC_BITSOS_PHYSICAL (iow 0xb0, an int).
const EVIOC_BITSOS_PHYSICAL: u32 = ioc(1, 0xb0, 4);
const EVENT: usize = 24;
const O_NONBLOCK: i32 = 0o4000;

struct Kbd {
    f: std::fs::File,
    grabbed: bool,
}

impl Drop for Kbd {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = usys::dev::ioctl_val(self.f.as_raw_fd() as usize, EVIOCGRAB, 0);
        }
    }
}

fn text(f: &std::fs::File, req: u32) -> String {
    let mut b = vec![0u8; 256];
    match usys::dev::ioctl_buf(f.as_raw_fd() as usize, req, &mut b) {
        Ok(_) => String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned(),
        Err(_) => String::new(),
    }
}

/// The real keyboards, filtered and grabbed.
fn keyboards() -> Vec<Kbd> {
    let Ok(rd) = std::fs::read_dir("/dev/input") else { return Vec::new() };
    let mut v = Vec::new();
    for e in rd.flatten() {
        if !e.file_name().to_string_lossy().starts_with("event") {
            continue;
        }
        let Ok(f) = std::fs::OpenOptions::new().read(true).custom_flags(O_NONBLOCK).open(e.path()) else { continue };
        let fd = f.as_raw_fd() as usize;
        let mut bits = vec![0u8; 0x300 / 8];
        // A keyboard has the letters of the code.
        if usys::dev::ioctl_buf(fd, EVIOCGBIT_KEY, &mut bits).is_err() || CODE_LETTERS.iter().any(|(c, _)| bits[*c as usize / 8] & (1 << (c % 8)) == 0) {
            continue;
        }
        let mut id = [0u8; 8];
        let bus = if usys::dev::ioctl_buf(fd, EVIOCGID, &mut id).is_ok() { u16::from_ne_bytes([id[0], id[1]]) } else { 0 };
        if console::injected_why(bus, &text(&f, EVIOCGNAME), &text(&f, EVIOCGPHYS)).is_some() {
            continue;
        }
        let mut one = 1i32.to_ne_bytes();
        // Fail closed: a keyboard whose written events the kernel will not filter does not count.
        if usys::dev::ioctl_buf(fd, EVIOC_BITSOS_PHYSICAL, &mut one).is_err() {
            continue;
        }
        let grabbed = usys::dev::ioctl_val(fd, EVIOCGRAB, 1).is_ok();
        let mut k = Kbd { f, grabbed };
        let mut junk = [0u8; EVENT * 64];
        while matches!(k.f.read(&mut junk), Ok(n) if n > 0) {}
        v.push(k);
    }
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// No real keyboard, or the time ran out.
    Nothing(&'static str),
}

/// The code's letters as the screen shows them.
pub fn letters(code: &[u16]) -> String {
    code.iter().filter_map(|c| CODE_LETTERS.iter().find(|l| l.0 == *c).map(|l| l.1)).map(String::from).collect::<Vec<_>>().join(" ")
}

/// The rule over a stream of presses (pure): `code` typed in order counts once armed; a wrong letter starts the
/// code again; Esc or N is no.
pub struct Typing {
    code: Vec<u16>,
    at: usize,
}

impl Typing {
    pub fn new(code: Vec<u16>) -> Typing {
        Typing { code, at: 0 }
    }
    pub fn press(&mut self, key: u16, armed: bool) -> Option<Answer> {
        if key == KEY_ESC || key == KEY_N {
            return Some(Answer::No);
        }
        if !armed {
            self.at = 0;
            return None;
        }
        if self.code.get(self.at) == Some(&key) {
            self.at += 1;
        } else {
            self.at = usize::from(self.code.first() == Some(&key));
        }
        (self.at == self.code.len()).then_some(Answer::Yes)
    }
}

/// Asks with `show(code letters)` and waits at most `wait` for the code on a real keyboard.
pub fn ask(wait: Duration, show: &mut dyn FnMut(&str)) -> Answer {
    let Some(r) = console::random() else { return Answer::Nothing("keine Zufallszahlen") };
    let code = console::code_from(r);
    let mut kbds = keyboards();
    if kbds.is_empty() {
        return Answer::Nothing("keine echte Tastatur gefunden");
    }
    show(&letters(&code));
    let start = Instant::now();
    let mut armed_at = start + ARM;
    let mut t = Typing::new(code);
    let mut buf = [0u8; EVENT * 32];
    while start.elapsed() < wait {
        for k in kbds.iter_mut() {
            while let Ok(n) = k.f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                for ev in buf[..n - n % EVENT].chunks_exact(EVENT) {
                    let (kind, code, value) = (u16::from_ne_bytes([ev[16], ev[17]]), u16::from_ne_bytes([ev[18], ev[19]]), i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]));
                    if kind != 1 || value != 1 {
                        continue;
                    }
                    let now = Instant::now();
                    let armed = now >= armed_at;
                    if !armed {
                        // A key before the code was armed moves the arming (bitscomp's ACCESS_ARM).
                        armed_at = now + ARM;
                    }
                    if let Some(a) = t.press(code, armed) {
                        return a;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Answer::Nothing("die Zeit ist um")
}

/// The rule of one answer key (pure, VB-1's `bitsupdate frage`): Esc or N is "later" (Some(None)), a key of
/// `keys` counts once armed (Some(Some(key))), anything else waits (None).
pub fn one(key: u16, armed: bool, keys: &[u16]) -> Option<Option<u16>> {
    if key == KEY_ESC || key == KEY_N {
        return Some(None);
    }
    (armed && keys.contains(&key)).then_some(Some(key))
}

/// One key of `keys` on a real keyboard within `wait`, armed `ARM` after the call and again after every real key
/// before that. Ok(None): Esc, N or the time ran out; Err: no real keyboard.
pub fn one_key(wait: Duration, keys: &[u16]) -> Result<Option<u16>, &'static str> {
    let mut kbds = keyboards();
    if kbds.is_empty() {
        return Err("keine echte Tastatur gefunden");
    }
    let start = Instant::now();
    let mut armed_at = start + ARM;
    let mut buf = [0u8; EVENT * 32];
    while start.elapsed() < wait {
        for k in kbds.iter_mut() {
            while let Ok(n) = k.f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                for ev in buf[..n - n % EVENT].chunks_exact(EVENT) {
                    let (kind, code, value) = (u16::from_ne_bytes([ev[16], ev[17]]), u16::from_ne_bytes([ev[18], ev[19]]), i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]));
                    if kind != 1 || value != 1 {
                        continue;
                    }
                    let now = Instant::now();
                    let armed = now >= armed_at;
                    if !armed {
                        armed_at = now + ARM;
                    }
                    if let Some(a) = one(code, armed, keys) {
                        return Ok(a);
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_counts_once_armed_and_in_order() {
        let code = vec![30, 48, 46, 32];
        assert_eq!(letters(&code), "A B C D");
        let mut t = Typing::new(code.clone());
        assert_eq!(t.press(30, false), None);
        assert_eq!(t.press(48, true), None, "B alone after an unarmed A");
        let mut t = Typing::new(code.clone());
        for (i, k) in code.iter().enumerate() {
            assert_eq!(t.press(*k, true), if i == 3 { Some(Answer::Yes) } else { None });
        }
        let mut t = Typing::new(code.clone());
        assert_eq!((t.press(30, true), t.press(30, true), t.press(48, true), t.press(46, true), t.press(32, true)), (None, None, None, None, Some(Answer::Yes)), "a repeated first letter restarts at it");
        let mut t = Typing::new(code);
        assert_eq!(t.press(KEY_N, false), Some(Answer::No));
    }

    #[test]
    fn one_answer_key_counts_once_armed() {
        let keys = [36, 21];
        assert_eq!(one(36, false, &keys), None, "J before the arming");
        assert_eq!(one(36, true, &keys), Some(Some(36)));
        assert_eq!(one(21, true, &keys), Some(Some(21)));
        assert_eq!(one(21, true, &[36]), None, "Z without an older version waits");
        assert_eq!(one(57, true, &keys), None);
        assert_eq!(one(KEY_ESC, false, &keys), Some(None), "Esc is later at once");
        assert_eq!(one(KEY_N, true, &keys), Some(None));
    }
}
