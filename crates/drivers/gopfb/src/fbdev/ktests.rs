//! In-kernel tests of DP-2 (`dp.fbdev.*`). All Safe and rerunnable in one boot: every program a test starts has
//! ended before it returns, fb0's var and colour map are left as found, the screen is back with the console.
//! The programs come from `user/abi-tests` (`/bin/abi-fbdev`, built into every image that has the musl sysroot);
//! a missing one fails the test, as UL's and AB's program tests do.

use super::abi::{self, UserIo, Var};
use super::{device, fix_node_owner, stats, Fb, NODE, NODE_GID, NODE_UID};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;
use ktest::{kassert, ktest, TestResult};
use proc::stdio::{Capture, Input};
use proc::{Process, SpawnOptions};

pub(super) const SEC: u64 = 1_000_000_000;
pub(super) const MS: u64 = 1_000_000;
pub(super) const PROGRAM: &str = "/bin/abi-fbdev";

pub(super) fn fb() -> Result<&'static Arc<Fb>, String> {
    device().ok_or_else(|| "no fbdev device (see the fbdev ready/absent/memory.failed events)".to_string())
}

pub(super) fn need_program() -> TestResult {
    if vfs::root_ns().lookup(PROGRAM).is_err() {
        // Images built without the musl sysroot have no user/abi-tests (MC-5b, RB-7a requests).
        return ktest::skip("/bin/abi-fbdev is not in this image (user/abi-tests need the musl sysroot)");
    }
    Ok(())
}

pub(super) fn text(c: &Capture) -> String {
    String::from_utf8_lossy(&c.contents()).into_owned()
}

pub(super) struct Started {
    pub p: Arc<Process>,
    pub out: Arc<Capture>,
    pub err: Arc<Capture>,
}

pub(super) fn start(argv: &[&str]) -> Result<Started, String> {
    start_at(PROGRAM, argv)
}

/// Starts another program of user/abi-tests (`/bin/abi-<stem>`) the same way.
pub(super) fn start_at(path: &str, argv: &[&str]) -> Result<Started, String> {
    let out = Capture::new(1 << 20);
    let err = Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin"];
    let mut o = SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdin = Some(Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
    Ok(Started { p, out, err })
}

/// Waits for the end; kills the process when it takes longer than `timeout_ns`.
pub(super) fn finish(s: &Started, timeout_ns: u64) -> Result<i32, String> {
    match s.p.wait(Some(timeout_ns)) {
        Some(c) => Ok(c),
        None => {
            s.p.kill(uapi::ab::signal::SIGKILL);
            let _ = s.p.wait(Some(10 * SEC));
            Err(format!("{PROGRAM} did not end within {} s (killed); stdout:\n{}\nstderr:\n{}", timeout_ns / SEC, text(&s.out), text(&s.err)))
        }
    }
}

/// Polls `cond` every 10 ms up to `timeout_ns`.
pub(super) fn wait_for(timeout_ns: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = time::now_ns() + timeout_ns;
    loop {
        if cond() {
            return true;
        }
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(10 * MS);
    }
}

/// The console has the screen again (fbdev gave it back and nobody else holds it).
pub(super) fn console_back(fb: &Fb, timeout_ns: u64) -> bool {
    wait_for(timeout_ns, || fb.holder_pid() == 0 && display::scanout_holder().is_none())
}

/// The bound of the functional check `handed_back`: the `fbdev` thread is woken when the holder's fb0 closes
/// (its exit) or the console asks (`request_scanout_back`), its 100 ms poll is the fallback, then the hand-back
/// copies the console frame once (~1.7 ms at 1440p on the Mini-PC). 5 s only says "it comes back"; that the
/// wakes happen is checked by the `wakes_*` counters, how fast on the Mini-PC from the `dp ktest.fbdev_handback`
/// event (DP-2f: a 1 s bound failed once there in a full Safe run).
pub(super) const HANDBACK_BOUND_NS: u64 = 5 * SEC;

/// Above this a hand-back is reported as slow (Warn `dp ktest.fbdev_handback_slow`, not a failure: timing is not
/// judged in QEMU, and a run pinned to CPU 0 may wait for its turn). Woken hand-backs take 0-22 ms in QEMU; the
/// path that caused the flake (the 1 s idle wait) took 983 ms, the 100 ms poll alone about 100 ms.
pub(super) const HANDBACK_SLOW_MS: u64 = 250;

/// The console gets the screen back after `what` (a program ended or asked for it): waits up to
/// `HANDBACK_BOUND_NS`, reports the time and fbdev's part of it (event `dp ktest.fbdev_handback`), and on failure
/// names who holds the screen and whether the fbdev thread ran meanwhile.
pub(super) fn handed_back(fb: &Fb, what: &str) -> TestResult {
    let t0 = time::now_ns();
    let passes = stats().passes;
    let ok = console_back(fb, HANDBACK_BOUND_NS);
    let ms = (time::now_ns() - t0) / MS;
    // The console is back once the display is free; fbdev records its hand-back after the console frame is shown.
    if ok {
        wait_for(100 * MS, || super::handback::last().at_ns >= t0);
    }
    let h = super::handback::last();
    let ago_ms = time::now_ns().saturating_sub(h.at_ns) / MS;
    klog::event!(Info, "dp", "ktest.fbdev_handback", after = what, ok = ok, ms = ms, pid = h.pid, reason = h.reason,
        release_us = h.release_ns / 1000, show_us = h.show_ns / 1000, pixels = h.pixels, presented = h.presented,
        ago_ms = ago_ms);
    if ok && ms > HANDBACK_SLOW_MS {
        klog::event!(Warn, "dp", "ktest.fbdev_handback_slow", after = what, ms = ms, bound_ms = HANDBACK_SLOW_MS,
            passes = stats().passes - passes; "the console got the screen back late: a missing wake of the fbdev thread, or CPU 0 busy");
    }
    kassert!(
        ok,
        "the console did not get the screen back within {} s after {what}: fbdev holder {}, display {:?}, fbdev passes \
         meanwhile {}, last hand-back {:?} ({ago_ms} ms ago)",
        HANDBACK_BOUND_NS / SEC,
        fb.holder_pid(),
        display::scanout_holder(),
        stats().passes - passes,
        h
    );
    Ok(())
}

/// The program's pixel (user/abi-tests/c/fbdev.c `pat`).
pub(super) fn pat(seed: u32, x: u32, y: u32) -> u32 {
    (x.wrapping_mul(3).wrapping_add(seed.wrapping_mul(7)) & 0xff) << 16
        | (y.wrapping_mul(5).wrapping_add(seed.wrapping_mul(11)) & 0xff) << 8
        | ((x ^ y ^ seed) & 0xff)
}

/// The "drawn a= b= band=r+n top= double=" line of `abi-fbdev draw`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Drawn {
    pub a: u32,
    pub b: u32,
    pub band: u32,
    pub rows: u32,
    pub double: bool,
}

pub(super) fn drawn(out: &str) -> Option<Drawn> {
    let l = out.lines().find(|l| l.starts_with("drawn "))?;
    let f = |k: &str| l.split_whitespace().find_map(|w| w.strip_prefix(k));
    let (band, rows) = f("band=")?.split_once('+')?;
    Some(Drawn { a: f("a=")?.parse().ok()?, b: f("b=")?.parse().ok()?, band: band.parse().ok()?, rows: rows.parse().ok()?, double: f("double=")? == "1" })
}

/// Compares a picture with what `abi-fbdev draw` drew: the first differing pixel, if any.
pub(super) fn check_picture(d: &Drawn, w: u32, h: u32, px: &[u32]) -> TestResult {
    kassert!(px.len() == (w * h) as usize, "picture {} pixels, screen {w}x{h}", px.len());
    for y in 0..h {
        let seed = if y >= d.band && y < d.band + d.rows { d.b } else { d.a };
        for x in 0..w {
            let want = pat(seed, x, y);
            let got = px[(y * w + x) as usize];
            if got != want {
                return Err(format!("pixel ({x}, {y}) is {got:#08x}, the program drew {want:#08x} (seed {seed}; {d:?})"));
            }
        }
    }
    Ok(())
}

/// Kernel memory standing in for a program's (one buffer at a fixed fake address; everything else faults).
struct FakeIo {
    buf: RefCell<Vec<u8>>,
}

impl FakeIo {
    const BASE: u64 = 0x7000_0000;
    fn new() -> FakeIo {
        FakeIo { buf: RefCell::new(alloc::vec![0; 4096]) }
    }
    fn put(&self, b: &[u8]) {
        self.buf.borrow_mut()[..b.len()].copy_from_slice(b);
    }
    fn span(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let o = addr.checked_sub(Self::BASE)? as usize;
        (o.checked_add(n)? <= self.buf.borrow().len()).then_some(o..o + n)
    }
}

impl UserIo for FakeIo {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        let Some(r) = self.span(addr, out.len()) else { return false };
        out.copy_from_slice(&self.buf.borrow()[r]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(r) = self.span(addr, data.len()) else { return false };
        self.buf.borrow_mut()[r].copy_from_slice(data);
        true
    }
}

fn get_var(fb: &Fb, io: &FakeIo) -> Result<Var, String> {
    let b = fb.ioctl(abi::FBIOGET_VSCREENINFO, 0, io, None).map_err(|e| format!("GET_V errno {e}"))?;
    Ok(Var::from_bytes(b.as_slice().try_into().map_err(|_| "GET_V size")?))
}

/// The node, its owner, and what GET_V/GET_F say about the live mode.
fn node() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    kassert!(fix_node_owner(), "{NODE} missing");
    let d = vfs::root_ns().lookup(NODE).map_err(|e| format!("{NODE}: errno {e}"))?;
    let mode = d.inode.meta.mode();
    kassert!(mode & uapi::S_IFMT == uapi::S_IFCHR && mode & 0o777 == 0o660, "mode {mode:o}");
    kassert!(d.inode.meta.uid() == NODE_UID, "owner {} (want {NODE_UID}:{NODE_GID})", d.inode.meta.uid());
    let f = vfs::node::file_of(&d);
    kassert!(f.rdev() == vfs::abi::encode_dev(abi::FB_MAJOR, 0), "rdev {:#x}", f.rdev());
    let m = display::device().ok_or("no display")?.current_mode();
    let g = fb.geometry();
    kassert!((g.width, g.height) == (m.width, m.height), "fbdev {}x{}, screen {}x{}", g.width, g.height, m.width, m.height);
    kassert!(g.rows == g.height || g.rows == 2 * g.height, "rows {}", g.rows);
    kassert!(g.smem_len as u64 >= g.line_length() as u64 * g.rows as u64 && g.smem_len % 4096 == 0, "smem_len {}", g.smem_len);
    let io = FakeIo::new();
    let v = get_var(fb, &io)?;
    kassert!(v.0[abi::XRES] == m.width && v.0[abi::YRES] == m.height && v.0[abi::BITS_PER_PIXEL] == 32, "var {:?}", &v.0[..8]);
    kassert!(v.0[abi::YRES_VIRTUAL] == g.rows, "yres_virtual {}", v.0[abi::YRES_VIRTUAL]);
    let f = fb.ioctl(abi::FBIOGET_FSCREENINFO, 0, &io, None).map_err(|e| format!("GET_F errno {e}"))?;
    kassert!(f == g.fix_bytes().to_vec(), "fix differs");
    klog::event!(Info, "dp", "ktest.fbdev_node", width = g.width, height = g.height, rows = g.rows, smem_len = g.smem_len);
    Ok(())
}
ktest!("dp.fbdev.node", Safe, node);

/// The ioctl rules through the device object (the host tests cover them in depth): refusals change nothing,
/// a pan moves the window, the argument is read and written through the caller's memory.
fn rules() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    let io = FakeIo::new();
    let a = FakeIo::BASE;
    let cur = get_var(fb, &io)?;
    kassert!(fb.ioctl(abi::FBIOPUT_VSCREENINFO, a + 4000, &io, None) == Err(uapi::errno::EFAULT), "PUT_V past the fake memory");
    let mut v = cur;
    v.0[abi::XRES] -= 8;
    io.put(&v.to_bytes());
    kassert!(fb.ioctl(abi::FBIOPUT_VSCREENINFO, a, &io, None) == Err(uapi::errno::EINVAL), "a smaller mode");
    io.put(&cur.to_bytes());
    kassert!(fb.ioctl(abi::FBIOPUT_VSCREENINFO, a, &io, None) == Ok(cur.to_bytes().to_vec()), "the same var");
    if cur.0[abi::YRES_VIRTUAL] == 2 * cur.0[abi::YRES] {
        let mut p = cur;
        let other = if cur.0[abi::YOFFSET] == 0 { cur.0[abi::YRES] } else { 0 };
        p.0[abi::YOFFSET] = other;
        io.put(&p.to_bytes());
        kassert!(fb.ioctl(abi::FBIOPAN_DISPLAY, a, &io, None).is_ok(), "pan to the other buffer");
        let now = get_var(fb, &io)?;
        kassert!(now.0[abi::YOFFSET] == other, "yoffset {}", now.0[abi::YOFFSET]);
        p.0[abi::YOFFSET] = cur.0[abi::YOFFSET];
        io.put(&p.to_bytes());
        kassert!(fb.ioctl(abi::FBIOPAN_DISPLAY, a, &io, None).is_ok(), "pan back");
    }
    let mut p = cur;
    p.0[abi::YOFFSET] = cur.0[abi::YRES_VIRTUAL];
    io.put(&p.to_bytes());
    kassert!(fb.ioctl(abi::FBIOPAN_DISPLAY, a, &io, None) == Err(uapi::errno::EINVAL), "pan past the buffer");
    kassert!(get_var(fb, &io)? == cur, "refusals and the pans back left the var changed");
    kassert!(fb.ioctl(abi::FBIOBLANK, 5, &io, None) == Err(uapi::errno::EINVAL), "blank level 5");
    kassert!(fb.ioctl(0x46ff, 0, &io, None) == Err(uapi::errno::ENOTTY), "unknown ioctl");
    // Hostile arguments through the caller's memory: a colour map header or channel outside it.
    kassert!(fb.ioctl(abi::FBIOGETCMAP, a + 4090, &io, None) == Err(uapi::errno::EFAULT), "GETCMAP header past the fake memory");
    kassert!(fb.ioctl(abi::FBIOPUTCMAP, 0, &io, None) == Err(uapi::errno::EFAULT), "PUTCMAP header at 0");
    let mut hdr = [0u8; abi::CMAP_USER_SIZE];
    hdr[4..8].copy_from_slice(&2u32.to_le_bytes());
    for (i, ch) in [a + 1024, a + 1056, 0xffff_8000_0000_0000u64].iter().enumerate() {
        hdr[8 + 8 * i..16 + 8 * i].copy_from_slice(&ch.to_le_bytes());
    }
    io.put(&hdr);
    kassert!(fb.ioctl(abi::FBIOPUTCMAP, a, &io, None) == Err(uapi::errno::EFAULT), "PUTCMAP blue at a kernel address");
    kassert!(fb.ioctl(abi::FBIOGETCMAP, a, &io, None) == Err(uapi::errno::EFAULT), "GETCMAP blue at a kernel address");
    let t0 = time::now_ns();
    kassert!(fb.ioctl(abi::FBIO_WAITFORVSYNC, u64::MAX, &io, None) == Ok(Vec::new()), "wait for vsync (the argument is ignored)");
    // Functional only: a hang shows here; the time of one vblank wait is judged on the Mini-PC (check list 3).
    let waited = time::now_ns() - t0;
    kassert!(waited < SEC, "waited {} ms for one vblank", waited / MS);
    klog::event!(Info, "dp", "ktest.fbdev_vsync", waited_us = waited / 1000);
    Ok(())
}
ktest!("dp.fbdev.rules", Safe, rules);

/// `abi-fbdev v2`: every fbdev call from a program, compared with the Linux answers (expected/dp2-fbdev.txt).
fn program() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let s = start(&["abi-fbdev", "v2"])?;
    let code = finish(&s, 120 * SEC)?;
    let got = text(&s.out);
    let want = include_str!("../../../../../user/abi-tests/expected/dp2-fbdev.txt");
    if got != want {
        let diff: Vec<String> = want
            .lines()
            .zip(got.lines().chain(core::iter::repeat("<missing>")))
            .enumerate()
            .filter(|(_, (w, g))| w != g)
            .take(6)
            .map(|(i, (w, g))| format!("line {}: want '{w}', got '{g}'", i + 1))
            .collect();
        return Err(format!("abi-fbdev v2 differs from Linux's answers:\n{}\nexit {code}, stderr: {}\nfull output:\n{got}", diff.join("\n"), text(&s.err)));
    }
    kassert!(code == 0, "exit {code}");
    handed_back(fb, "abi-fbdev v2 ended")?;
    Ok(())
}
ktest!("dp.fbdev.program", Safe, program);

/// An fbDOOM-style program: double-buffered frames with pans and vsync waits, then stores straight into the
/// shown buffer and a pwrite band. While it holds the screen, MCP `screen`'s picture (display::snapshot) is
/// exactly what it drew; when it is killed the console comes back.
fn picture() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let before = stats();
    let presents = display::stats().presents;
    let s = start(&["abi-fbdev", "draw", "5", "4", "hold"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(60 * SEC, || drawn(&text(&s.out)).is_some() || s.p.has_exited()), "no 'drawn' line:\n{}", text(&s.out));
        let d = drawn(&text(&s.out)).ok_or_else(|| format!("ended early: {:?}\n{}{}", s.p.status(), text(&s.out), text(&s.err)))?;
        kassert!(display::scanout_holder() == Some(("fbdev", s.p.pid)), "screen holder {:?}, program pid {}", display::scanout_holder(), s.p.pid);
        kassert!(fb.holder_pid() == s.p.pid, "fbdev holder {}", fb.holder_pid());
        let (w, h, px) = display::snapshot().ok_or("no snapshot")?;
        check_picture(&d, w, h, &px)?;
        let st = stats();
        kassert!(st.takes > before.takes, "no take counted");
        kassert!(st.write_faults > before.write_faults, "no write fault: the stores were not tracked");
        kassert!(st.flushes > before.flushes, "no deferred-io flush");
        if d.double {
            kassert!(st.refreshes >= before.refreshes + 4, "{} window copies for 4 pans", st.refreshes - before.refreshes);
        }
        klog::event!(Info, "dp", "ktest.fbdev_picture", width = w, height = h, double = d.double, write_faults = st.write_faults - before.write_faults,
            flushes = st.flushes - before.flushes, flushed_rows = st.flushed_rows - before.flushed_rows, refreshes = st.refreshes - before.refreshes);
        Ok(())
    })();
    let closes = stats().wakes_close;
    s.p.kill(uapi::ab::signal::SIGKILL);
    let ended = s.p.wait(Some(10 * SEC)).is_some();
    r?;
    kassert!(ended, "the program did not end after SIGKILL");
    handed_back(fb, "the kill")?;
    kassert!(stats().wakes_close > closes, "the program's exit closed fb0 without waking the fbdev thread");
    kassert!(display::stats().presents > presents, "nothing presented");
    let (w, h, _) = display::snapshot().ok_or("no console snapshot")?;
    let m = display::device().ok_or("no display")?.current_mode();
    kassert!((w, h) == (m.width, m.height), "console snapshot {w}x{h}");
    Ok(())
}
ktest!("dp.fbdev.picture", Safe, picture);

/// A program that crashes while it holds the screen: the console comes back by itself.
fn crash() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let s = start(&["abi-fbdev", "draw", "9", "1", "crash"])?;
    let code = finish(&s, 60 * SEC)?;
    kassert!(drawn(&text(&s.out)).is_some(), "no 'drawn' line:\n{}{}", text(&s.out), text(&s.err));
    kassert!(code != 0, "the crash ended with {code}");
    handed_back(fb, "the crash")?;
    klog::event!(Info, "dp", "ktest.fbdev_crash", code = code, message = s.p.end_message().unwrap_or_default().as_str());
    Ok(())
}
ktest!("dp.fbdev.crash", Safe, crash);

/// The console asks for the screen back (KD_TEXT's path, `display::request_scanout_back`): fbdev gives it while
/// the program runs on; a kernel owner holding the screen keeps a program's picture off it.
fn handback() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let s = start(&["abi-fbdev", "draw", "11", "1", "hold"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(60 * SEC, || drawn(&text(&s.out)).is_some() || s.p.has_exited()), "no 'drawn' line:\n{}", text(&s.out));
        kassert!(fb.holder_pid() == s.p.pid, "fbdev holder {}", fb.holder_pid());
        let backs = stats().wakes_back;
        display::request_scanout_back();
        kassert!(stats().wakes_back > backs, "request_scanout_back did not wake the fbdev thread");
        handed_back(fb, "request_scanout_back")?;
        kassert!(!s.p.has_exited(), "the program ended: {:?}", s.p.status());
        Ok(())
    })();
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    r?;
    // A kernel owner has the screen: the program draws unseen and the owner keeps it.
    let g = display::take_scanout("dp.fbdev.handback").map_err(|e| format!("{e:?}"))?;
    let s = start(&["abi-fbdev", "draw", "12", "1"]);
    let code = s.as_ref().map_err(|e| e.clone()).and_then(|s| finish(s, 60 * SEC));
    let held = display::scanout_holder();
    drop(g);
    let s = s?;
    let code = code?;
    kassert!(code == 0, "exit {code}: {}{}", text(&s.out), text(&s.err));
    kassert!(held == Some(("dp.fbdev.handback", 0)), "holder while the kernel owned it: {held:?}");
    handed_back(fb, "both programs ended")?;
    Ok(())
}
ktest!("dp.fbdev.handback", Safe, handback);

/// DP-2f: every open of /dev/fb0 is its own object whose close wakes the `fbdev` thread. A close of an open that
/// does not hold the screen leaves the holder alone (the thread looks, sees it alive); a close with nobody
/// holding the screen is harmless (no hand-back, the console keeps it).
fn opens() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let ns = vfs::root_ns();
    let open = || ns.open_flags(b"/dev/fb0", 2, 0).map_err(|e| format!("open /dev/fb0: errno {e}"));
    // Closes `f`: the thread is woken and has run a pass afterwards.
    let close = |f: Arc<vfs::OpenFile>, what: &str| -> TestResult {
        let closes = stats().wakes_close;
        drop(f);
        let passes = stats().passes;
        kassert!(stats().wakes_close > closes, "closing {what} did not wake the fbdev thread");
        kassert!(wait_for(5 * SEC, || stats().passes > passes), "no fbdev pass within 5 s after closing {what}");
        Ok(())
    };
    let s = start(&["abi-fbdev", "draw", "13", "1", "hold"])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(60 * SEC, || drawn(&text(&s.out)).is_some() || s.p.has_exited()), "no 'drawn' line:\n{}", text(&s.out));
        kassert!(fb.holder_pid() == s.p.pid, "fbdev holder {}", fb.holder_pid());
        let at = super::handback::last().at_ns;
        let (a, b) = (open()?, open()?);
        for (f, what) in [(a, "a second open"), (b, "a third open")] {
            close(f, what)?;
            kassert!(fb.holder_pid() == s.p.pid && display::scanout_holder() == Some(("fbdev", s.p.pid)),
                "{what} closed: holder {} / {:?}, program pid {}", fb.holder_pid(), display::scanout_holder(), s.p.pid);
            kassert!(super::handback::last().at_ns == at, "{what} closed: the screen was handed back");
        }
        Ok(())
    })();
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    r?;
    handed_back(fb, "the kill")?;
    let at = super::handback::last().at_ns;
    close(open()?, "an open with nobody holding the screen")?;
    kassert!(console_back(fb, 0), "holder {} / {:?} after a close without a take", fb.holder_pid(), display::scanout_holder());
    kassert!(super::handback::last().at_ns == at, "a close without a take recorded a hand-back");
    Ok(())
}
ktest!("dp.fbdev.opens", Safe, opens);
