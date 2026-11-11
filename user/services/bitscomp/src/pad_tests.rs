use super::*;

const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);
const STICK: Axis = Axis { min: 0, max: 255, flat: 15 };

fn dragonrise() -> Pad {
    let keys: Vec<u16> = (0x120..=0x12b).collect();
    Pad::new(mapping(0x0079, 0x0006, &keys), Some(STICK), Some(STICK))
}

/// Events at `now`, then a SYN_REPORT.
fn frame(p: &mut Pad, now: u64, evs: &[(u16, u16, i32)]) -> Vec<Press> {
    let mut out = Vec::new();
    for &(t, c, v) in evs.iter().chain([SYN].iter()) {
        p.feed(t, c, v, now, &mut out);
    }
    out
}

fn tick(p: &mut Pad, now: u64) -> Vec<Press> {
    let mut out = Vec::new();
    p.tick(now, &mut out);
    out
}

fn key(code: u16, v: i32) -> (u16, u16, i32) {
    (EV_KEY, code, v)
}

const RIGHT: Press = Press::Move(Dir::Right);

#[test]
fn dragonrise_is_table_mapped_as_sdl_says() {
    let p = dragonrise();
    assert_eq!(p.map.source, Source::Table);
    for (code, b) in [(0x122, Btn::A), (0x121, Btn::B), (0x123, Btn::X), (0x120, Btn::Y), (0x124, Btn::LB), (0x125, Btn::RB), (0x128, Btn::Select), (0x129, Btn::Start)] {
        assert_eq!(p.map.of(code), Some(b), "{code:#x}");
    }
    assert!(p.map.chord(), "no Home button: Select+Start is Home");
}

#[test]
fn dragonrise_b2_is_a_and_repeats_are_ignored() {
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 0, &[key(0x122, 1)]), vec![Press::Btn(Btn::A)]);
    // The kernel's soft repeat (2) and a second press report while held give nothing.
    assert!(frame(&mut p, 10, &[key(0x122, 2)]).is_empty());
    assert!(frame(&mut p, 20, &[key(0x122, 0)]).is_empty());
    assert_eq!(frame(&mut p, 30, &[key(0x121, 1)]), vec![Press::Btn(Btn::B)]);
    // An unmapped button (b6) gives nothing.
    assert!(frame(&mut p, 40, &[key(0x126, 1)]).is_empty());
}

#[test]
fn hat_right_twice_and_held_with_repeat() {
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 1000, &[(EV_ABS, ABS_HAT0X, 1)]), vec![RIGHT]);
    assert!(frame(&mut p, 1100, &[(EV_ABS, ABS_HAT0X, 0)]).is_empty());
    assert_eq!(p.due(), None);
    // The second press, held: once at once, again after 400 ms, then every 120 ms.
    assert_eq!(frame(&mut p, 2000, &[(EV_ABS, ABS_HAT0X, 1)]), vec![RIGHT]);
    assert_eq!(p.due(), Some(2400));
    assert!(tick(&mut p, 2399).is_empty());
    assert_eq!(tick(&mut p, 2400), vec![RIGHT]);
    assert!(tick(&mut p, 2519).is_empty());
    assert_eq!(tick(&mut p, 2520), vec![RIGHT]);
    // A late tick repeats once, not in a burst.
    assert_eq!(tick(&mut p, 3000), vec![RIGHT]);
    assert_eq!(p.due(), Some(3120));
    assert!(frame(&mut p, 3050, &[(EV_ABS, ABS_HAT0X, 0)]).is_empty());
    assert!(tick(&mut p, 4000).is_empty());
    // Up on the hat is -1.
    assert_eq!(frame(&mut p, 5000, &[(EV_ABS, ABS_HAT0Y, -1)]), vec![Press::Move(Dir::Up)]);
}

#[test]
fn select_and_start_held_500_ms_is_home_499_is_not() {
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 0, &[key(0x128, 1), key(0x129, 1)]), vec![Press::Btn(Btn::Select), Press::Btn(Btn::Start)]);
    assert_eq!(p.due(), Some(500));
    assert!(tick(&mut p, 499).is_empty(), "499 ms is not Home");
    assert_eq!(tick(&mut p, 500), vec![Press::Btn(Btn::Home)]);
    assert!(tick(&mut p, 2000).is_empty(), "one Home per hold");
    assert_eq!(p.due(), None);
    // Let go at 499 ms: nothing.
    frame(&mut p, 2100, &[key(0x128, 0), key(0x129, 0)]);
    frame(&mut p, 3000, &[key(0x129, 1)]);
    frame(&mut p, 3100, &[key(0x128, 1)]);
    assert_eq!(p.due(), Some(3600), "the hold counts from the second button");
    assert!(tick(&mut p, 3599).is_empty());
    frame(&mut p, 3599, &[key(0x128, 0)]);
    assert!(tick(&mut p, 4000).is_empty(), "released at 499 ms");
}

#[test]
fn a_standard_pad_has_its_own_home_and_dpad_buttons() {
    let keys = [BTN_SOUTH, BTN_EAST, BTN_NORTH, BTN_WEST, BTN_TL, BTN_TR, BTN_SELECT, BTN_START, BTN_MODE, BTN_THUMBR, BTN_DPAD_UP, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT];
    let axis = Axis { min: -32768, max: 32767, flat: 128 };
    let mut p = Pad::new(mapping(0x045e, 0x028e, &keys), Some(axis), Some(axis));
    assert_eq!(p.map.source, Source::Standard);
    assert!(!p.map.chord());
    assert_eq!(frame(&mut p, 0, &[key(BTN_SOUTH, 1)]), vec![Press::Btn(Btn::A)]);
    assert_eq!(frame(&mut p, 10, &[key(BTN_NORTH, 1), key(BTN_WEST, 1)]), vec![Press::Btn(Btn::X), Press::Btn(Btn::Y)]);
    assert_eq!(frame(&mut p, 20, &[key(BTN_MODE, 1)]), vec![Press::Btn(Btn::Home)]);
    assert_eq!(frame(&mut p, 30, &[key(BTN_SELECT, 1), key(BTN_START, 1)]), vec![Press::Btn(Btn::Select), Press::Btn(Btn::Start)]);
    assert!(tick(&mut p, 1000).is_empty(), "no chord on a pad with Home");
    assert_eq!(frame(&mut p, 1100, &[key(BTN_DPAD_DOWN, 1)]), vec![Press::Move(Dir::Down)]);
    assert!(frame(&mut p, 1200, &[key(BTN_DPAD_DOWN, 0)]).is_empty());
    // Sony's left face button is BTN_WEST.
    let s = mapping(0x054c, 0x09cc, &keys);
    assert_eq!((s.of(BTN_WEST), s.of(BTN_NORTH)), (Some(Btn::X), Some(Btn::Y)));
}

#[test]
fn stick_dead_zone_threshold_and_diagonal() {
    let mut p = dragonrise();
    let at = |f: f64| (127.5 + f * 127.5).round() as i32;
    assert!(frame(&mut p, 0, &[(EV_ABS, ABS_X, at(0.2))]).is_empty(), "20 % is inside the dead zone");
    assert!(frame(&mut p, 10, &[(EV_ABS, ABS_X, at(0.45))]).is_empty(), "45 % is not a direction yet");
    assert_eq!(frame(&mut p, 20, &[(EV_ABS, ABS_X, at(0.6))]), vec![RIGHT]);
    // Back to 40 %: still held (outside the dead zone), so no second press; it repeats on.
    assert!(frame(&mut p, 30, &[(EV_ABS, ABS_X, at(0.4))]).is_empty());
    assert_eq!(tick(&mut p, 420), vec![RIGHT]);
    assert!(frame(&mut p, 500, &[(EV_ABS, ABS_X, at(0.2))]).is_empty());
    assert_eq!(p.due(), None, "in the dead zone it lets go");
    // A diagonal: the stronger axis wins.
    assert_eq!(frame(&mut p, 1000, &[(EV_ABS, ABS_X, at(0.8)), (EV_ABS, ABS_Y, at(0.6))]), vec![RIGHT]);
    assert_eq!(frame(&mut p, 1010, &[(EV_ABS, ABS_X, at(0.6)), (EV_ABS, ABS_Y, at(0.9))]), vec![Press::Move(Dir::Down)]);
    assert_eq!(frame(&mut p, 1020, &[(EV_ABS, ABS_X, at(-0.9)), (EV_ABS, ABS_Y, at(-0.3))]), vec![Press::Move(Dir::Left)]);
    // The D-pad wins over the stick.
    assert_eq!(frame(&mut p, 1030, &[(EV_ABS, ABS_HAT0Y, -1)]), vec![Press::Move(Dir::Up)]);
    // A wide flat makes the dead zone wider: 40 % of a stick with flat 60 % lets go.
    let wide = Axis { min: 0, max: 255, flat: 77 };
    let mut q = Pad::new(p.map.clone(), Some(wide), Some(wide));
    assert_eq!(frame(&mut q, 0, &[(EV_ABS, ABS_X, at(0.7))]), vec![RIGHT]);
    frame(&mut q, 10, &[(EV_ABS, ABS_X, at(0.55))]);
    assert!(q.due().is_some());
    frame(&mut q, 20, &[(EV_ABS, ABS_X, at(0.5))]);
    assert_eq!(q.due(), None);
}

#[test]
fn an_unknown_stick_with_two_buttons_gets_the_fallback() {
    let mut p = Pad::new(mapping(0x1234, 0x5678, &[0x121, 0x120]), Some(Axis { min: -127, max: 127, flat: 0 }), Some(Axis { min: -127, max: 127, flat: 0 }));
    assert_eq!(p.map.source, Source::Fallback);
    assert_eq!(p.map.buttons, vec![(0x120, Btn::A), (0x121, Btn::B)]);
    assert!(!p.map.chord(), "no Select/Start: no Home");
    assert_eq!(frame(&mut p, 0, &[key(0x120, 1)]), vec![Press::Btn(Btn::A)]);
    assert_eq!(frame(&mut p, 10, &[(EV_ABS, ABS_Y, -100)]), vec![Press::Move(Dir::Up)]);
    // The first four buttons in SDL's order: the joystick block first, then BTN_MISC.
    let m = mapping(0x1234, 0x5678, &[0x100, 0x12a, 0x120, 0x101, 0x125]);
    assert_eq!(m.buttons, vec![(0x120, Btn::A), (0x125, Btn::B), (0x12a, Btn::X), (0x100, Btn::Y)]);
}

#[test]
fn unplugged_or_dropped_mid_repeat_stops() {
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 0, &[(EV_ABS, ABS_HAT0X, 1)]), vec![RIGHT]);
    assert_eq!(tick(&mut p, 400), vec![RIGHT]);
    p.reset();
    assert_eq!(p.due(), None);
    assert!(tick(&mut p, 520).is_empty());
    assert!(tick(&mut p, 5000).is_empty());
    // SYN_DROPPED: let go, the rest of that frame is ignored.
    frame(&mut p, 6000, &[key(0x128, 1), key(0x129, 1)]);
    let mut out = Vec::new();
    p.feed(EV_SYN, SYN_DROPPED, 0, 6100, &mut out);
    p.feed(EV_ABS, ABS_HAT0X, 1, 6100, &mut out);
    p.feed(EV_SYN, SYN_REPORT, 0, 6100, &mut out);
    assert!(out.is_empty());
    assert_eq!(p.due(), None, "the chord went with the drop");
    assert!(tick(&mut p, 7000).is_empty());
}

#[test]
fn an_unknown_pad_with_ten_buttons_gets_select_start_as_home() {
    let keys: Vec<u16> = (0x120..=0x12b).collect();
    let mut p = Pad::new(mapping(0x1234, 0x0001, &keys), Some(STICK), Some(STICK));
    assert_eq!(p.map.source, Source::Fallback);
    assert_eq!((p.map.of(0x128), p.map.of(0x129)), (Some(Btn::Select), Some(Btn::Start)));
    assert!(p.map.chord());
    frame(&mut p, 0, &[key(0x128, 1), key(0x129, 1)]);
    assert_eq!(tick(&mut p, 500), vec![Press::Btn(Btn::Home)]);
    // Nine buttons: no guess, no Home.
    let nine = mapping(0x1234, 0x0001, &keys[..9]);
    assert!(!nine.chord() && !nine.has(Btn::Select) && !nine.has(Btn::Start));
}

#[test]
fn hostile_axes_and_hat_values_never_panic() {
    let map = dragonrise().map;
    for a in [Axis { min: 0, max: 0, flat: 0 }, Axis { min: 10, max: -10, flat: -5 }, Axis { min: i32::MIN, max: i32::MAX, flat: i32::MAX }] {
        let mut p = Pad::new(map.clone(), Some(a), Some(a));
        for v in [i32::MIN, -1, 0, 1, i32::MAX] {
            let out = frame(&mut p, 0, &[(EV_ABS, ABS_X, v), (EV_ABS, ABS_Y, v)]);
            if a.max <= a.min {
                assert!(out.is_empty(), "{a:?} {v}: {out:?}");
            }
        }
        tick(&mut p, u64::MAX);
    }
    // Hat values other than -1/0/1 count by their sign.
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 0, &[(EV_ABS, ABS_HAT0X, 5)]), vec![RIGHT]);
    assert_eq!(frame(&mut p, 10, &[(EV_ABS, ABS_HAT0X, -7)]), vec![Press::Move(Dir::Left)]);
}

#[test]
fn a_quiet_pad_stops_repeating_but_keeps_its_chord() {
    let mut p = dragonrise();
    assert_eq!(frame(&mut p, 0, &[(EV_ABS, ABS_HAT0X, 1), key(0x128, 1), key(0x129, 1)]).last(), Some(&RIGHT));
    assert_eq!(p.due(), Some(400));
    assert_eq!(p.chord_due(), Some(500));
    p.quiet();
    assert_eq!(p.due(), Some(500), "only the chord is left");
    assert_eq!(tick(&mut p, 500), vec![Press::Btn(Btn::Home)]);
    assert!(tick(&mut p, 5000).is_empty(), "the quiet hold does not repeat");
    // A new direction arms the repeat again.
    assert_eq!(frame(&mut p, 6000, &[(EV_ABS, ABS_HAT0X, -1)]), vec![Press::Move(Dir::Left)]);
    assert_eq!(p.due(), Some(6400));
}

#[test]
fn a_stick_deflected_when_opened_is_not_resting() {
    let mut p = dragonrise();
    p.seed(Some(127), Some(250), (0, 0));
    // Only X moves; Y was down all along.
    assert_eq!(frame(&mut p, 0, &[(EV_ABS, ABS_X, 128)]), vec![Press::Move(Dir::Down)]);
    let mut q = dragonrise();
    q.seed(None, None, (3, 0));
    assert_eq!(frame(&mut q, 0, &[key(0x126, 1)]), vec![RIGHT], "the hat held at open counts at the first report");
}
