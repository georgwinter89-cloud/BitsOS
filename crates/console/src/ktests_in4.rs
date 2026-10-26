//! IN-4 ktests in the kernel: the console terminal's ioctls and raw input through the real VT, the keyboard
//! modes and the screen hand-over, the evdev nodes (MCP's injected keyboard: Safe, it is grabbed while the test
//! types; fake devices: QemuOnly). All restore what they change, so they can run again in the same boot.

use crate::evdev::{self, Caps, DeviceInfo, EV_KEY, EV_SYN, InputEvent, SYN_REPORT};
use crate::evdevfs::{self, EVENT_SIZE, Handle, INJECT_KEYBOARD, ioc};
use crate::keymap::code;
use crate::termios::{Termios, cc, flag, ioc as tio};
use crate::{Console, tty, vt};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use ktest::{TestResult, kassert, ktest};
use uapi::errno::*;

const WAIT_NS: u64 = 5_000_000_000;

/// Kernel memory standing in for a program's (one buffer at `BASE`).
pub struct KMem(RefCell<Vec<u8>>);
pub const BASE: u64 = 0x10_0000;
impl KMem {
    pub fn new() -> KMem {
        KMem(RefCell::new(vec![0; 4096]))
    }
    pub fn at(&self, off: usize, n: usize) -> Vec<u8> {
        self.0.borrow()[off..off + n].to_vec()
    }
    pub fn put(&self, off: usize, b: &[u8]) {
        self.0.borrow_mut()[off..off + b.len()].copy_from_slice(b);
    }
    pub fn i32_at(&self, off: usize) -> i32 {
        let b = self.at(off, 4);
        i32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}
impl Default for KMem {
    fn default() -> Self {
        Self::new()
    }
}
impl vfs::sys::UserMem for KMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let m = self.0.borrow();
        match addr.checked_sub(BASE) {
            Some(o) if o as usize + buf.len() <= m.len() => {
                buf.copy_from_slice(&m[o as usize..o as usize + buf.len()]);
                true
            }
            _ => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut m = self.0.borrow_mut();
        match addr.checked_sub(BASE) {
            Some(o) if o as usize + data.len() <= m.len() => {
                m[o as usize..o as usize + data.len()].copy_from_slice(data);
                true
            }
            _ => false,
        }
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// An injected key event as MCP's `send_keys` makes it.
pub fn inject(code: u16, value: i32) {
    let s = evdev::sink();
    let t = time::now_ns();
    s.push(InputEvent { time_ns: t, kind: EV_KEY, code, value, device: INJECT_KEYBOARD, injected: true });
    s.push(InputEvent { time_ns: t, kind: EV_SYN, code: SYN_REPORT, value: 0, device: INJECT_KEYBOARD, injected: true });
}
pub fn stroke(mods: &[u16], key: u16) {
    for &m in mods {
        inject(m, 1);
    }
    inject(key, 1);
    inject(key, 0);
    for &m in mods.iter().rev() {
        inject(m, 0);
    }
}
/// Waits until the VT took everything pushed so far, and a little longer.
pub fn settle() -> TestResult {
    let end = time::now_ns() + WAIT_NS;
    while vt::input_pending() > 0 {
        kassert!(time::now_ns() < end, "the VT did not take the events within 5 s");
        sched::sleep_ns(5_000_000);
    }
    sched::sleep_ns(50_000_000);
    Ok(())
}

fn drain_tty() -> Vec<u8> {
    let mut out = Vec::new();
    let mut b = [0u8; 64];
    while let Ok(n) = crate::tty().read(&mut b, true, &|| false, false) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&b[..n]);
    }
    out
}

/// TCGETS/TCSETS/TIOCGWINSZ/FIONREAD on the console through `ioctl_user`, exact sizes; a kernel caller has no
/// controlling terminal (TIOCGPGRP: ENOTTY).
fn tty_ioctls() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let t = crate::tty();
    let saved = t.termios();
    let m = KMem::new();
    let r = (|| -> TestResult {
        m.put(0, &[0xEE; 64]);
        kassert!(crate::ConsoleDev::ioctl_user(tio::TCGETS, BASE, &m) == Ok(0));
        kassert!(m.at(0, 36) == saved.to_termios() && m.at(36, 4) == [0xEE; 4], "TCGETS: {:?}", m.at(0, 40));
        let mut raw = saved;
        raw.lflag &= !(flag::ICANON | flag::ECHO);
        raw.cc[cc::VMIN] = 0;
        m.put(100, &raw.to_termios());
        kassert!(crate::ConsoleDev::ioctl_user(tio::TCSETSW, BASE + 100, &m) == Ok(0));
        kassert!(t.termios().lflag == raw.lflag && t.is_raw(), "TCSETSW did not take");
        kassert!(crate::ConsoleDev::ioctl_user(tio::TCSETS, 0x10, &m) == Err(EFAULT));
        kassert!(crate::ConsoleDev::ioctl_user(tio::TIOCGWINSZ, BASE + 200, &m) == Ok(0));
        let (cols, rows) = vt::size();
        let w = m.at(200, 8);
        kassert!(u16::from_le_bytes([w[0], w[1]]) as u32 == rows && u16::from_le_bytes([w[2], w[3]]) as u32 == cols, "winsize {w:?} vs grid {cols}x{rows}");
        kassert!(crate::ConsoleDev::ioctl_user(tio::TIOCINQ, BASE + 300, &m) == Ok(0));
        kassert!(crate::ConsoleDev::ioctl_user(tio::TIOCGPGRP, BASE + 300, &m) == Err(ENOTTY));
        kassert!(crate::ConsoleDev::ioctl_user(tio::KDGETMODE, BASE + 300, &m) == Ok(0) && m.i32_at(300) == 0, "KD_TEXT");
        kassert!(crate::ConsoleDev::ioctl_user(0x5499, BASE, &m) == Err(ENOTTY));
        Ok(())
    })();
    t.set_termios(saved, false);
    r
}
ktest!("in.tty.ioctls", Safe, tty_ioctls);

/// Keys typed (injected) through the VT reach a raw terminal as a Linux console sends them; K_MEDIUMRAW and
/// K_RAW give key codes and scancodes; K_OFF and KD_GRAPHICS type nothing (focus).
fn raw_input() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if display::device().is_none() {
        return ktest::skip("no display device (no VT)");
    }
    let t = crate::tty();
    let saved = t.termios();
    let r = (|| -> TestResult {
        stroke(&[code::LEFTCTRL], 22); // Ctrl+U: nothing half typed is left
        settle()?;
        t.set_raw(true);
        drain_tty();
        stroke(&[], 30); // a
        stroke(&[code::LEFTCTRL], 46); // Ctrl+C: 0x03, no signal in raw mode
        stroke(&[], code::UP);
        stroke(&[], code::ENTER);
        stroke(&[], 40); // ä
        settle()?;
        let got = drain_tty();
        kassert!(got == b"a\x03\x1b[A\r\xc3\xa4", "raw bytes {got:x?}");
        vt::set_kb_mode(tio::K_MEDIUMRAW as u8, None);
        stroke(&[], 30);
        settle()?;
        let got = drain_tty();
        kassert!(got == [30, 30 | 0x80], "K_MEDIUMRAW {got:x?}");
        vt::set_kb_mode(tio::K_RAW as u8, None);
        stroke(&[], code::UP);
        settle()?;
        let got = drain_tty();
        kassert!(got == [0xE0, 0x48, 0xE0, 0xC8], "K_RAW {got:x?}");
        vt::set_kb_mode(tio::K_OFF as u8, None);
        stroke(&[], 30);
        settle()?;
        kassert!(drain_tty().is_empty(), "K_OFF typed");
        vt::set_kb_mode(tio::K_UNICODE as u8, None);
        vt::set_kd_mode(true, None);
        stroke(&[], 30);
        settle()?;
        kassert!(drain_tty().is_empty(), "the VT typed while a program had the screen");
        Ok(())
    })();
    vt::set_kd_mode(false, None);
    vt::set_kb_mode(tio::K_UNICODE as u8, None);
    t.set_termios(saved, false);
    t.flush();
    r
}
ktest!("in.tty.raw_input", Safe, raw_input);

/// VMIN 0 / VTIME 1 waits about 100 ms and returns 0; VMIN 0 / VTIME 0 returns at once (a private terminal).
fn vtime() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let t = tty::Tty::new();
    t.set_raw(true);
    let mut x = t.termios();
    x.cc[cc::VMIN] = 0;
    x.cc[cc::VTIME] = 1;
    t.set_termios(x, false);
    let mut b = [0u8; 4];
    let t0 = time::now_ns();
    kassert!(t.read(&mut b, false, &|| false, false) == Ok(0));
    let waited = time::now_ns() - t0;
    // Judged on the Mini-PC only (want 100 ms and a little): the number for the desk's round.
    klog::event!(Info, "console", "ktest.vtime", waited_us = waited / 1000; "VMIN 0 VTIME 1 read");
    kassert!((50_000_000..5_000_000_000).contains(&waited), "VTIME 1 waited {waited} ns");
    x.cc[cc::VTIME] = 0;
    t.set_termios(x, false);
    kassert!(t.read(&mut b, false, &|| false, false) == Ok(0));
    // VMIN 3 VTIME 1: after the first byte the inter-byte timer ends the read with what came.
    x.cc[cc::VMIN] = 3;
    x.cc[cc::VTIME] = 1;
    t.set_termios(x, false);
    t.input(b"z", false, &mut |_| {});
    kassert!(t.read(&mut b, false, &|| false, false) == Ok(1), "inter-byte timer");
    Ok(())
}
ktest!("in.tty.vtime", Safe, vtime);

/// KD_GRAPHICS: the VT draws nothing (the frame stays as the program left it); KD_TEXT draws the console again;
/// a dead owner gives the screen back (`reclaim`).
fn kd_mode() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if display::device().is_none() {
        return ktest::skip("no display device");
    }
    let r = (|| -> TestResult {
        vt::set_kd_mode(true, None);
        kassert!(vt::graphics());
        let (_, _, before) = display::snapshot().ok_or("no frame")?;
        crate::console().write_str("\nin.tty.kd_mode: written while a program has the screen\n", crate::FG);
        let (_, _, after) = display::snapshot().ok_or("no frame")?;
        kassert!(before == after, "the VT drew into the frame in KD_GRAPHICS");
        vt::set_kd_mode(false, None);
        let (_, _, back) = display::snapshot().ok_or("no frame")?;
        kassert!(back != after, "KD_TEXT did not draw the console again");
        // Owned by a process that does not exist: the next reclaim gives the screen back.
        vt::set_kd_mode(true, Some(0x7FFF_FFF0));
        vt::reclaim();
        kassert!(!vt::graphics(), "the screen stayed with a process that is gone");
        Ok(())
    })();
    vt::set_kd_mode(false, None);
    r
}
ktest!("in.tty.kd_mode", Safe, kd_mode);

fn mcp_keyboard() -> Result<Arc<evdev::InputDevice>, String> {
    evdev::devices().into_iter().find(|d| d.alias == Some(INJECT_KEYBOARD)).ok_or_else(|| String::from("no MCP keyboard device"))
}

fn events(h: &Handle) -> Vec<(u16, u16, i32, u64)> {
    let mut b = [0u8; EVENT_SIZE * 32];
    let n = h.read(&mut b, true, &|| false).unwrap_or(0);
    b[..n]
        .chunks(EVENT_SIZE)
        .map(|e| {
            let sec = i64::from_le_bytes([e[0], e[1], e[2], e[3], e[4], e[5], e[6], e[7]]) as u64;
            let usec = i64::from_le_bytes([e[8], e[9], e[10], e[11], e[12], e[13], e[14], e[15]]) as u64;
            (u16::from_le_bytes([e[16], e[17]]), u16::from_le_bytes([e[18], e[19]]), i32::from_le_bytes([e[20], e[21], e[22], e[23]]), sec * 1_000_000_000 + usec * 1000)
        })
        .collect()
}

/// MCP's keyboard has its node (/dev/input/eventN, crw-rw---- uid 1000, 13:64+N); a grabbing client reads the
/// injected keys as 24-byte events on CLOCK_MONOTONIC and the VT does not type them.
fn mcp_node() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let dev = mcp_keyboard()?;
    evdevfs::sync();
    let path = format!("/dev/input/event{}", dev.number);
    let node = vfs::root_ns().lookup(&path).map_err(|e| format!("{path}: errno {e}"))?;
    let mode = node.inode.meta.mode();
    kassert!(mode == uapi::S_IFCHR | 0o660, "{path} mode {mode:o}");
    let f = vfs::node::file_of(&node);
    kassert!(f.rdev() == vfs::abi::encode_dev(13, 64 + dev.number), "rdev");
    let h = Handle::open(&dev).map_err(|e| format!("open: {e}"))?;
    let m = KMem::new();
    kassert!(h.ioctl(ioc::ior(ioc::NAME, 64), BASE, &m) == Ok(20), "EVIOCGNAME");
    kassert!(m.at(0, 20) == b"BitsOS MCP keyboard\0");
    m.put(0, &1i32.to_le_bytes());
    kassert!(h.ioctl(ioc::EVIOCSCLOCKID, BASE, &m) == Ok(0));
    kassert!(h.ioctl(ioc::EVIOCGRAB, 1, &m) == Ok(0), "grab");
    let editing = crate::tty().editing();
    let t0 = time::now_ns();
    let r = (|| -> TestResult {
        events(&h);
        inject(16, 1); // KEY_Q
        inject(16, 0);
        let end = time::now_ns() + WAIT_NS;
        while !h.events().contains(vfs::Events::IN) {
            kassert!(time::now_ns() < end, "no event within 5 s");
            sched::sleep_ns(5_000_000);
        }
        sched::sleep_ns(20_000_000);
        let got = events(&h);
        let kinds: Vec<(u16, u16, i32)> = got.iter().map(|e| (e.0, e.1, e.2)).collect();
        kassert!(kinds == [(EV_KEY, 16, 1), (EV_SYN, SYN_REPORT, 0), (EV_KEY, 16, 0), (EV_SYN, SYN_REPORT, 0)], "events {kinds:?}");
        let now = time::now_ns();
        kassert!(got.iter().all(|e| e.3 + 1000 >= t0 && e.3 <= now), "timestamps not on CLOCK_MONOTONIC: {got:?} ({t0}..{now})");
        settle()?;
        kassert!(crate::tty().editing() == editing, "the VT typed a grabbed key");
        Ok(())
    })();
    let _ = h.ioctl(ioc::EVIOCGRAB, 0, &m);
    r
}
ktest!("in.evdev.mcp_node", Safe, mcp_node);

/// desk fix typeloss: 2500 strokes (10000 events) injected as MCP does (`push_paced`, from another thread)
/// into a /dev/input reader of MCP's keyboard that reads slowly (16 events, then 1 ms) lose nothing: no
/// SYN_DROPPED, every press and release in order, nothing counted as lost. Unpaced, the reader's 64-event ring
/// overflowed by chunks of 63 events (the 15-16 characters missing from foot's lines). Safe: grabbed while it
/// types, so the console types nothing.
fn paced_burst() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let dev = mcp_keyboard()?;
    let h = Handle::open(&dev).map_err(|e| format!("open: {e}"))?;
    let m = KMem::new();
    kassert!(h.ioctl(ioc::EVIOCGRAB, 1, &m) == Ok(0), "grab");
    let r = (|| -> TestResult {
        events(&h);
        const N: usize = 2500;
        let keys = [16u16, 17, 18, 19, 20, 21, 22, 23, 24, 25];
        let lost_before = evdev::sink().readers_lost(INJECT_KEYBOARD);
        let t = sched::spawn("ktest-paced", sched::Class::Normal, move || {
            let mut v = Vec::with_capacity(N * 4);
            for i in 0..N {
                let code = keys[i % keys.len()];
                for (kind, code, value) in [(EV_KEY, code, 1), (EV_SYN, SYN_REPORT, 0), (EV_KEY, code, 0), (EV_SYN, SYN_REPORT, 0)] {
                    v.push(InputEvent { time_ns: time::now_ns(), kind, code, value, device: INJECT_KEYBOARD, injected: true });
                }
            }
            evdev::sink().push_paced(&v);
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        let mut got: Vec<(u16, i32)> = Vec::new();
        let mut dropped = 0;
        let mut reads = 0u64;
        let t0 = time::now_ns();
        let end = t0 + 6 * WAIT_NS;
        let mut b = [0u8; EVENT_SIZE * 16];
        while got.len() < 2 * N {
            kassert!(time::now_ns() < end, "only {} of {} key events within 30 s ({reads} reads)", got.len(), 2 * N);
            let n = h.read(&mut b, true, &|| false).unwrap_or(0);
            for e in b[..n].chunks(EVENT_SIZE) {
                let (kind, code, value) = (u16::from_le_bytes([e[16], e[17]]), u16::from_le_bytes([e[18], e[19]]), i32::from_le_bytes([e[20], e[21], e[22], e[23]]));
                if kind == EV_SYN && code == evdevfs::SYN_DROPPED {
                    dropped += 1;
                }
                if kind == EV_KEY && value != 2 {
                    got.push((code, value));
                }
            }
            reads += 1;
            sched::sleep_ns(1_000_000);
        }
        let took = time::now_ns() - t0;
        klog::event!(Info, "input", "ktest.paced_burst", reads = reads, took_ms = took / 1_000_000);
        kassert!(t.join_timeout(WAIT_NS), "the injector did not end");
        kassert!(dropped == 0, "{dropped} SYN_DROPPED");
        let want: Vec<(u16, i32)> = (0..N).flat_map(|i| [(keys[i % keys.len()], 1), (keys[i % keys.len()], 0)]).collect();
        kassert!(got == want, "key events differ ({} got)", got.len());
        kassert!(evdev::sink().readers_lost(INJECT_KEYBOARD) == lost_before, "the reader lost events");
        settle()
    })();
    let _ = h.ioctl(ioc::EVIOCGRAB, 0, &m);
    r
}
ktest!("in.evdev.paced_burst", Safe, paced_burst);

/// A key held on a device with EV_REP repeats to its evdev reader (value 2, SYN_REPORT value 1) through the
/// `evdev-repeat` thread, also while grabbed, and stops at the release. Safe: MCP's keyboard, grabbed, so the
/// console types nothing. No timing is judged (only that a repeat came at all within 5 s).
fn repeat() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let dev = mcp_keyboard()?;
    let h = Handle::open(&dev).map_err(|e| format!("open: {e}"))?;
    let m = KMem::new();
    kassert!(h.ioctl(ioc::EVIOCGRAB, 1, &m) == Ok(0), "grab");
    let r = (|| -> TestResult {
        events(&h);
        inject(30, 1); // KEY_A
        let end = time::now_ns() + WAIT_NS;
        let mut got = Vec::new();
        while !got.iter().any(|e: &(u16, u16, i32, u64)| e.2 == 2) {
            kassert!(time::now_ns() < end, "no repeat within 5 s: {got:?}");
            sched::sleep_ns(20_000_000);
            got.extend(events(&h));
        }
        inject(30, 0);
        sched::sleep_ns(20_000_000);
        got.extend(events(&h));
        let kinds: Vec<(u16, u16, i32)> = got.iter().map(|e| (e.0, e.1, e.2)).collect();
        kassert!(kinds[..2] == [(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)], "press first: {kinds:?}");
        kassert!(kinds[2..4] == [(EV_KEY, 30, 2), (EV_SYN, SYN_REPORT, 1)], "then the repeat: {kinds:?}");
        kassert!(kinds.ends_with(&[(EV_KEY, 30, 0), (EV_SYN, SYN_REPORT, 0)]), "release last: {kinds:?}");
        sched::sleep_ns(200_000_000);
        let after = events(&h);
        kassert!(after.is_empty(), "repeats after the release: {after:?}");
        Ok(())
    })();
    if dev.held_keys().contains(&30) {
        inject(30, 0);
    }
    let _ = h.ioctl(ioc::EVIOCGRAB, 0, &m);
    r
}
ktest!("in.evdev.repeat", Safe, repeat);

/// A fake keyboard gets its node at once, a reader that does not read gets SYN_DROPPED, the node goes when the
/// device goes and an open client answers ENODEV. QemuOnly: it registers a device.
fn fake_device() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let mut caps = Caps::default();
    caps.keys.extend([1u16, 30, 31]);
    let k = evdev::register(evdev::sink(), DeviceInfo { name: "in4 fake keyboard".to_string(), bus: evdev::BUS_VIRTUAL, caps, ..DeviceInfo::default() });
    let path = format!("/dev/input/event{}", k.number);
    let r = (|| -> TestResult {
        evdevfs::sync();
        kassert!(vfs::root_ns().lookup(&path).is_ok(), "{path} not made");
        let h = Handle::open(&k).map_err(|e| format!("open: {e}"))?;
        let m = KMem::new();
        kassert!(h.ioctl(ioc::EVIOCGRAB, 1, &m) == Ok(0));
        for i in 0..100u64 {
            k.emit(time::now_ns(), &[(EV_KEY, 30, (i % 2 == 0) as i32), (EV_SYN, SYN_REPORT, 0)]);
        }
        let got = events(&h);
        kassert!(got.first().map(|e| (e.0, e.1)) == Some((EV_SYN, evdevfs::SYN_DROPPED)), "no SYN_DROPPED first: {:?}", got.first());
        k.unregister(time::now_ns());
        evdevfs::sync();
        kassert!(vfs::root_ns().lookup(&path).is_err(), "{path} still there");
        kassert!(h.read(&mut [0u8; 24], true, &|| false) == Err(ENODEV));
        kassert!(h.events().contains(vfs::Events::HUP));
        Ok(())
    })();
    if !k.is_gone() {
        k.unregister(time::now_ns());
    }
    r
}
ktest!("in.evdev.fake_device", QemuOnly, fake_device);

/// What the console's terminal answers before a program changed it: Linux's VT settings.
fn start_settings() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    let t = crate::tty().termios();
    kassert!(t == Termios::vt() || crate::tty().owned(), "termios {t:?}");
    Ok(())
}
ktest!("in.tty.start_settings", Safe, start_settings);
