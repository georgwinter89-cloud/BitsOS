//! DS-5: the boot scene's model and picture. The pins are pixels of the kernel's own picture of `KERNEL`
//! (crates/console tests_boot.rs `bootscreen_matches_model` with BITSOS_BOOT_PPM, 28.10.); host/tests/ds/60-boot.sh
//! compares the whole picture.

use std::sync::OnceLock;
use std::time::Duration;

use super::boot::eligible;
use super::boot_art::Frame;
use super::boot_draw::{Art, HINT, RING_CENTRE, Scene, State, View, px, row_top, rows_shown};
use super::boot_model::{Own, Steps, label, own, seconds};

const KERNEL: &str = "bitsos-boot 1\nseq 14\ndrawn 14\nscreen handed\ngeneration 49\n\
step kernel done 120 version 0.1.0 · generation 49\nstep memory done 140 31 gb frei\nstep cpus done 280 16 kerne\n\
step storage done 450 stick sda\nstep network running 900 igc0 verbindet\nstep sound done 330 hda0\n\
step gpu skipped 500 nur gop-bild\nstep services running 700 init startet\nstep mcp done 520 port 8765\n\
step desktop running 5000 /bin/bitscomp startet (versuch 1)\n";

fn art() -> &'static Art {
    static ART: OnceLock<Art> = OnceLock::new();
    ART.get_or_init(Art::new)
}

fn view(text: &str) -> View {
    Steps::parse(text).unwrap().view(art())
}

fn at(px: &[u32], w: i32, x: i32, y: i32) -> u32 {
    px[(y * w + x) as usize]
}

#[test]
fn the_file_is_read_as_the_kernel_writes_it() {
    let s = Steps::parse(KERNEL).unwrap();
    assert_eq!((s.seq, s.drawn, s.screen.as_str(), s.generation, s.steps.len()), (14, 14, "handed", 49, 10));
    let d = s.get("desktop").unwrap();
    assert_eq!((d.state, d.t_ms, d.detail.as_str()), (State::Running, 5000, "/bin/bitscomp startet (versuch 1)"));
    assert_eq!(s.get("kernel").unwrap().detail, "version 0.1.0 · generation 49");
    assert_eq!(s.progress(), (7, 10));
    assert_eq!(s.status(), "Startet: netzwerk");
    assert_eq!(label("desktop"), "arbeitsfläche");
    assert_eq!(label("future"), "future", "an id a newer kernel adds shows itself");
}

#[test]
fn an_odd_file_is_no_scene_and_no_panic() {
    for t in [
        "",
        "\n",
        "bitsos-boot 2\nstep kernel done 1 x\n",
        "bitsos-boot 1\n",
        "bitsos-boot 1\nstep kernel\n",
        "bitsos-boot 1\nstep kernel sleeping 1 x\n",
        "bitsos-boot 1\nstep kernel done -5 x\n",
        "bitsos-boot 1\nseq x\nstep kernel done 1 x\n",
        "\u{0}\u{1}garbage",
        "bitsos-boot 1\nstep kernel done 99999999999999999999999 x\n",
    ] {
        assert_eq!(Steps::parse(t), None, "{t:?}");
    }
    // Control characters in a detail never reach the picture; unknown lines are skipped.
    let s = Steps::parse("bitsos-boot 1\nnew-field 3\nstep kernel done 1 a\u{1b}[2Jb\n").unwrap();
    assert_eq!(s.steps[0].detail, "a[2Jb");
}

#[test]
fn only_this_starts_handover_shows_the_scene() {
    // This bitscomp started at 5 050 ms: the kernel marked the step running at its spawn (5 000), done at the take.
    let start = 5_050;
    let s = Steps::parse(KERNEL).unwrap();
    assert_eq!(eligible(&s, start), Ok(()));
    let done = Steps::parse(&KERNEL.replace("desktop running 5000", "desktop done 5100")).unwrap();
    assert_eq!(eligible(&done, start), Ok(()));
    for (from, to) in [("screen handed", "screen text"), ("screen handed", "screen off"), ("desktop running 5000", "desktop failed 5000")] {
        let s = Steps::parse(&KERNEL.replace(from, to)).unwrap();
        assert!(eligible(&s, start).is_err(), "{to}");
    }
    // A second bitscomp a minute later: the kernel still says handed, but the desktop step is the first one's.
    assert!(eligible(&s, start + 60_000).is_err());
    // A restart soon after the first handover (checker DS-5): the step is done before this start, no replay.
    assert!(eligible(&done, 5_100 + 2_000).is_err());
    assert!(eligible(&done, 5_100 + 1_000).is_err());
    // INT-W27: the take and the kernel's poll come before App::new: done 300 ms before the start is still ours.
    assert_eq!(eligible(&done, 5_100 + 300), Ok(()));
    let no_desktop = Steps::parse(&KERNEL.replace("step desktop running 5000 /bin/bitscomp startet (versuch 1)\n", "")).unwrap();
    assert!(eligible(&no_desktop, start).is_err());
}

#[test]
fn a_key_skips_once_the_shell_is_ready_and_without_a_fade() {
    use super::boot::{ends, fades, key_seen};
    assert!(key_seen(4, 3) && !key_seen(3, 3));
    // A key before settings and status: noted, the scene stays until they are there (or the cap).
    assert_eq!(ends(true, Own::Services, false), None);
    assert_eq!(ends(true, Own::Window, false), Some("key"));
    assert_eq!(ends(true, Own::Services, true), Some("key"));
    assert_eq!(ends(true, Own::Ready, false), Some("key"));
    // Without a key: ready, or the cap; never before.
    assert_eq!(ends(false, Own::Services, false), None);
    assert_eq!(ends(false, Own::Window, false), None);
    assert_eq!(ends(false, Own::Ready, false), Some("ready"));
    assert_eq!(ends(false, Own::Window, true), Some("cap (no window yet)"));
    assert_eq!(ends(false, Own::Services, true), Some("cap (settings or status missing)"));
    // A key cuts to the desktop (transition 0), so does motion off; otherwise the cheap fade.
    assert!(!fades("key", false) && !fades("ready", true) && fades("ready", false) && fades("cap (no window yet)", false));
}

#[test]
fn the_desktop_waits_for_its_own_part_and_the_first_window_only() {
    assert_eq!(own(false, true, true), Own::Services);
    assert_eq!(own(true, false, true), Own::Services);
    assert_eq!(own(true, true, false), Own::Window);
    assert_eq!(own(true, true, true), Own::Ready);
    let s = Steps::parse(KERNEL).unwrap();
    let mine = s.with_desktop(Some(&Own::Window.step("foot", 5300)));
    let d = mine.get("desktop").unwrap();
    assert_eq!((d.state, d.detail.as_str(), d.t_ms), (State::Running, "bitscomp: erstes fenster (foot)", 5300));
    let ready = s.with_desktop(Some(&Own::Ready.step("", 5400)));
    assert_eq!(ready.get("desktop").unwrap().state, State::Done);
    assert_eq!(ready.progress(), (8, 10));
    // The fade is the cheap level's transition (at most 400 ms), the cap ends a wait that does not end.
    assert!(super::boot::FADE <= Duration::from_millis(400));
    assert!(super::boot::CAP <= Duration::from_secs(2) && super::boot::POLL <= Duration::from_millis(100));
    assert!(crate::anim::START <= Duration::from_millis(400));
}

#[test]
fn the_layout_is_the_kernels() {
    assert_eq!(RING_CENTRE, (1280, 507));
    assert_eq!(row_top(0), 1093);
    assert_eq!(row_top(9), px(1036));
    assert_eq!(rows_shown(), 10);
    assert_eq!(seconds(2404), "2,40 s");
    assert!(Scene::new(1920, 1080).is_none() && Scene::new(2559, 1440).is_none());
    let big = Scene::new(3840, 2160).unwrap();
    assert_eq!((big.ox, big.oy), (640, 360));
}

#[test]
fn the_picture_is_the_kernels_pixel_for_pixel() {
    let s = Scene::new(2560, 1440).unwrap();
    let px = s.picture(&view(KERNEL), art());
    // (x, y, the kernel's pixel): core dot, scanline row and the row under it, a grid dot, corners (vignette), the
    // dashed ring at 3 o'clock, the disc's fill, a title stroke, row 0's label, row 4's busy mark, the progress
    // count, the status line, the header's live dot.
    let pins = [
        (1280, 507, 0x7fcfa0),
        (3, 1200, 0x0f1412),
        (3, 1201, 0x070c0a),
        (15, 15, 0x0b100d),
        (0, 0, 0x0e1210),
        (2559, 1439, 0x060a08),
        (1557, 507, 0x7fcfa0),
        (1280, 1000, 0x1c2721),
        (100, 100, 0x0f1411),
        (1194, 900, 0xc9d0cb),
        (383, 1100, 0xe8eee9),
        (362, 1196, 0xe8eee9),
        (2398, 1118, 0x7ecd9e),
        (1183, 1020, 0x87928b),
        (67, 58, 0x7fcfa0),
    ];
    let bad: Vec<_> =
        pins.iter().filter(|&&(x, y, want)| at(&px, 2560, x, y) != want).map(|&(x, y, want)| (x, y, at(&px, 2560, x, y), want)).collect();
    assert!(bad.is_empty(), "(x, y, got, kernel's): {bad:x?}");
    // Drawn in bands on threads == drawn in one piece.
    let mut one = Frame::new(s.full());
    s.draw(&mut one, &view(KERNEL), art());
    assert!(one.px == px, "the bands differ from the whole");
}

#[test]
fn a_changed_row_is_drawn_again_as_the_whole_picture_shows_it() {
    let s = Scene::new(2560, 1440).unwrap();
    let (old, new) = (view(KERNEL), view(&KERNEL.replace("network running 900 igc0 verbindet", "network done 1400 igc0 10.0.0.7/24")));
    let changed = s.changes(&old, &new);
    // Row 4, the status line ("Startet: dienste") and the count.
    assert_eq!(changed.len(), 3, "{changed:?}");
    let whole = s.picture(&new, art());
    for r in changed {
        let mut f = Frame::new(r);
        s.draw(&mut f, &new, art());
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                assert_eq!(f.px[((y - r.y) * r.w + x - r.x) as usize], at(&whole, 2560, x, y), "({x}, {y}) in {r:?}");
            }
        }
    }
    assert!(s.changes(&new, &new).is_empty());
}

#[test]
fn rows_say_what_they_wait_for_and_long_details_are_cut() {
    let long = "x".repeat(200);
    let v = view(&KERNEL.replace("init startet", &long).replace("network running 900 igc0 verbindet", "network waiting 0 egal"));
    let net = &v.rows[4];
    assert_eq!((net.detail.as_str(), net.time.as_str()), ("wartet", ""));
    let svc = &v.rows[7];
    assert!(svc.detail.ends_with('…') && svc.detail.chars().count() < 200, "{}", svc.detail);
    assert!(art().mono.width(&svc.detail, 0) <= art().detail_max(&svc.time));
    assert_eq!(art().detail_max("2,40 s"), art().detail_max("00,00 s"), "below 100 s the kernel's cut");
    // A time of 100 s or more never runs into the detail (checker DS-5: "166621,31 s" overlapped).
    let late = view(&KERNEL.replace("init startet", &long).replace("services running 700", "services running 166621310"));
    let (d, t) = (&late.rows[7].detail, &late.rows[7].time);
    assert_eq!(t, "166621,31 s");
    let mono = &art().mono;
    assert!(px(156) + mono.width(d, 0) + px(12) <= super::boot_draw::ROW_W - mono.width(t, 0), "{d} / {t}");
    assert_eq!(v.header, "GENERATION 49");
    assert_eq!(view(&KERNEL.replace("generation 49\n", "generation 0\n")).header, "BITSOS V2");
    // The hint at the bottom says what a key does here.
    assert!(HINT.contains("ARBEITSFLÄCHE"));
}
