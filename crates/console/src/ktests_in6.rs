//! IN-6 ktests (B4 latency path): event times from the one clock never go back, a pointer frame is measured,
//! and the pointer moves over a fullscreen program that never presents (a kernel scanout owner with its own
//! picture stands in for the hanging fbdev program). Functional only: timing is judged on the Mini-PC.

use crate::evdev::{self, Caps, DeviceInfo, EV_REL, EV_SYN, EvdevSink, InputEvent, SYN_REPORT, Source};
use crate::latency::{self, Path};
use crate::render::{ARROW_W, POINTER_FILL, POINTER_OUTLINE};
use crate::{pointer_over, vt};
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use display::ScanoutSource;
use ktest::{TestResult, kassert, ktest};

const WAIT_NS: u64 = 5_000_000_000;
/// REL_WHEEL: the VT ignores it, so these events move nothing on the screen.
const REL_WHEEL: u16 = 8;

fn fake_mouse(name: &str, rel: &[u16]) -> Arc<evdev::InputDevice> {
    fake_mouse_on(evdev::sink(), name, rel)
}

fn fake_mouse_on(sink: &'static EvdevSink, name: &str, rel: &[u16]) -> Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    caps.keys.insert(0x110);
    caps.rel.extend(rel.iter().copied());
    evdev::register(sink, DeviceInfo { name: name.to_string(), bus: evdev::BUS_VIRTUAL, caps, ..DeviceInfo::default() })
}

/// Reports from several CPUs at once (each stamped before it takes the device's lock, so the stamps race): a
/// reader sees non-decreasing times, none in the future, none before the test began. IN-6b: on a sink of its
/// own, so its clamps (wanted here) stay out of the machine's `input.time_clamped` numbers.
fn timestamps() -> TestResult {
    static S: EvdevSink = EvdevSink::new();
    while S.pop().is_some() {}
    let sink = &S;
    let m = fake_mouse_on(sink, "ktest latency wheel", &[REL_WHEEL]);
    let c = sink.subscribe("ktest.latency");
    let clamped_before = sink.clamped();
    let begin = time::now_ns();
    let r = (|| -> TestResult {
        let threads = sched::online_cpus().clamp(2, 8);
        let mut hs = Vec::new();
        for cpu in 0..threads {
            let m = m.clone();
            let opts = sched::SpawnOpts { cpu: Some(cpu % sched::online_cpus()), ..sched::SpawnOpts::default() };
            let h = sched::spawn_with("ktest-latency-emit", sched::Class::Normal, opts, move || {
                for i in 0..50 {
                    let v = if i % 2 == 0 { 1 } else { -1 };
                    m.emit(time::now_ns(), &[(EV_REL, REL_WHEEL, v), (EV_SYN, SYN_REPORT, 0)]);
                }
            })
            .map_err(|e| alloc::format!("spawn: {e:?}"))?;
            hs.push(h);
        }
        for h in &hs {
            kassert!(h.join_timeout(WAIT_NS), "an emitter did not finish");
        }
        // One stamped in the past on purpose (a report stamped on a CPU that was then preempted).
        m.emit(begin, &[(EV_REL, REL_WHEEL, 1), (EV_SYN, SYN_REPORT, 0)]);
        let end = time::now_ns();
        let mut last = 0;
        let mut n = 0;
        while let Some(ev) = c.pop() {
            if ev.device != m.number {
                continue;
            }
            kassert!(ev.time_ns >= last, "event {n} at {} after one at {last}", ev.time_ns);
            kassert!(ev.time_ns >= begin && ev.time_ns <= end, "event {n} at {} outside [{begin}, {end}]", ev.time_ns);
            last = ev.time_ns;
            n += 1;
        }
        kassert!(n >= 2, "only {n} events arrived");
        kassert!(sink.clamped() > clamped_before, "the report stamped in the past was not counted");
        Ok(())
    })();
    sink.unsubscribe(&c);
    m.unregister(time::now_ns());
    while S.pop().is_some() {}
    r
}
ktest!("in.latency.timestamps", Safe, timestamps);

const SOURCES: [Source; 5] = [Source::Usb, Source::Device, Source::Inject, Source::Write, Source::Reader];

fn clamp_counts() -> [u64; 5] {
    SOURCES.map(|s| s.clamped())
}

/// IN-6b: a normal run clamps nothing. On the machine's sink, from one thread: reports of a device stamped
/// just before they go in, injected packets (as MCP sends them, with a stale caller time the sink replaces)
/// between them. No `input.time_clamped.*` count moves, the reader's times never go back, each injected packet
/// has one time from inside the round. Real input arriving meanwhile can make one round clamp (a report
/// stamped at its interrupt after ours): up to three rounds, one clean round passes.
fn normal_run_clamps_zero() -> TestResult {
    let m = fake_mouse("ktest latency clamp", &[REL_WHEEL]);
    let sink = evdev::sink();
    let c = sink.subscribe("ktest.clamp");
    let r = (|| -> TestResult {
        let mut last_counts = ([0u64; 5], [0u64; 5]);
        for _round in 0..3 {
            while c.pop().is_some() {}
            let before = clamp_counts();
            let begin = time::now_ns();
            for i in 0..100 {
                let v = if i % 2 == 0 { 1 } else { -1 };
                m.emit(time::now_ns(), &[(EV_REL, REL_WHEEL, v), (EV_SYN, SYN_REPORT, 0)]);
                let inj = |kind, code, value| InputEvent { time_ns: begin, kind, code, value, device: m.number, injected: true };
                sink.push(inj(EV_REL, REL_WHEEL, -v));
                sink.push(inj(EV_SYN, SYN_REPORT, 0));
            }
            let end = time::now_ns();
            let after = clamp_counts();
            let mut last = 0;
            let mut packet: Option<u64> = None;
            let mut n = 0;
            while let Some(ev) = c.pop() {
                if ev.device != m.number {
                    continue;
                }
                kassert!(ev.time_ns >= last && ev.time_ns >= begin && ev.time_ns <= end,
                    "event {n} at {} (last {last}, round [{begin}, {end}])", ev.time_ns);
                if ev.injected {
                    kassert!(packet.is_none_or(|t| t == ev.time_ns), "an injected packet has two times");
                    packet = if ev.kind == EV_SYN { None } else { Some(ev.time_ns) };
                }
                last = ev.time_ns;
                n += 1;
            }
            kassert!(n >= 400, "only {n} of 400 events arrived");
            if after == before {
                return Ok(());
            }
            last_counts = (before, after);
        }
        let names = SOURCES.map(|s| s.name());
        Err(alloc::format!("clamped in three rounds: {names:?} before {:?} after {:?}", last_counts.0, last_counts.1).into())
    })();
    sink.unsubscribe(&c);
    m.unregister(time::now_ns());
    r
}
ktest!("in.latency.clamps_zero", Safe, normal_run_clamps_zero);

fn wait_pointer(x: u32, y: u32) -> TestResult {
    let end = time::now_ns() + WAIT_NS;
    loop {
        let p = vt::pointer().ok_or("no pointer")?;
        if (p.0, p.1) == (x, y) {
            return Ok(());
        }
        kassert!(time::now_ns() < end, "pointer at ({}, {}), want ({x}, {y})", p.0, p.1);
        sched::sleep_ns(5_000_000);
    }
}

fn frames(path: Path) -> u64 {
    latency::report().0[path as usize].1.frames
}

/// The newest recent frame of `path` that shows the report stamped `t`: it left after `t`, and its arrival is
/// `t` or earlier (other input pending in the same batch, e.g. QEMU's tablet right after a rebind, is the
/// earliest the frame shows: the frame is charged from that).
fn newest(path: Path, t: u64) -> Option<latency::Sample> {
    latency::report().1.into_iter().rev().find(|s| s.path == path && s.start_ns >= t && s.arrival_ns <= t)
}

fn wait_sample(path: Path, since: u64) -> Result<latency::Sample, alloc::string::String> {
    let end = time::now_ns() + WAIT_NS;
    loop {
        if let Some(s) = newest(path, since) {
            return Ok(s);
        }
        kassert!(time::now_ns() < end, "no {} frame measured", path.name());
        sched::sleep_ns(5_000_000);
    }
}

fn check_sample(s: &latency::Sample, t: u64) -> TestResult {
    kassert!(s.arrival_ns <= t && t <= s.start_ns, "arrival {} after the report's time {t} or frame before it", s.arrival_ns);
    kassert!(s.arrival_ns <= s.picked_ns && s.picked_ns <= s.start_ns && s.start_ns <= s.done_ns, "stages out of order: {s:?}");
    Ok(())
}

/// A mouse report moves the pointer on the console; that frame is measured from the report's time.
fn pointer_frame() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    if vt::graphics() {
        return ktest::skip("a program has the screen");
    }
    let m = fake_mouse("ktest latency mouse", &[0, 1]);
    let r = (|| -> TestResult {
        m.emit(time::now_ns(), &[(EV_REL, 0, -100_000), (EV_REL, 1, -100_000), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(0, 0)?;
        // Other input in the same batch (QEMU's devices) makes the frame's arrival earlier than our report's:
        // a few tries to see one measured from exactly our report's time.
        for i in 0..5u32 {
            let before = frames(Path::Pointer);
            let t = time::now_ns();
            let d = if i % 2 == 0 { 1 } else { -1 };
            m.emit(t, &[(EV_REL, 0, 37 * d), (EV_REL, 1, 23 * d), (EV_SYN, SYN_REPORT, 0)]);
            let (x, y) = if d == 1 { (37, 23) } else { (0, 0) };
            wait_pointer(x, y)?;
            let s = wait_sample(Path::Pointer, t)?;
            check_sample(&s, t)?;
            kassert!(frames(Path::Pointer) > before, "the pointer frame was not counted");
            if s.arrival_ns == t {
                return Ok(());
            }
        }
        Err("5 pointer frames, none measured from its own report's time".into())
    })();
    m.unregister(time::now_ns());
    r
}
ktest!("in.latency.pointer_frame", Safe, pointer_frame);

/// The hanging program's picture: one colour, never presented by anyone.
struct Still {
    px: Vec<u32>,
    w: u32,
    h: u32,
}

impl ScanoutSource for Still {
    fn with_picture(&self, f: &mut dyn FnMut(&[u32], u32, u32)) -> bool {
        f(&self.px, self.w, self.h);
        true
    }
}

const STILL: u32 = 0x0033_6699;

fn wait_drawn(want: display::Rect) -> TestResult {
    let end = time::now_ns() + WAIT_NS;
    while pointer_over::drawn_at() != Some(want) {
        kassert!(time::now_ns() < end, "arrow over the program at {:?}, want {want:?}", pointer_over::drawn_at());
        sched::sleep_ns(5_000_000);
    }
    Ok(())
}

/// The patch copied to the screen for `r` in the last move.
fn patch(r: display::Rect) -> Result<Vec<u32>, alloc::string::String> {
    pointer_over::last_patches().into_iter().find(|(x, _)| *x == r).map(|(_, p)| p).ok_or_else(|| alloc::format!("no patch for {r:?}"))
}

/// A fullscreen "program" holds the screen and never presents (it hangs): the pointer still moves, drawn by
/// the kernel over the program's picture; its old spot gets the picture back; the program's picture and the
/// console frame stay untouched; the frame is measured on the pointer-over-program path.
fn pointer_over_hanging_program() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    if vt::graphics() || display::scanout_holder().is_some() {
        return ktest::skip("a program has the screen");
    }
    let (w, h) = display::with_frame(|f| (f.width, f.height)).ok_or("no frame")?;
    let mut px = Vec::new();
    if px.try_reserve((w * h) as usize).is_err() {
        return ktest::skip("no memory for a full-screen picture");
    }
    px.resize((w * h) as usize, STILL);
    let still = Arc::new(Still { px, w, h });
    let m = fake_mouse("ktest latency over", &[0, 1]);
    m.emit(time::now_ns(), &[(EV_REL, 0, -100_000), (EV_REL, 1, -100_000), (EV_SYN, SYN_REPORT, 0)]);
    wait_pointer(0, 0)?;
    let guard = match display::take_scanout_with("ktest.pointer_over", Some(still.clone()), 0) {
        Ok(g) => g,
        Err(e) => {
            m.unregister(time::now_ns());
            return ktest::skip(&alloc::format!("scanout busy: {e:?}"));
        }
    };
    pointer_over::set_record(true);
    vt::set_kd_mode(true, None);
    let scale = vt::font().map(|f| (f.h / 16).max(1)).unwrap_or(1);
    let r = (|| -> TestResult {
        let before = frames(Path::PointerOverProgram);
        let t = time::now_ns();
        m.emit(t, &[(EV_REL, 0, 40 * scale as i32), (EV_REL, 1, 30 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
        let (x, y) = (40 * scale, 30 * scale);
        wait_pointer(x, y)?;
        let at = pointer_over::arrow_rect(x, y, scale, w, h);
        wait_drawn(at)?;
        let p = patch(at)?;
        let px = |p: &[u32], dx: u32, dy: u32| p[(dy * at.w + dx) as usize];
        kassert!(px(&p, 0, 0) == POINTER_OUTLINE, "tip is not the outline over the program");
        kassert!(px(&p, scale, 3 * scale) == POINTER_FILL, "inside is not the fill over the program");
        kassert!(px(&p, ARROW_W * scale - 1, 0) == STILL, "beside the arrow is not the program's picture");
        // The console's arrow (at 0,0 when the program took the screen) is gone from the screen.
        let old = pointer_over::arrow_rect(0, 0, scale, w, h);
        kassert!(patch(old)?.iter().all(|&c| c == STILL), "the console's old arrow stayed on the screen");
        let s = wait_sample(Path::PointerOverProgram, t)?;
        check_sample(&s, t)?;
        kassert!(frames(Path::PointerOverProgram) > before, "the frame over the program was not counted");

        // A second move: the old spot gets the program's picture back, the console frame is not touched.
        let (x2, y2) = (x + 100 * scale, y);
        let at2 = pointer_over::arrow_rect(x2, y2, scale, w, h);
        let frame_before = display::with_frame(|f| pointer_over::copy_out_vec(f.pixels(), w, h, at2)).flatten().ok_or("console frame")?;
        m.emit(time::now_ns(), &[(EV_REL, 0, 100 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(x2, y2)?;
        wait_drawn(at2)?;
        kassert!(patch(at)?.iter().all(|&c| c == STILL), "the old spot did not get the program's picture back");
        kassert!(px(&patch(at2)?, 0, 0) == POINTER_OUTLINE, "no arrow at the new spot");
        let frame_after = display::with_frame(|f| pointer_over::copy_out_vec(f.pixels(), w, h, at2)).flatten().ok_or("console frame")?;
        kassert!(frame_before == frame_after, "the console frame changed under the arrow");
        kassert!(still.px.iter().all(|&c| c == STILL), "the program's picture changed");

        // IN-6b: a program grabs the mouse (it draws its own pointer): the arrow leaves the screen at once and
        // another mouse's moves draw none; after the ungrab they draw again.
        if pointer_over::grabbed() {
            return Ok(()); // a program on this machine already holds a pointer grab (bitscomp)
        }
        let id = evdev::sink().attach(Arc::new(Swallow));
        let m2 = fake_mouse("ktest latency over 2", &[0, 1]);
        let r = (|| -> TestResult {
            evdev::sink().grab(m.event_id(), id).map_err(|e| alloc::format!("grab: errno {e}"))?;
            kassert!(pointer_over::drawn_at().is_none() && !pointer_over::enabled(), "the arrow stayed on after the grab");
            kassert!(patch(at2)?.iter().all(|&c| c == STILL), "the arrow's spot did not get the program's picture back");
            m2.emit(time::now_ns(), &[(EV_REL, 0, 20 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
            wait_pointer(x2 + 20 * scale, y2)?;
            kassert!(pointer_over::drawn_at().is_none(), "an arrow was drawn while a program holds a pointer grab");
            evdev::sink().ungrab(m.event_id(), id).map_err(|e| alloc::format!("ungrab: errno {e}"))?;
            kassert!(pointer_over::enabled(), "the pointer stayed off after the ungrab");
            m2.emit(time::now_ns(), &[(EV_REL, 0, 20 * scale as i32), (EV_SYN, SYN_REPORT, 0)]);
            wait_pointer(x2 + 40 * scale, y2)?;
            wait_drawn(pointer_over::arrow_rect(x2 + 40 * scale, y2, scale, w, h))
        })();
        evdev::sink().detach(id);
        m2.unregister(time::now_ns());
        r
    })();
    pointer_over::set_record(false);
    vt::set_kd_mode(false, None);
    drop(guard);
    m.unregister(time::now_ns());
    kassert!(pointer_over::drawn_at().is_none(), "the arrow over the program is still remembered after KD_TEXT");
    r
}
ktest!("in.latency.pointer_over_program", Safe, pointer_over_hanging_program);

/// Takes every event and keeps none (the grabbing program of `pointer_grabbed`).
struct Swallow;

impl evdev::Listener for Swallow {
    fn push(&self, _ev: evdev::InputEvent) {}
}

/// Decided for IN-6 (notes, fixer table): a program that grabs a mouse (EVIOCGRAB) owns it alone, the VT
/// included, as in Linux (linux-6.6/drivers/input/input.c:155, evdev.c:341): its moves do not move the kernel's
/// pointer, on the console or over a program; once the grab ends they do again.
fn pointer_grabbed() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if display::device().is_none() {
        return ktest::skip("no display device (no GOP framebuffer)");
    }
    if vt::graphics() {
        return ktest::skip("a program has the screen");
    }
    let sink = evdev::sink();
    let m = fake_mouse("ktest latency grabbed", &[0, 1]);
    let id = sink.attach(Arc::new(Swallow));
    let r = (|| -> TestResult {
        m.emit(time::now_ns(), &[(EV_REL, 0, -100_000), (EV_REL, 1, -100_000), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(0, 0)?;
        sink.grab(m.event_id(), id).map_err(|e| alloc::format!("grab: errno {e}"))?;
        m.emit(time::now_ns(), &[(EV_REL, 0, 50), (EV_REL, 1, 40), (EV_SYN, SYN_REPORT, 0)]);
        // The VT would have the move within a few ms; give it 100 ms and an empty queue.
        let end = time::now_ns() + 100_000_000;
        while time::now_ns() < end || vt::input_pending() > 0 {
            kassert!(time::now_ns() < end + WAIT_NS, "the VT's queue does not drain");
            sched::sleep_ns(5_000_000);
        }
        let p = vt::pointer().ok_or("no pointer")?;
        kassert!((p.0, p.1) == (0, 0), "a grabbed mouse moved the kernel's pointer to ({}, {})", p.0, p.1);
        sink.ungrab(m.event_id(), id).map_err(|e| alloc::format!("ungrab: errno {e}"))?;
        m.emit(time::now_ns(), &[(EV_REL, 0, 50), (EV_REL, 1, 40), (EV_SYN, SYN_REPORT, 0)]);
        wait_pointer(50, 40)
    })();
    sink.detach(id);
    m.unregister(time::now_ns());
    r
}
ktest!("in.latency.pointer_grabbed", Safe, pointer_grabbed);

/// IN-6b (WY-1a change request 2): the console's arrow over programs is off exactly while some program holds the
/// grab of a pointer device on the machine's sink, however the grab ends (EVIOCGRAB 0, the handle closing, the
/// device going). A keyboard's grab and a grab on a private sink change nothing.
fn pointer_grab_switch() -> TestResult {
    let _console = crate::ktest_lease::ConsoleLease::take()?;
    if pointer_over::grabbed() {
        return ktest::skip("a program holds a pointer grab (bitscomp)");
    }
    let sink = evdev::sink();
    let mut kcaps = Caps::default();
    kcaps.keys.insert(30);
    let k = evdev::register(sink, DeviceInfo { name: "ktest grab kbd".to_string(), bus: evdev::BUS_VIRTUAL, caps: kcaps, ..DeviceInfo::default() });
    let m = fake_mouse("ktest grab mouse", &[0, 1]);
    let mut gcaps = Caps::default();
    gcaps.keys.extend([0x130u16, 0x131]); // BTN_GAMEPAD (BTN_SOUTH), BTN_EAST
    gcaps.abs.extend([0u16, 1, 2, 5].map(|a| (a, evdev::AbsInfo::default())));
    let gp = evdev::register(sink, DeviceInfo { name: "ktest grab gamepad".to_string(), bus: evdev::BUS_VIRTUAL, caps: gcaps, ..DeviceInfo::default() });
    static S: EvdevSink = EvdevSink::new();
    let pm = fake_mouse_on(&S, "ktest grab private", &[0, 1]);
    let (a, b) = (sink.attach(Arc::new(Swallow)), S.attach(Arc::new(Swallow)));
    let r = (|| -> TestResult {
        let on = || pointer_over::enabled() && !pointer_over::grabbed();
        let err = |e| alloc::format!("errno {e}");
        sink.grab(k.event_id(), a).map_err(err)?;
        S.grab(pm.event_id(), b).map_err(err)?;
        sink.grab(gp.event_id(), a).map_err(err)?;
        kassert!(on(), "a keyboard's, a gamepad's or a private sink's grab switched the pointer off");
        sink.grab(m.event_id(), a).map_err(err)?;
        kassert!(pointer_over::grabbed() && !pointer_over::enabled(), "a mouse grab left the pointer on");
        sink.ungrab(m.event_id(), a).map_err(err)?;
        kassert!(on(), "EVIOCGRAB 0 left the pointer off");
        let c = sink.attach(Arc::new(Swallow));
        sink.grab(m.event_id(), c).map_err(err)?;
        kassert!(!on(), "the second grab left the pointer on");
        sink.detach(c);
        kassert!(on(), "closing the grabbing handle left the pointer off");
        sink.grab(m.event_id(), a).map_err(err)?;
        kassert!(!on(), "the third grab left the pointer on");
        m.unregister(time::now_ns());
        kassert!(on(), "the grabbed mouse went and the pointer stayed off");
        Ok(())
    })();
    sink.detach(a);
    S.detach(b);
    if !m.is_gone() {
        m.unregister(time::now_ns());
    }
    k.unregister(time::now_ns());
    gp.unregister(time::now_ns());
    pm.unregister(time::now_ns());
    kassert!(!pointer_over::grabbed(), "a pointer grab is left over");
    r
}
ktest!("in.latency.pointer_grab_switch", Safe, pointer_grab_switch);
