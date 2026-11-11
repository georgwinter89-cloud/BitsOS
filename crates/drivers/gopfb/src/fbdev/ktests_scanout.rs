//! In-kernel tests of WY-17 (`dp.fbdev.scanout`): FBIO_BITSOS_SCANOUT without a display engine, the rules a program
//! cannot provoke on itself and what a window is. Each exported fd is a dma-buf over fb0's RAM whose bytes are fb0's,
//! window 1 at its offset; BEGIN names the window not shown; a pan with damage reports stays what it was; END puts the
//! view on window 0; the program's death ends it all and the dma-bufs with it.
//!
//! `dp.fbdev.scanout_engine`: the same ioctls through `Fb` with a fake engine in place of amdgpu's (QEMU has none): the
//! windows are the scanout buffers, a pan is a flip and nothing is copied, a refused pan changes nothing, MCP's look
//! does not need `fbdev.screen`, every way out puts the view home, and after a holder's death the next take shows the
//! console's picture, not what fb0's memory held. The real engine's half: `cargo test -p amdgpu` (the flip against
//! its fake registers) and the desk's Mini-PC list (plan/notes/wy-WY-17.md).

use super::abi::{self, UserIo};
use super::flip::{DirectBuffer, DirectFrame, FlipError, ScanoutFlip, Vblank};
use super::ktests::{fb, finish, handed_back, need_program, start, text, wait_for, Started, SEC};
use super::scanout::{self, Args, FBIO_BITSOS_SCANOUT, FLAG_ENGINE, FLAG_WHOLE, OP_BEGIN, OP_END, OP_EXPORT, SIZE, XRGB8888};
use super::{fix_node_owner, stats, Fb};
use alloc::format;
use alloc::string::String;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use devmodel::display::{DisplayDevice, Fence};
use devmodel::DevError;
use display::ScanoutSource;
use ktest::{kassert, ktest, TestResult};
use uapi::errno::{EBUSY, EFAULT, EINVAL, EIO, EPERM};

/// One argument at a fake address; any other address faults.
struct Arg(Cell<[u8; SIZE]>);
const AT: u64 = 0x7400_0000;

impl UserIo for Arg {
    fn read(&self, addr: u64, out: &mut [u8]) -> bool {
        if addr != AT || out.len() > SIZE {
            return false;
        }
        out.copy_from_slice(&self.0.get()[..out.len()]);
        true
    }
    fn write(&self, _: u64, _: &[u8]) -> bool {
        false
    }
}

fn raw(fb: &Fb, s: Option<&Started>, a: Args) -> Result<Args, i64> {
    let out = fb.ioctl(FBIO_BITSOS_SCANOUT, AT, &Arg(Cell::new(a.to_bytes())), s.map(|s| &s.p))?;
    let b: [u8; SIZE] = out.as_slice().try_into().map_err(|_| EINVAL)?;
    Ok(Args::from_bytes(&b))
}

fn call(fb: &Fb, s: Option<&Started>, op: u32, window: u32) -> Result<Args, i64> {
    raw(fb, s, Args { op, window, fd: -1, ..Args::default() })
}

fn buffer(s: &Started, fd: i32) -> Result<alloc::sync::Arc<drm::dma_buf::DmaBuf>, String> {
    let f = s.p.fds.get(fd as usize).map_err(|e| format!("fd {fd}: {e}"))?;
    drm::dma_buf::as_dma_buf(&f.file).ok_or(format!("fd {fd} is no dma-buf"))
}

fn steps(fb: &Fb, s: &Started, other: &Started) -> TestResult {
    let g = fb.g();
    let (line, h) = (u64::from(g.line_length()), u64::from(g.height));
    kassert!(g.rows >= 2 * g.height, "fb0 has one window here: {} rows", g.rows);
    kassert!(call(fb, None, OP_EXPORT, 0) == Err(EPERM), "a kernel caller has no fd table");
    kassert!(call(fb, Some(s), 9, 0) == Err(EINVAL), "unknown op");
    kassert!(fb.ioctl(FBIO_BITSOS_SCANOUT, AT + 8, &Arg(Cell::new([0; SIZE])), Some(&s.p)) == Err(EFAULT), "bad address");

    // A program that only looks takes nothing from the console.
    kassert!(fb.holder_pid() == 0, "held at the start by {}", fb.holder_pid());
    kassert!(raw(fb, Some(s), Args { op: OP_EXPORT, flags: 4, ..Args::default() }) == Err(EINVAL), "an unknown flag bit");
    kassert!(call(fb, Some(s), OP_BEGIN, 0) == Err(EBUSY), "BEGIN with the screen not its own");
    kassert!(call(fb, Some(s), OP_END, 0) == Err(EBUSY), "END with the screen not its own");
    kassert!(fb.holder_pid() == 0, "a refused call took the screen for {}", fb.holder_pid());

    // The console has the screen: the program with fb0 open takes it, as for the other BitsOS ioctls.
    let a0 = call(fb, Some(s), OP_EXPORT, 0).map_err(|e| format!("export 0: {e}"))?;
    kassert!(fb.holder_pid() == s.p.pid, "the export took no screen (holder {})", fb.holder_pid());
    kassert!(call(fb, Some(other), OP_EXPORT, 0) == Err(EBUSY), "someone else's export while it is held");
    kassert!(call(fb, Some(other), OP_BEGIN, 0) == Err(EBUSY), "someone else's BEGIN while it is held");
    kassert!(call(fb, Some(s), OP_EXPORT, 2) == Err(EINVAL), "window 2");
    kassert!(call(fb, Some(s), OP_BEGIN, 0).is_ok(), "BEGIN after an export");
    let a1 = call(fb, Some(s), OP_EXPORT, 1).map_err(|e| format!("export 1: {e}"))?;
    let mem = fb.mem.len() as u64;
    for (a, offset) in [(a0, 0), (a1, line * h)] {
        kassert!(a.offset == offset && a.size == mem && a.fd >= 0, "window at {} of {} bytes, fd {}: {a:?}", a.offset, a.size, a.fd);
        kassert!(a.width == g.width && a.height == g.height && u64::from(a.stride) == line && a.fourcc == XRGB8888, "layout {a:?}");
        kassert!(a.flags & FLAG_ENGINE == 0, "an engine under this test: {a:?}");
    }

    // The windows' bytes are fb0's, in both directions.
    let b1 = buffer(s, a1.fd)?;
    kassert!(b1.exporter().write(a1.offset + 8, &[1, 2, 3, 4]), "a store through the dma-buf");
    let mut seen = [0u8; 4];
    kassert!(fb.mem.read((a1.offset + 8) as usize, &mut seen) && seen == [1, 2, 3, 4], "fb0 shows {seen:?}");
    kassert!(fb.mem.write((a1.offset + 16) as usize, &[5, 6, 7, 8]), "a store through fb0");
    kassert!(b1.exporter().read(a1.offset + 16, &mut seen) && seen == [5, 6, 7, 8], "the dma-buf shows {seen:?}");
    kassert!(b1.exporter().page_phys(mem / 4096 - 1).is_some(), "the last page has an address");
    kassert!(b1.exporter().page_phys(mem / 4096).is_none(), "a page past the end has one");
    kassert!(b1.device_map().is_some(), "programs cannot map it");

    // BEGIN names the window not shown, whichever that is, and without an engine the kernel draws nothing itself.
    let shown = fb.yoffset.load(core::sync::atomic::Ordering::Acquire) / g.height;
    let b = call(fb, Some(s), OP_BEGIN, 0).map_err(|e| format!("begin: {e}"))?;
    kassert!(b.window == shown ^ 1 && b.flags == 0, "BEGIN with window {shown} shown: {b:?}");
    kassert!(!fb.direct.drawing(), "RAM windows stay on the copy path");
    let mut var = fb.meta.lock().var;
    var.0[abi::YOFFSET] = b.window * g.height;
    let bytes = var.to_bytes();
    struct Pan([u8; abi::VAR_SIZE]);
    impl UserIo for Pan {
        fn read(&self, _: u64, out: &mut [u8]) -> bool {
            out.copy_from_slice(&self.0[..out.len()]);
            true
        }
        fn write(&self, _: u64, _: &[u8]) -> bool {
            false
        }
    }
    fb.ioctl(abi::FBIOPAN_DISPLAY, AT, &Pan(bytes), Some(&s.p)).map_err(|e| format!("pan: {e}"))?;
    let next = call(fb, Some(s), OP_BEGIN, 0).map_err(|e| format!("begin 2: {e}"))?;
    kassert!(next.window == b.window ^ 1, "after the pan to {}: {next:?}", b.window);

    // END: the view back on window 0, unseen.
    call(fb, Some(s), OP_END, 0).map_err(|e| format!("end: {e}"))?;
    kassert!(fb.yoffset.load(core::sync::atomic::Ordering::Acquire) == 0, "END left the view on {}", fb.yoffset.load(core::sync::atomic::Ordering::Acquire));
    Ok(())
}

fn rules() -> TestResult {
    if display::device().is_none() {
        return ktest::fail("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let live = drm::dma_buf::live();
    let s = start(&["abi-fbdev", "idle"])?;
    let other = start(&["abi-fbdev", "idle"]);
    let r = (|| -> TestResult {
        let other = other.as_ref().map_err(|e| e.clone())?;
        for p in [&s, other] {
            kassert!(wait_for(30 * SEC, || text(&p.out).contains("idle pid=") || p.p.has_exited()), "no 'idle' line: {}{}", text(&p.out), text(&p.err));
        }
        steps(fb, &s, other)
    })();
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    if let Ok(o) = &other {
        o.p.kill(uapi::ab::signal::SIGKILL);
        let _ = o.p.wait(Some(10 * SEC));
    }
    r?;
    handed_back(fb, "the kill")?;
    kassert!(!fb.direct.drawing(), "the direct state outlived the hold");
    drop(s);
    kassert!(wait_for(5 * SEC, || drm::dma_buf::live() <= live), "dma-bufs alive: {} before, {} after the program ended", live, drm::dma_buf::live());
    Ok(())
}
// The fb's own RAM as the windows, so no display engine: on the Mini-PC `dp.fbdev.scanout_engine` is the same test.
ktest!("dp.fbdev.scanout", QemuOnly, rules);

/// The fake engine: two buffers at the framebuffer's real address (a program maps them, the kernel never touches them),
/// counting what it is asked.
struct Engine {
    phys: AtomicU64,
    len: AtomicU64,
    stride: AtomicU32,
    shown: AtomicU32,
    ordered: AtomicU64,
    flips: AtomicU64,
    waits: AtomicU64,
    stopped: AtomicBool,
    whole: AtomicBool,
}

static ENGINE: Engine = Engine {
    phys: AtomicU64::new(0),
    len: AtomicU64::new(0),
    stride: AtomicU32::new(0),
    shown: AtomicU32::new(0),
    ordered: AtomicU64::new(0),
    flips: AtomicU64::new(0),
    waits: AtomicU64::new(0),
    stopped: AtomicBool::new(false),
    whole: AtomicBool::new(true),
};

/// What `read_shown` hands out for buffer `w`: not a picture anything else could have drawn.
const fn look_px(w: u32) -> u32 {
    0x00A0_B0C0 + w
}
/// What the test stores into fb0's memory: a next take must not show it.
const POISON: u32 = 0x0012_3457;

impl ScanoutFlip for Engine {
    fn ready(&self) -> bool {
        true
    }
    fn active(&self) -> bool {
        !self.stopped.load(Ordering::Acquire)
    }
    fn present_shown(&self, _: &[u32], _: u32, _: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        Err(DevError::Unsupported)
    }
    fn begin(&self, _: &[u32], _: u32) -> Result<&dyn DisplayDevice, FlipError> {
        Err(FlipError::Off)
    }
    fn commit(&self) -> Result<u64, FlipError> {
        Err(FlipError::Off)
    }
    fn wait(&self, _: u64) -> Result<(), FlipError> {
        self.waits.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn vblank(&self) -> Option<Vblank> {
        None
    }
    fn rearm(&self) {}
    fn direct_buffer(&self, _window: usize) -> Option<DirectBuffer> {
        Some(DirectBuffer { phys: self.phys.load(Ordering::Acquire), len: self.len.load(Ordering::Acquire), stride: self.stride.load(Ordering::Acquire) })
    }
    fn direct_begin(&self) -> Result<DirectFrame, FlipError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(FlipError::Off);
        }
        Ok(DirectFrame { window: (self.shown.load(Ordering::Acquire) ^ 1) as usize, whole: self.whole.swap(false, Ordering::AcqRel) })
    }
    fn direct_flip(&self, window: usize) -> Result<u64, FlipError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(FlipError::Late);
        }
        if self.shown.swap(window as u32, Ordering::AcqRel) == window as u32 {
            return Ok(self.ordered.load(Ordering::Acquire));
        }
        self.flips.fetch_add(1, Ordering::AcqRel);
        Ok(self.ordered.fetch_add(1, Ordering::AcqRel) + 1)
    }
    fn read_shown(&self, dst: &mut [u32], _stride: u32) -> bool {
        dst.fill(look_px(self.shown.load(Ordering::Acquire)));
        true
    }
}

fn pan_to(fb: &Fb, s: &Started, yoffset: u32) -> Result<(), i64> {
    struct Pan([u8; abi::VAR_SIZE]);
    impl UserIo for Pan {
        fn read(&self, _: u64, out: &mut [u8]) -> bool {
            out.copy_from_slice(&self.0[..out.len()]);
            true
        }
        fn write(&self, _: u64, _: &[u8]) -> bool {
            false
        }
    }
    let mut var = fb.meta.lock().var;
    var.0[abi::YOFFSET] = yoffset;
    fb.ioctl(abi::FBIOPAN_DISPLAY, AT, &Pan(var.to_bytes()), Some(&s.p)).map(|_| ())
}

/// The view as the var and the atomic say; they must agree.
fn view(fb: &Fb) -> Result<u32, String> {
    let (var, top) = (fb.meta.lock().var.0[abi::YOFFSET], fb.yoffset.load(Ordering::Acquire));
    kassert!(var == top, "the var says {var}, the view {top}");
    Ok(top)
}

/// EXPORT both windows and BEGIN: frames go straight to the scanout from here.
fn draw_direct(fb: &Fb, s: &Started) -> Result<Args, String> {
    let a = call(fb, Some(s), OP_EXPORT, 0).map_err(|e| format!("export 0: {e}"))?;
    call(fb, Some(s), OP_EXPORT, 1).map_err(|e| format!("export 1: {e}"))?;
    let b = call(fb, Some(s), OP_BEGIN, 0).map_err(|e| format!("begin: {e}"))?;
    kassert!(fb.direct.drawing(), "BEGIN on the engine started nothing");
    kassert!(b.flags & FLAG_ENGINE != 0, "BEGIN on the engine: {b:?}");
    Ok(a)
}

fn engine_steps(fb: &Fb, s: &Started, other: &Started) -> TestResult {
    let g = fb.g();
    let h = g.height;
    let line = u64::from(g.line_length());
    kassert!(g.rows >= 2 * h, "fb0 has one window here: {} rows", g.rows);
    let e = &ENGINE;

    // Holding the screen is not having exported.
    kassert!(fb.take_for(s.p.pid), "the take");
    kassert!(call(fb, Some(s), OP_BEGIN, 0) == Err(EINVAL), "BEGIN with nothing exported");
    kassert!(call(fb, Some(s), OP_END, 0) == Err(EINVAL), "END with nothing exported");

    // A window is the scanout buffer; what stands in the out fields on the way in does not count.
    let junk = Args { op: OP_EXPORT, window: 1, fd: 77, flags: 0, width: 1, height: 2, stride: 3, fourcc: 4, offset: 5, size: 6 };
    let a = raw(fb, Some(s), junk).map_err(|e| format!("export: {e}"))?;
    let want = (e.len.load(Ordering::Acquire) + 4095) & !4095;
    kassert!(a.flags == FLAG_ENGINE && a.size == want && a.offset == 0 && a.fd >= 0, "{a:?}, {want} bytes");
    kassert!(a.width == g.width && a.height == h && u64::from(a.stride) == line && a.fourcc == XRGB8888, "layout {a:?}");
    let b = buffer(s, a.fd)?;
    kassert!(b.exporter().name() == "bitsos-scanout", "exporter {}", b.exporter().name());
    kassert!(b.device_map().is_some(), "programs cannot map it");
    kassert!(!b.exporter().read(0, &mut [0u8; 4]), "the kernel reads through a scanout window");
    kassert!(call(fb, Some(s), OP_END, 0).is_ok(), "END without BEGIN");
    kassert!(!fb.direct.drawing(), "END left it drawing");

    // The pan is a flip; nothing is copied.
    draw_direct(fb, s)?;
    let f = call(fb, Some(s), OP_BEGIN, 0).map_err(|e| format!("begin: {e}"))?;
    kassert!(f.window == 1 && f.flags == FLAG_ENGINE, "the second BEGIN: {f:?}");
    let (flips, before) = (e.flips.load(Ordering::Acquire), stats());

    // A refused pan changes nothing: off the grid, past the room, by someone else.
    kassert!(pan_to(fb, s, h / 2) == Err(EINVAL), "a pan off the window grid");
    kassert!(view(fb)? == 0, "a refused pan moved the view");
    kassert!(pan_to(fb, s, 2 * h) == Err(EINVAL), "a pan past the windows");
    kassert!(pan_to(fb, other, h) == Err(EBUSY), "someone else's pan");
    kassert!(view(fb)? == 0 && e.flips.load(Ordering::Acquire) == flips, "a refused pan flipped");

    kassert!(pan_to(fb, s, h).is_ok(), "the pan to window 1");
    kassert!(e.flips.load(Ordering::Acquire) == flips + 1 && e.shown.load(Ordering::Acquire) == 1, "the flip");
    kassert!(view(fb)? == h, "the view after the flip");
    // Twice to the buffer shown: no flip, no hang.
    kassert!(pan_to(fb, s, h).is_ok() && pan_to(fb, s, h).is_ok(), "pans to the window shown");
    kassert!(e.flips.load(Ordering::Acquire) == flips + 1, "a pan to the window shown flipped");
    let after = stats();
    kassert!(after.refreshes == before.refreshes && after.flushed_rows == before.flushed_rows, "a pan copied: {} refreshes, {} rows", after.refreshes - before.refreshes, after.flushed_rows - before.flushed_rows);
    let f = call(fb, Some(s), OP_BEGIN, 0).map_err(|e| format!("begin: {e}"))?;
    kassert!(f.window == 0 && f.flags & FLAG_WHOLE == 0, "the hidden window after the flip: {f:?}");

    // MCP's look: what shows, without `fbdev.screen` (the compositor's pan needs that lock).
    {
        let _scr = fb.screen.lock();
        let (mut ok, mut size) = (false, (0, 0));
        let looked = fb.with_picture(&mut |px, w, h| {
            size = (w, h);
            ok = px.len() == (w * h) as usize && px.iter().all(|&p| p == look_px(1));
        });
        kassert!(looked && ok && size == (g.width, h), "the look under a held lock: {looked}, {ok}, {size:?}");
    }

    // A flip that fails ends it: EIO, view home; BEGIN says so until END, and then there is nothing exported.
    e.stopped.store(true, Ordering::Release);
    kassert!(pan_to(fb, s, 0) == Err(EIO), "a failed flip");
    kassert!(!fb.direct.drawing() && view(fb)? == 0, "after a failed flip: view {}", view(fb)?);
    kassert!(call(fb, Some(s), OP_BEGIN, 0) == Err(EIO), "BEGIN after the engine stopped");
    kassert!(call(fb, Some(s), OP_END, 0).is_ok(), "END after the engine stopped");
    kassert!(call(fb, Some(s), OP_BEGIN, 0) == Err(EINVAL), "BEGIN after END");
    e.stopped.store(false, Ordering::Release);

    // The same at BEGIN.
    e.shown.store(0, Ordering::Release);
    draw_direct(fb, s)?;
    kassert!(pan_to(fb, s, h).is_ok(), "the pan");
    e.stopped.store(true, Ordering::Release);
    kassert!(call(fb, Some(s), OP_BEGIN, 0) == Err(EIO), "BEGIN with the engine stopped");
    kassert!(!fb.direct.drawing() && view(fb)? == 0, "after a failed BEGIN: view {}", view(fb)?);
    call(fb, Some(s), OP_END, 0).map_err(|e| format!("end: {e}"))?;
    e.stopped.store(false, Ordering::Release);

    // END: the view home, drawing off.
    e.shown.store(0, Ordering::Release);
    draw_direct(fb, s)?;
    kassert!(pan_to(fb, s, h).is_ok(), "the pan");
    call(fb, Some(s), OP_END, 0).map_err(|e| format!("end: {e}"))?;
    kassert!(!fb.direct.drawing() && view(fb)? == 0, "after END: view {}", view(fb)?);

    // The holder dies with window 1 shown, whose RAM nobody ever wrote. The next take shows the console's picture.
    e.shown.store(0, Ordering::Release);
    draw_direct(fb, s)?;
    kassert!(pan_to(fb, s, h).is_ok(), "the pan");
    let poison: alloc::vec::Vec<u8> = POISON.to_le_bytes().iter().copied().cycle().take((2 * u64::from(h) * line) as usize).collect();
    kassert!(fb.mem.write(0, &poison), "the poison");
    s.p.kill(uapi::ab::signal::SIGKILL);
    let _ = s.p.wait(Some(10 * SEC));
    handed_back(fb, "the holder's kill")?;
    kassert!(!fb.direct.drawing() && view(fb)? == 0, "after the kill: view {}", view(fb)?);
    kassert!(fb.fresh.load(Ordering::Acquire), "the next take would show fb0's memory");
    kassert!(fb.take_for(other.p.pid), "the next take");
    let mut bad = 0;
    kassert!(fb.with_picture(&mut |px, _, _| bad = px.iter().filter(|&&p| p == POISON).count()), "no picture");
    kassert!(bad == 0, "{bad} pixels of fb0's old memory on the screen");
    Ok(())
}

fn with_engine() -> TestResult {
    if display::device().is_none() {
        return ktest::fail("no display (headless)");
    }
    let fb = fb()?;
    need_program()?;
    fix_node_owner();
    let Some((_, target)) = crate::live() else {
        return ktest::fail("no live framebuffer");
    };
    let Some((_, phys)) = target.window() else {
        return ktest::fail("the framebuffer is no mapped window");
    };
    let e = &ENGINE;
    e.phys.store(phys, Ordering::Release);
    e.len.store(target.len() as u64, Ordering::Release);
    e.stride.store(fb.g().line_length(), Ordering::Release);
    e.shown.store(0, Ordering::Release);
    e.stopped.store(false, Ordering::Release);
    e.whole.store(true, Ordering::Release);
    kassert!(fb.holder_pid() == 0, "held at the start by {}", fb.holder_pid());
    scanout::stand_in_for_test(e, true);
    let r = (|| -> TestResult {
        // A window as a program's file: Linux's answers for a dma-buf, on device memory.
        let p = start(&["abi-fbdev", "scanout-file"])?;
        finish(&p, 30 * SEC)?;
        let out = text(&p.out);
        let want = format!("scanout-file flags={FLAG_ENGINE} end=1 cur=22 mapped=1 stored=1 beyond=22 sync=0 unknown=25\n");
        kassert!(out == want, "the file: {out} {}", text(&p.err));
        handed_back(fb, "the file's program")?;

        let s = start(&["abi-fbdev", "idle"])?;
        let other = start(&["abi-fbdev", "idle"]);
        let r = (|| -> TestResult {
            let other = other.as_ref().map_err(|e| e.clone())?;
            for p in [&s, other] {
                kassert!(wait_for(30 * SEC, || text(&p.out).contains("idle pid=") || p.p.has_exited()), "no 'idle' line: {}{}", text(&p.out), text(&p.err));
            }
            engine_steps(fb, &s, other)
        })();
        s.p.kill(uapi::ab::signal::SIGKILL);
        let _ = s.p.wait(Some(10 * SEC));
        if let Ok(o) = &other {
            o.p.kill(uapi::ab::signal::SIGKILL);
            let _ = o.p.wait(Some(10 * SEC));
        }
        r?;
        handed_back(fb, "the kill")
    })();
    scanout::stand_in_for_test(e, false);
    r
}
ktest!("dp.fbdev.scanout_engine", Screen, with_engine);
