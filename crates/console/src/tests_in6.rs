//! IN-6 host tests: the pointer over a program (patch planning, drawing, the blit through a scratch frame, the
//! redraw after a program's present), the latency numbers and the monotonic event times.

use crate::evdev::{self, EV_REL, EV_SYN, EvdevSink, InputEvent};
use crate::latency::{self, Path, PathStats, Sample};
use crate::pointer_over::{self, arrow_rect, blit, copy_out, draw_arrow, plan};
use crate::render::{POINTER_FILL, POINTER_OUTLINE};
use alloc::vec;
use alloc::vec::Vec;
use devmodel::DevError;
use devmodel::display::{DisplayDevice, Fence, Mode, PixelFormat, VblankEvent};
use display::{RamFrame, Rect};
use ksync::SpinLock;

/// A screen in memory: what `present` copied.
struct Screen {
    w: u32,
    h: u32,
    px: SpinLock<Vec<u32>>,
}

impl Screen {
    fn new(w: u32, h: u32, c: u32) -> Screen {
        Screen { w, h, px: SpinLock::named("test.screen", vec![c; (w * h) as usize]) }
    }
    fn at(&self, x: u32, y: u32) -> u32 {
        self.px.lock()[(y * self.w + x) as usize]
    }
}

impl DisplayDevice for Screen {
    fn name(&self) -> &str {
        "test"
    }
    fn modes(&self) -> Vec<Mode> {
        vec![self.current_mode()]
    }
    fn current_mode(&self) -> Mode {
        Mode { width: self.w, height: self.h, refresh_mhz: 0 }
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Bgrx8888
    }
    fn present(&self, src: &[u32], stride: u32, d: (u32, u32, u32, u32)) -> Result<Fence, DevError> {
        if d.0 + d.2 > self.w || d.1 + d.3 > self.h {
            return Err(DevError::InvalidArgument);
        }
        let mut px = self.px.lock();
        for y in d.1..d.1 + d.3 {
            for x in d.0..d.0 + d.2 {
                px[(y * self.w + x) as usize] = src[(y * stride + x) as usize];
            }
        }
        Ok(Fence { seq: 0, signalled: true })
    }
    fn last_vblank(&self) -> VblankEvent {
        VblankEvent::default()
    }
}

fn frame(w: u32, h: u32, c: u32) -> RamFrame {
    let mut f = RamFrame::new(w, h);
    f.pixels_mut().fill(c);
    f.take_damage();
    f
}

#[test]
fn plan_joins_near_and_splits_far() {
    let a = Rect::new(10, 10, 12, 19);
    let b = Rect::new(15, 12, 12, 19);
    assert_eq!(plan(None, b), [Some(b), None]);
    assert_eq!(plan(Some(a), b), [Some(a.union(b)), None], "overlapping moves copy one rectangle");
    let far = Rect::new(200, 100, 12, 19);
    assert_eq!(plan(Some(a), far), [Some(a), Some(far)], "a jump restores the old spot and draws the new one");
    assert_eq!(arrow_rect(95, 95, 2, 100, 100), Rect::new(95, 95, 5, 5), "clipped at the screen's edge");
}

#[test]
fn arrow_lands_in_the_patch_at_its_hot_spot() {
    let r = Rect::new(8, 8, 20, 30);
    let mut p = vec![0x00AB_CDEF; (r.w * r.h) as usize];
    draw_arrow(&mut p, r, 10, 12, 1);
    let at = |x: u32, y: u32| p[((y - r.y) * r.w + (x - r.x)) as usize];
    assert_eq!(at(10, 12), POINTER_OUTLINE, "tip");
    assert_eq!(at(11, 15), POINTER_FILL, "inside");
    assert_eq!(at(9, 12), 0x00AB_CDEF, "left of the tip is the picture");
    assert_eq!(at(20, 12), 0x00AB_CDEF, "right of the tip row is the picture");
    // Partly outside the patch: no panic, only the overlap drawn.
    let mut small = vec![1; 4];
    draw_arrow(&mut small, Rect::new(0, 0, 2, 2), 1, 1, 3);
    assert_eq!(small[3], POINTER_OUTLINE);
}

#[test]
fn copy_out_refuses_outside() {
    let f = frame(10, 10, 7);
    let mut out = Vec::new();
    assert!(copy_out(f.pixels(), 10, 10, Rect::new(8, 8, 2, 2), &mut out));
    assert_eq!(out, vec![7; 4]);
    assert!(!copy_out(f.pixels(), 10, 10, Rect::new(9, 9, 2, 2), &mut out));
    assert!(!copy_out(&[0; 5], 10, 10, Rect::new(0, 0, 2, 2), &mut out), "a short picture is refused, not indexed");
}

#[test]
fn blit_shows_the_patch_and_leaves_the_frame_alone() {
    let scr = Screen::new(40, 40, 0);
    let mut f = frame(40, 40, 0x11);
    let r = Rect::new(5, 6, 3, 2);
    let patch = vec![0x22; 6];
    let mut keep = Vec::new();
    assert!(blit(&scr, &mut f, r, &patch, &mut keep));
    assert_eq!(scr.at(5, 6), 0x22);
    assert_eq!(scr.at(7, 7), 0x22);
    assert_eq!(scr.at(8, 6), 0, "only the patch went to the screen");
    assert!(f.pixels().iter().all(|&p| p == 0x11), "the scratch frame is as before");
    assert!(f.damage_rects().is_empty(), "no damage marked");
    assert!(!blit(&scr, &mut f, r, &patch[..5], &mut keep), "a patch of the wrong size is refused");
}

#[test]
fn after_present_draws_the_arrow_on_top_of_a_program_frame() {
    // Without a move over a program nothing is drawn.
    let scr = Screen::new(64, 64, 0);
    let mut f = frame(64, 64, 0x33);
    pointer_over::after_present(&scr, &mut f, Rect::new(0, 0, 64, 64));
    assert_eq!(scr.at(10, 10), 0, "no arrow is shown over a program");
    assert_eq!(pointer_over::drawn_at(), None);
    // Shown at (10, 10): a present elsewhere leaves it, one over it draws it again on top.
    let at = arrow_rect(10, 10, 1, 64, 64);
    pointer_over::remember(at, 10, 10, 1);
    pointer_over::after_present(&scr, &mut f, Rect::new(40, 40, 8, 8));
    assert_eq!(scr.at(10, 10), 0, "a present elsewhere does not draw");
    pointer_over::after_present(&scr, &mut f, Rect::new(0, 0, 64, 64));
    assert_eq!(scr.at(10, 10), POINTER_OUTLINE, "tip on top of the program's frame");
    assert_eq!(scr.at(11, 13), POINTER_FILL);
    assert!(f.pixels().iter().all(|&p| p == 0x33), "the program's frame is unchanged");
    assert_eq!(pointer_over::forget(), Some(at));
    pointer_over::after_present(&scr, &mut f, Rect::new(0, 0, 64, 64));
    assert_eq!(pointer_over::drawn_at(), None, "forgotten when the console takes the screen back");
}

#[test]
fn percentiles_from_the_histogram() {
    let mut s = Vec::new();
    for us in [100u64, 200, 300, 400, 30_000] {
        s.push(Sample { path: Path::Pointer, arrival_ns: 1_000, picked_ns: 2_000, start_ns: 1_000 + us * 1000, done_ns: 2_000 + us * 1000 });
    }
    let mut p = None;
    latency::reset();
    for x in &s {
        latency::record(*x);
    }
    let (paths, recent) = latency::report();
    for (path, st) in paths {
        if path == Path::Pointer {
            p = Some(st);
        }
    }
    let p: PathStats = p.unwrap();
    assert_eq!(p.frames, 5);
    assert_eq!(p.over_budget, 1, "30 ms is over the 20 ms budget");
    assert_eq!(p.min_ns, 100_000);
    assert_eq!(p.max_ns, 30_000_000);
    assert!(p.percentile_ns(50) >= 300_000 && p.percentile_ns(50) <= 512_000, "p50 {}", p.percentile_ns(50));
    assert_eq!(p.percentile_ns(100), 30_000_000, "capped at the max");
    assert_eq!(recent.len(), 5);
    // Stale samples stay out of the numbers.
    latency::record(Sample { path: Path::Pointer, arrival_ns: 1, picked_ns: 1, start_ns: 2_000_000_001, done_ns: 2_000_000_002 });
    let (paths, _) = latency::report();
    let p = &paths[Path::Pointer as usize].1;
    assert_eq!((p.frames, p.stale), (5, 1));
    latency::set_keep(2);
    latency::record(s[0]);
    assert_eq!(latency::report().1.len(), 2, "keeps the newest `keep` frames");
    latency::set_keep(latency::KEEP_DEFAULT);
    latency::reset();
}

#[test]
fn event_times_never_go_back() {
    let sink: &'static EvdevSink = alloc::boxed::Box::leak(alloc::boxed::Box::new(EvdevSink::new()));
    let c = sink.subscribe("t");
    let ev = |t: u64| InputEvent { time_ns: t, kind: EV_REL, code: 0, value: 1, device: 1, injected: false };
    sink.push(ev(100));
    sink.push(ev(90)); // stamped on another CPU before the first, pushed after it
    sink.push(InputEvent { time_ns: 120, kind: EV_SYN, code: 0, value: 0, device: 1, injected: false });
    let times: Vec<u64> = core::iter::from_fn(|| c.pop()).map(|e| e.time_ns).collect();
    assert_eq!(times, vec![100, 100, 120]);
    let own: Vec<u64> = core::iter::from_fn(|| sink.pop()).map(|e| e.time_ns).collect();
    assert_eq!(own, vec![100, 100, 120], "the sink's own ring too");
    let _ = evdev::EV_KEY;
    let mut ring = crate::evdevfs::Ring::new(8);
    ring.pass(ev(100));
    ring.pass(ev(90));
    ring.pass(InputEvent { time_ns: 95, kind: EV_SYN, code: 0, value: 0, device: 1, injected: false });
    let a = ring.fetch().unwrap().time_ns;
    let b = ring.fetch().unwrap().time_ns;
    let c = ring.fetch().unwrap().time_ns;
    assert_eq!((a, b, c), (100, 100, 100), "a /dev/input reader's ring too");
    assert_eq!(ring.take_clamps(), (2, 10), "the ring counts its clamps and the largest step back");
    assert_eq!(ring.take_clamps(), (0, 0), "taken once");
    assert_eq!(sink.clamped(), 1, "the sink counted the late event once, its reader did not count it again");
}

/// IN-6b: injected events (MCP) keep their caller's time per packet when it is in order, one time per packet
/// even when another device's packet is open meanwhile; a written event never keeps the writer's time.
#[test]
fn injected_packets_keep_one_time() {
    let sink: &'static EvdevSink = alloc::boxed::Box::leak(alloc::boxed::Box::new(EvdevSink::new()));
    let ev = |t: u64, dev: u32, kind: u16, injected: bool| InputEvent { time_ns: t, kind, code: 0, value: 1, device: dev, injected };
    sink.push(ev(7, 5, EV_REL, true));
    sink.push(ev(20, 6, EV_REL, true));
    sink.push(ev(9, 5, EV_SYN, true));
    sink.push(ev(25, 6, EV_SYN, true));
    let own: Vec<(u32, u64)> = core::iter::from_fn(|| sink.pop()).map(|e| (e.device, e.time_ns)).collect();
    // Device 6's packet began at 20 between device 5's first event (7) and its SYN: device 5's open packet fell
    // behind, so its SYN starts again with the sink's clock, never behind 20 (on the host the clock reads 0), and
    // nothing is clamped (fixer, checker finding 1/8).
    assert_eq!(own, vec![(5, 7), (6, 20), (5, 20), (6, 20)]);
    assert_eq!(sink.clamped(), 0);
    sink.push(ev(30, 5, EV_REL, true));
    sink.push(ev(31, 5, EV_SYN, true));
    let own: Vec<u64> = core::iter::from_fn(|| sink.pop()).map(|e| e.time_ns).collect();
    assert_eq!(own, vec![30, 30], "one time per packet, the caller's");
    sink.push_written(ev(1_000_000, 7, EV_REL, false));
    let e = sink.pop().unwrap();
    assert!(e.injected && e.time_ns != 1_000_000, "a written event is injected and stamped by the sink: {e:?}");
}

/// Fixer (checker findings 1 and 8): a program's write without SYN_REPORT neither captures MCP's packets on the same
/// device nor makes every later injected event clamp; an open packet that fell behind starts again.
#[test]
fn unfinished_packets_do_not_clamp() {
    let sink: &'static EvdevSink = alloc::boxed::Box::leak(alloc::boxed::Box::new(EvdevSink::new()));
    let ev = |t: u64, dev: u32, kind: u16, injected: bool| InputEvent { time_ns: t, kind, code: 0, value: 1, device: dev, injected };
    sink.push(ev(30, 1, EV_REL, false));
    sink.push_written(ev(0, 8, EV_REL, false)); // no SYN_REPORT ever: the sink's clock, never behind 30
    sink.push(ev(40, 8, EV_REL, true)); // MCP on the same device: a packet of its own, its caller's time
    sink.push(ev(45, 8, EV_SYN, true));
    sink.push_written(ev(0, 8, EV_REL, false)); // the write's open packet (30) fell behind 40: starts again
    sink.push(ev(60, 9, EV_REL, true)); // an injected packet left open
    sink.push(ev(70, 1, EV_REL, false)); // a device's report after it
    sink.push(ev(65, 9, EV_REL, true)); // the open packet (60) fell behind 70: starts again, not clamped
    sink.push(ev(66, 9, EV_SYN, true));
    let own: Vec<(u32, u64)> = core::iter::from_fn(|| sink.pop()).map(|e| (e.device, e.time_ns)).collect();
    assert_eq!(own, vec![(1, 30), (8, 30), (8, 40), (8, 40), (8, 40), (9, 60), (1, 70), (9, 70), (9, 70)]);
    assert_eq!(sink.clamped(), 0);
}

/// Fixer (checker finding 4): only devices that move the screen's pointer (as Linux's mousedev takes them) switch
/// the console's arrow off when grabbed; a gamepad's sticks do not.
#[test]
fn only_pointers_move_the_pointer() {
    use crate::evdev::{AbsInfo, Caps};
    let caps = |keys: &[u16], rel: &[u16], abs: &[u16]| {
        let mut c = Caps::default();
        c.keys.extend(keys.iter().copied());
        c.rel.extend(rel.iter().copied());
        c.abs.extend(abs.iter().map(|&a| (a, AbsInfo::default())));
        c
    };
    assert!(caps(&[0x110], &[0, 1], &[]).moves_pointer(), "mouse");
    assert!(caps(&[0x14a], &[], &[0, 1]).moves_pointer(), "touchscreen");
    assert!(caps(&[0x140, 0x14a], &[], &[0, 1]).moves_pointer(), "tablet");
    assert!(caps(&[0x145, 0x110], &[], &[0, 1]).moves_pointer(), "touchpad");
    assert!(!caps(&[0x130, 0x131], &[], &[0, 1, 2, 5]).moves_pointer(), "gamepad");
    assert!(!caps(&[0x120, 0x121], &[], &[0, 1]).moves_pointer(), "joystick");
    assert!(!caps(&[0x110], &[8], &[]).moves_pointer(), "wheel only");
    assert!(!caps(&[30], &[], &[]).moves_pointer(), "keyboard");
}
