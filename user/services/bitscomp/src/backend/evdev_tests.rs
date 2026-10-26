use super::evdev_map::*;

fn bits(codes: &[u16]) -> Vec<u8> {
    let mut v = vec![0u8; KEY_MAX as usize / 8 + 1];
    for &c in codes {
        v[c as usize / 8] |= 1 << (c % 8);
    }
    v
}

fn mouse() -> Decoder {
    let keys = bits(&[BTN_LEFT, BTN_LEFT + 1, BTN_LEFT + 2]);
    let rel = bits(&[REL_X, REL_Y, REL_WHEEL, REL_WHEEL_HI_RES, REL_HWHEEL, REL_HWHEEL_HI_RES]);
    Decoder::new(Caps::from_bits(&keys, &rel, &bits(&[]), None))
}

fn feed(d: &mut Decoder, evs: &[(u16, u16, i32)]) -> Vec<Out> {
    let mut out = Vec::new();
    for &(t, c, v) in evs {
        d.feed(t, c, v, &mut out);
    }
    out
}

#[test]
fn classifies_devices() {
    let kb = Caps::from_bits(&bits(&(1..0x100).collect::<Vec<_>>()), &bits(&[]), &bits(&[]), None);
    assert!(kb.keyboard && !kb.pointer() && kb.wanted());
    let m = mouse().caps;
    assert!(m.rel_xy && m.buttons && m.wheel_hi && m.hwheel_hi && !m.keyboard);
    let r = Range { min: 0, max: 1023 };
    // The MCP pointer: relative and absolute, left button.
    let mcp = Caps::from_bits(&bits(&[BTN_LEFT]), &bits(&[REL_X, REL_Y]), &bits(&[ABS_X, ABS_Y]), Some((r, r)));
    assert!(mcp.rel_xy && mcp.abs.is_some());
    // A pad: X/Y axes but no left button or touch.
    let pad = Caps::from_bits(&bits(&[0x130]), &bits(&[]), &bits(&[ABS_X, ABS_Y]), Some((r, r)));
    assert!(!pad.wanted());
    // A power button: one key, no letters.
    assert!(!Caps::from_bits(&bits(&[116]), &bits(&[]), &bits(&[]), None).wanted());
}

#[test]
fn motion_buttons_and_wheel_per_report() {
    let mut d = mouse();
    let out = feed(&mut d, &[(EV_REL, REL_X, 3), (EV_REL, REL_Y, -2), (EV_KEY, BTN_LEFT, 1), (EV_REL, REL_X, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Rel { dx: 4.0, dy: -2.0 }, Out::Button { code: 0x110, pressed: true }]);
    // Hi-res wheel: the coarse REL_WHEEL of the same frame is ignored; up (positive) is Wayland's negative.
    let out = feed(&mut d, &[(EV_REL, REL_WHEEL_HI_RES, 60), (EV_REL, REL_WHEEL, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Wheel { steps: -0.5 }]);
    let out = feed(&mut d, &[(EV_REL, REL_HWHEEL_HI_RES, 120), (EV_REL, REL_HWHEEL, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::HWheel { steps: 1.0 }]);
    // Nothing before SYN_REPORT.
    assert!(feed(&mut d, &[(EV_KEY, BTN_LEFT, 0)]).is_empty());
    assert_eq!(feed(&mut d, &[(EV_SYN, SYN_REPORT, 0)]), vec![Out::Button { code: 0x110, pressed: false }]);
}

#[test]
fn coarse_wheel_without_hi_res() {
    let mut d = Decoder::new(Caps::from_bits(&bits(&[BTN_LEFT]), &bits(&[REL_X, REL_Y, REL_WHEEL]), &bits(&[]), None));
    assert_eq!(feed(&mut d, &[(EV_REL, REL_WHEEL, -2), (EV_SYN, SYN_REPORT, 0)]), vec![Out::Wheel { steps: 2.0 }]);
}

#[test]
fn keys_ignore_repeat_and_double_press() {
    let mut d = Decoder::new(Caps::from_bits(&bits(&(1..0x100).collect::<Vec<_>>()), &[], &[], None));
    let out = feed(&mut d, &[(EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0), (EV_KEY, 30, 2), (EV_SYN, SYN_REPORT, 0), (EV_KEY, 30, 1), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Key { code: 30, pressed: true }]);
    // A release of a key never seen pressed is dropped; joystick buttons (0x120..) are not keys.
    assert!(feed(&mut d, &[(EV_KEY, 31, 0), (EV_KEY, 0x120, 1), (EV_SYN, SYN_REPORT, 0)]).is_empty());
    let mut rel = Vec::new();
    d.release_all(&mut rel);
    assert_eq!(rel, vec![Out::Key { code: 30, pressed: false }]);
}

#[test]
fn syn_dropped_releases_and_skips_to_next_report() {
    let mut d = mouse();
    feed(&mut d, &[(EV_KEY, BTN_LEFT, 1), (EV_SYN, SYN_REPORT, 0)]);
    let out = feed(&mut d, &[(EV_REL, REL_X, 5), (EV_SYN, SYN_DROPPED, 0), (EV_REL, REL_X, 7), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Button { code: 0x110, pressed: false }]);
    assert_eq!(feed(&mut d, &[(EV_REL, REL_X, 2), (EV_SYN, SYN_REPORT, 0)]), vec![Out::Rel { dx: 2.0, dy: 0.0 }]);
}

#[test]
fn absolute_positions() {
    let r = Range { min: 0, max: 1279 };
    let ry = Range { min: 0, max: 799 };
    let mut d = Decoder::new(Caps::from_bits(&bits(&[BTN_LEFT]), &[], &bits(&[ABS_X, ABS_Y]), Some((r, ry))));
    let out = feed(&mut d, &[(EV_ABS, ABS_X, 1279), (EV_ABS, ABS_Y, 0), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Abs { fx: 1.0, fy: 0.0 }]);
    // Only Y changed: X stays where it was.
    let out = feed(&mut d, &[(EV_ABS, ABS_Y, 799), (EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(out, vec![Out::Abs { fx: 1.0, fy: 1.0 }]);
    assert_eq!(placed(1.0, 0.5, (2560, 1440)), (2559.0, 719.5));
}

#[test]
fn acceleration_and_clamp() {
    assert_eq!(accel(2.0, -1.0), (2.0, -1.0));
    let (a, _) = accel(20.0, 0.0);
    assert!((a - 40.4).abs() < 1e-9, "{a}");
    assert_eq!(accel(1000.0, 0.0).0, 2500.0);
    assert_eq!(moved((10.0, 10.0), (-50.0, 5000.0), (100, 80)), (0.0, 79.0));
}

fn keyboard() -> Decoder {
    let mut codes: Vec<u16> = (1..=88).collect();
    codes.push(KEY_Z);
    Decoder::new(Caps::from_bits(&bits(&codes), &bits(&[]), &bits(&[]), None))
}

fn event(ty: u16, code: u16, value: i32) -> [u8; 24] {
    let mut e = [0u8; 24];
    e[16..18].copy_from_slice(&ty.to_le_bytes());
    e[18..20].copy_from_slice(&code.to_le_bytes());
    e[20..24].copy_from_slice(&value.to_le_bytes());
    e
}

/// A non-blocking pipe standing in for an event node; the write end is returned for the test to fill or close.
fn fake_node() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is valid for two ints; both fds are owned below.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) }, 0);
    // SAFETY: both were just made by pipe2.
    unsafe { (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1])) }
}

fn write_events(w: &std::os::fd::OwnedFd, evs: &[(u16, u16, i32)]) {
    use std::io::Write;
    let mut f = std::fs::File::from(w.try_clone().unwrap());
    for &(t, c, v) in evs {
        f.write_all(&event(t, c, v)).unwrap();
    }
}

#[test]
fn unplugged_device_releases_held_keys() {
    use crate::backend::evdev::pump;
    use std::os::fd::AsRawFd;
    let (r, w) = fake_node();
    let mut d = keyboard();
    let mut stats = crate::stats::Stats::default();
    write_events(&w, &[(EV_KEY, KEY_A, 1), (EV_SYN, SYN_REPORT, 0)]);
    let mut out = Vec::new();
    assert!(pump(r.as_raw_fd(), &mut d, &mut out, &mut stats), "nothing more yet: still there");
    assert_eq!(out, vec![Out::Key { code: KEY_A as u32, pressed: true }]);
    // The device goes (end of file here, ENODEV on V2 and Linux: the same branch): A is let go.
    drop(w);
    let mut out = Vec::new();
    assert!(!pump(r.as_raw_fd(), &mut d, &mut out, &mut stats), "gone");
    assert_eq!(out, vec![Out::Key { code: KEY_A as u32, pressed: false }]);
    assert_eq!(stats.input_events, 1);
}

#[test]
fn flooding_device_yields_after_max_reads() {
    use crate::backend::evdev::{MAX_READS, pump};
    use std::os::fd::AsRawFd;
    let (r, w) = fake_node();
    let mut d = keyboard();
    let mut stats = crate::stats::Stats::default();
    let reports = MAX_READS * 32 + 10;
    let mut evs = Vec::new();
    for _ in 0..reports {
        evs.extend([(EV_KEY, KEY_A, 1), (EV_SYN, SYN_REPORT, 0)]);
    }
    write_events(&w, &evs);
    let mut out = Vec::new();
    assert!(pump(r.as_raw_fd(), &mut d, &mut out, &mut stats));
    assert_eq!(stats.input_events as usize, MAX_READS * 32, "one call reads MAX_READS x 64 events, no more");
    assert!(pump(r.as_raw_fd(), &mut d, &mut out, &mut stats));
    assert_eq!(stats.input_events as usize, reports, "the rest on the next turn");
}

#[test]
fn seen_retries_then_gives_up_and_forgets_vanished_nodes() {
    use crate::backend::evdev::{MAX_TRIES, Seen};
    use std::collections::HashSet;
    let k = (std::path::PathBuf::from("/dev/input/event3"), 7u64);
    let mut s = Seen::default();
    for i in 1..MAX_TRIES {
        assert!(s.wants(&k));
        assert!(!s.failed(k.clone()), "try {i} is not the last");
    }
    assert!(s.failed(k.clone()), "the last try");
    assert!(!s.wants(&k), "given up");
    // Still listed: stays given up (and an ENODEV'd node is not re-opened while it still shows).
    s.retain(&HashSet::from([k.clone()]));
    assert!(!s.wants(&k));
    // Gone from /dev/input: forgotten; made again, it is tried afresh.
    s.retain(&HashSet::new());
    assert!(s.wants(&k));
    assert!(!s.failed(k.clone()), "tries counted from zero again");
    s.settle(k.clone());
    assert!(!s.wants(&k));
}
