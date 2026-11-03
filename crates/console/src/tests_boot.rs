//! DS-4 host tests: the step model from real log lines, `/proc/bitsos/boot`, `desktop=`, and the picture (the
//! same drawing code the kernel runs; `bootscreen_matches_model` compares a QEMU screendump with it).

extern crate std;

use crate::bootart;
use crate::bootdraw::{self, Scene, View, px};
use crate::bootscreen::resolve;
use crate::bootsteps::{Init, Key, KEYS, State, Steps};
use alloc::string::String;
use alloc::vec::Vec;
use display::RamFrame;

fn line(t_ms: u64, level: char, subsystem: &str, name: &str, fields: &str) -> String {
    alloc::format!("[{:5}.{:06}] {level} {subsystem} {name} {fields}", t_ms / 1000, (t_ms % 1000) * 1000)
}

/// A start as the Mini-PC logs it (field names as the drivers write them).
fn mini_pc_start() -> Vec<String> {
    alloc::vec![
        line(12, 'I', "kernel", "start", "version=0.1.0 generation=49 reload=0"),
        line(40, 'I', "mm", "frames", "total_mb=31744 free_mb=30876 nx=true"),
        line(95, 'I', "smp", "prepared", "trampoline=0x8000"),
        line(180, 'I', "smp", "done", "cpus=16 planned=16 up=15 failed=0 total_ms=80"),
        line(260, 'I', "usb", "xhci.up", "ctrl=xhci0 version=0x120"),
        line(300, 'I', "net", "igc.up", "ifname=igc0 model=I226-V mac=aa:bb"),
        line(320, 'I', "snd", "hda.probe", "function=0000:c4:00.6 kind=hda"),
        line(330, 'I', "snd", "hda.output", "ctrl=hda0 cad=0 pin=0x14"),
        line(410, 'I', "block", "disk.stick", "disk=sda stick=yes"),
        line(450, 'I', "fat", "stick.mounted", "disk=sda start_lba=2048 sectors=1"),
        line(520, 'I', "mcp", "server.start", "port=8765 conns=3 tools=40"),
        line(530, 'I', "kernel", "ready", ""),
        line(600, 'I', "ul", "init.start", "pid=1 path=/bin/init start=1"),
        line(900, 'I', "net", "link.up", "ifname=igc0 speed_mbps=1000 full_duplex=true"),
        line(1500, 'I', "net", "dhcp.lease", "ifname=igc0 ip=10.0.0.7 prefix=24"),
    ]
}

fn fed(lines: &[String]) -> Steps {
    let mut s = Steps::new();
    for l in lines {
        s.feed(l);
    }
    s
}

#[test]
fn steps_follow_the_real_start() {
    let s = fed(&mini_pc_start());
    assert_eq!(s.generation, 49);
    let want = [
        (Key::Kernel, State::Done, "version 0.1.0", 12),
        (Key::Memory, State::Done, "30,1 GB frei von 31,0 GB", 40),
        (Key::Cpus, State::Done, "16 kerne bereit", 180),
        (Key::Storage, State::Done, "stick sda eingehängt", 450),
        (Key::Network, State::Done, "igc0 10.0.0.7", 1500),
        (Key::Sound, State::Done, "hda ausgang bereit", 330),
        (Key::Gpu, State::Skipped, "nur gop-bild", 530),
        (Key::Services, State::Done, "init läuft (pid 1)", 600),
        (Key::Mcp, State::Done, "port 8765", 520),
        (Key::Desktop, State::Waiting, "", 0),
    ];
    for (k, st, d, t) in want {
        let got = s.get(k);
        assert_eq!((got.state, got.detail.as_str(), got.t_ms), (st, d, t), "{k:?}");
    }
    assert_eq!(s.init, Init::Running);
    assert_eq!(s.progress(), (9, 10));
    assert_eq!(s.status(), "Startet …");
}

#[test]
fn failures_show_with_their_reason_and_a_later_success_wins() {
    let mut s = Steps::new();
    s.feed(&line(300, 'E', "net", "igc.probe_failed", "function=0000:01:00.0 error=Timeout"));
    assert_eq!(s.get(Key::Network).state, State::Failed);
    assert_eq!(s.get(Key::Network).detail, "igc.probe_failed");
    // A running card later does not hide the failure ...
    s.feed(&line(400, 'I', "wlan", "netdev.registered", "ifname=wlan0"));
    assert_eq!(s.get(Key::Network).state, State::Failed);
    // ... an address does.
    s.feed(&line(900, 'I', "net", "dhcp.lease", "ifname=wlan0 ip=10.0.0.9 prefix=24"));
    assert_eq!((s.get(Key::Network).state, s.get(Key::Network).detail.as_str()), (State::Done, "wlan0 10.0.0.9"));
    // A failure after success keeps the success.
    s.feed(&line(950, 'E', "net", "igc.failed", "ifname=igc0"));
    assert_eq!(s.get(Key::Network).state, State::Done);
    // CPUs that do not answer, the desktop giving up.
    s.feed(&line(180, 'I', "smp", "done", "cpus=14 planned=16 up=13 failed=2"));
    assert_eq!((s.get(Key::Cpus).state, s.get(Key::Cpus).detail.as_str()), (State::Failed, "14 laufen, 2 antworten nicht"));
    s.feed(&line(2000, 'I', "console", "desktop.start", "program=/bin/bitscomp attempt=1"));
    assert_eq!(s.get(Key::Desktop).detail, "/bin/bitscomp startet (versuch 1)");
    s.feed(&line(2100, 'W', "console", "desktop.failed", "program=/bin/bitscomp attempt=1 reason=exit_code"));
    assert_eq!(s.get(Key::Desktop).state, State::Running);
    s.feed(&line(5000, 'E', "console", "desktop.given_up", "attempts=3 reason=not_found"));
    assert_eq!((s.get(Key::Desktop).state, s.get(Key::Desktop).detail.as_str()), (State::Failed, "3 versuche: nicht gefunden"));
    assert!(s.status().starts_with("Startet"), "{}", s.status());
}

#[test]
fn kernel_ready_settles_what_never_spoke_and_init_off_is_seen() {
    let mut s = Steps::new();
    s.feed(&line(12, 'I', "kernel", "start", "version=0.1.0 generation=0"));
    let seq = s.seq;
    s.feed(&line(530, 'I', "kernel", "ready", ""));
    assert!(s.seq > seq && s.kernel_ready);
    for k in [Key::Cpus, Key::Storage, Key::Network, Key::Sound, Key::Gpu, Key::Mcp] {
        assert_eq!(s.get(k).state, State::Skipped, "{k:?}");
    }
    assert_eq!(s.get(Key::Services).state, State::Running);
    s.feed(&line(531, 'I', "ul", "init.off", "| CONFIG init=off: no user program is started"));
    assert_eq!((s.init, s.get(Key::Services).state), (Init::Off, State::Skipped));
    // Lines that are not steps change nothing; neither do broken ones.
    let seq = s.seq;
    assert!(!s.feed(&line(600, 'I', "net", "tcp.listen", "port=80")));
    assert!(!s.feed("[... 3 records lost]"));
    assert!(!s.feed("garbage"));
    assert_eq!(s.seq, seq);
}

#[test]
fn a_finished_start_says_so() {
    let mut s = fed(&mini_pc_start());
    s.feed(&line(2400, 'I', "console", "desktop.up", "program=/bin/bitscomp pid=40"));
    assert_eq!(s.progress(), (10, 10));
    assert_eq!(s.status(), "Bereit nach 2,40 s.");
    s.feed(&line(2500, 'E', "snd", "hda.bring_up_failed", "ctrl=hda0"));
    // Sound was done: the later failure does not hide it.
    assert_eq!(s.status(), "Bereit nach 2,40 s.");
}

#[test]
fn proc_text_round_trips() {
    let mut s = fed(&mini_pc_start());
    s.feed(&line(2000, 'I', "console", "desktop.start", "program=/bin/bitscomp attempt=2"));
    let t = s.proc_text("shown", s.seq);
    assert!(t.starts_with("bitsos-boot 1\n"), "{t}");
    assert!(t.contains("\nstep desktop running 2000 /bin/bitscomp startet (versuch 2)\n"), "{t}");
    assert!(t.contains("\nscreen shown\n") && t.contains("\ngeneration 49\n"), "{t}");
    let back = Steps::from_proc(&t).expect("parses");
    assert_eq!(back.steps, s.steps);
    assert_eq!((back.generation, back.seq), (s.generation, s.seq));
}

#[test]
fn desktop_mode_from_config() {
    assert_eq!(resolve(None, false), Ok((true, "auto")));
    assert_eq!(resolve(Some("auto"), true), Ok((false, "auto, hypervisor")));
    assert_eq!(resolve(Some(" on "), true), Ok((true, "on")));
    assert_eq!(resolve(Some("off"), false), Ok((false, "off")));
    assert_eq!(resolve(Some("yes"), false), Err(()));
}

#[test]
fn atlases_have_german_text() {
    assert!(bootart::ready(), "fonts missing at build time");
    for face in [&bootart::MONO, &bootart::TEXT, &bootart::LABEL] {
        for c in "äöüÄÖÜß°·…–".chars() {
            let g = face.glyph(c).expect("glyph");
            assert_eq!(char::from_u32(g.ch), Some(c), "{c}");
        }
        // Not in the atlas: drawn as '?'.
        assert_eq!(face.glyph('€').map(|g| g.ch), Some('?' as u32));
    }
    // JetBrains Mono at 13 px x 4/3: 0.6 em per character.
    let w = bootart::MONO.width("0123456789", 0);
    assert!((100..=110).contains(&w), "{w}");
    let (cut, more) = bootart::MONO.fit("eine sehr lange zeile die nicht passt", 120, 0);
    assert!(more && bootart::MONO.width(cut, 0) + bootart::MONO.width("…", 0) <= 120, "{cut}");
    assert!(bootart::TITLE.w > 200 && bootart::TITLE.h > 90, "{}x{}", bootart::TITLE.w, bootart::TITLE.h);
}

fn picture(s: &Steps, w: u32, h: u32) -> RamFrame {
    let mut f = RamFrame::new(w, h);
    let scene = Scene::new(w, h).expect("fits");
    scene.draw_all(&mut f, &View::of(s));
    f
}

fn pixel(f: &RamFrame, x: u32, y: u32) -> u32 {
    f.pixels()[(y * f.width + x) as usize]
}

/// The picture as a PPM (for looking at it: BITSOS_BOOT_PPM=<dir> cargo test -p console tests_boot).
fn save(f: &RamFrame, name: &str) {
    let Ok(dir) = std::env::var("BITSOS_BOOT_PPM") else { return };
    let mut out = alloc::format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
    for p in f.pixels() {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(alloc::format!("{dir}/{name}.ppm"), out).unwrap();
}

#[test]
fn the_picture_has_the_designs_parts() {
    let mut lines = mini_pc_start();
    lines.truncate(9);
    lines.push(line(300, 'E', "nvme", "probe_failed", "function=0000:02:00.0"));
    let s = fed(&lines);
    let f = picture(&s, 2560, 1440);
    save(&f, "boot-mid");
    // Background: dark green-grey (vignette darker in the corner), the green glow brighter above the middle.
    let corner = pixel(&f, 2, 2);
    let mid = pixel(&f, 1280, 900);
    let g = |p: u32| (p >> 8) & 0xFF;
    assert!(g(corner) <= g(bootdraw::SUNKEN) && g(mid) > g(corner), "{corner:06x} {mid:06x}");
    // The core dot at the ring's centre is the accent.
    let core = pixel(&f, bootdraw::RING_CENTRE.0 as u32, bootdraw::RING_CENTRE.1 as u32);
    assert!(g(core) > 0xB0, "{core:06x}");
    // Every row has drawn text (pixels brighter than the background) where its label stands.
    for i in 0..KEYS.len() {
        let (top, end) = (bootdraw::row_top(i) as u32, bootdraw::row_top(i + 1) as u32);
        let lit = (top..end).flat_map(|y| (176..330).map(move |x| (x, y))).filter(|&(x, y)| g(pixel(&f, x, y)) > 0x60).count();
        assert!(lit > 20, "row {i}: {lit}");
    }
    // The failed step's detail is in the warning colour (red over green).
    let (top, end) = (bootdraw::row_top(3) as u32, bootdraw::row_top(4) as u32);
    let red = (top..end).flat_map(|y| (336..700).map(move |x| (x, y))).filter(|&(x, y)| (pixel(&f, x, y) >> 16) & 0xFF > 0xC0).count();
    assert!(red > 20, "{red}");
}

#[test]
fn a_change_redraws_only_its_row() {
    let lines = mini_pc_start();
    let before = fed(&lines[..10]);
    let mut after = before.clone();
    after.feed(&lines[10]);
    let scene = Scene::new(2560, 1440).unwrap();
    let mut f = picture(&before, 2560, 1440);
    let _ = f.take_damage();
    let (a, b) = (View::of(&before), View::of(&after));
    scene.draw_changes(&mut f, &a, &b);
    let mcp_top = bootdraw::row_top(8) as u32;
    let d = f.damage_rects().to_vec();
    assert!(!d.is_empty());
    // The MCP row and the progress block (one more step done), nothing of the ring or title.
    for r in &d {
        assert!(r.y >= 1000, "{r:?}");
    }
    assert!(d.iter().any(|r| r.y == mcp_top), "{d:?}");
    // The same picture as drawing everything anew.
    let whole = picture(&after, 2560, 1440);
    assert!(f.pixels() == whole.pixels(), "partial redraw differs from a full one");
}

#[test]
fn larger_screens_centre_the_scene_and_smaller_ones_keep_the_text_console() {
    assert!(Scene::new(1920, 1080).is_none());
    assert!(Scene::new(2559, 1440).is_none());
    let s = fed(&mini_pc_start());
    let f = picture(&s, 3840, 2160);
    let a = picture(&s, 2560, 1440);
    let (ox, oy) = (640, 360);
    for (x, y) in [(10u32, 10u32), (1280, bootdraw::RING_CENTRE.1 as u32), (300, 900), (2000, 1100)] {
        assert_eq!(pixel(&f, x + ox, y + oy), pixel(&a, x, y), "({x},{y})");
    }
    assert_eq!(pixel(&f, 5, 5), bootdraw::SUNKEN);
}

/// QEMU's screendump while the boot screen is up equals this model drawn from `/proc/bitsos/boot` read at the
/// same moment (host/tests/in/50-boot-screen.sh: BITSOS_SCREENDUMP, BITSOS_BOOT_PROC).
#[test]
#[ignore]
fn bootscreen_matches_model() {
    let ppm = std::fs::read(std::env::var("BITSOS_SCREENDUMP").expect("BITSOS_SCREENDUMP")).expect("read the PPM");
    let text = std::fs::read_to_string(std::env::var("BITSOS_BOOT_PROC").expect("BITSOS_BOOT_PROC")).expect("read the steps");
    let steps = Steps::from_proc(&text).expect("steps");
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !ppm[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&ppm[start..i]).into_owned());
    }
    i += 1;
    let (w, h): (u32, u32) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let rgb = &ppm[i..];
    let want = picture(&steps, w, h);
    save(&want, "boot-model");
    let mut bad = 0u64;
    let mut first = None;
    for (n, p) in want.pixels().iter().enumerate() {
        let got = ((rgb[n * 3] as u32) << 16) | ((rgb[n * 3 + 1] as u32) << 8) | rgb[n * 3 + 2] as u32;
        if got != *p {
            bad += 1;
            first.get_or_insert((n as u32 % w, n as u32 / w, got, *p));
        }
    }
    assert_eq!(bad, 0, "{bad} pixels differ, first (x, y, got, want) = {first:x?}");
}

#[test]
fn the_head_and_the_list_follow_the_board() {
    // Board (design px, x4/3): ring centre 380, title block at 664, list at 820, progress 16 below the list.
    assert_eq!(bootdraw::RING_CENTRE, (1280, px(380)));
    assert_eq!(bootdraw::row_top(0), px(820));
    // Ten steps end above the scene's bottom.
    assert!(bootdraw::row_top(KEYS.len()) <= bootdraw::SCENE_H as i32, "{}", bootdraw::row_top(KEYS.len()));
    // The scanlines: one row in four carries the faint ink (the dot grid and gradients aside).
    let f = picture(&Steps::new(), 2560, 1440);
    let (a, b) = (pixel(&f, 3, 1200), pixel(&f, 3, 1201));
    assert!(a != b && (a & 0xFF) > (b & 0xFF), "{a:06x} {b:06x}");
}

#[test]
fn the_first_picture_in_bands_equals_the_whole() {
    let s = fed(&mini_pc_start()[..7]);
    let v = View::of(&s);
    for (w, h) in [(2560u32, 1440u32), (3840, 2160)] {
        let scene = Scene::new(w, h).unwrap();
        let whole = picture(&s, w, h);
        let mut f = RamFrame::new(w, h);
        let bands: Vec<_> = Scene::bands(w, h).collect();
        assert!(bands.iter().all(|b| b.h <= bootdraw::BAND_ROWS) && bands.iter().map(|b| b.h).sum::<u32>() == h);
        for b in bands {
            scene.draw_band(&mut f, &v, b);
        }
        assert!(f.pixels() == whole.pixels(), "{w}x{h}: bands differ from draw_all");
    }
}

#[test]
fn hostile_details_stay_one_short_printable_line() {
    let long = "x".repeat(10_000);
    let hostile = alloc::format!("a\nb\tc\u{1b}[31md\u{7f}eÄÖÜß€☃\u{0}{long}");
    let mut s = fed(&mini_pc_start());
    assert!(s.feed(&line(2000, 'E', "console", "desktop.given_up", &alloc::format!("program=/bin/x attempts=3 reason={hostile}"))));
    let d = s.get(Key::Desktop).detail.clone();
    assert!(d.chars().count() <= 80 && !d.chars().any(char::is_control), "{d:?}");
    // The same line again is no change (compared after cleaning).
    let seq = s.seq;
    s.feed(&line(2001, 'E', "console", "desktop.given_up", &alloc::format!("program=/bin/x attempts=3 reason={hostile}")));
    assert_eq!(s.seq, seq);
    let text = s.proc_text("text", 3);
    assert_eq!(text.lines().filter(|l| l.starts_with("step ")).count(), KEYS.len());
    assert_eq!(text.lines().count(), 5 + KEYS.len(), "{text}");
    let back = Steps::from_proc(&text).expect("round trip");
    assert_eq!(back.get(Key::Desktop).detail, d);
    assert_eq!(back.get(Key::Desktop).state, State::Failed);
    // Characters the atlas lacks are left out; drawing never panics.
    let _ = picture(&back, 2560, 1440);
}

#[test]
fn a_flapping_step_keeps_the_list_bounded() {
    let mut s = fed(&mini_pc_start());
    for i in 0..5000u64 {
        s.feed(&line(3000 + i, 'I', "console", "desktop.start", &alloc::format!("program=/bin/x attempt={i}")));
    }
    assert_eq!(s.get(Key::Desktop).state, State::Running);
    assert!(s.get(Key::Desktop).detail.chars().count() <= 80);
    assert_eq!(s.proc_text("shown", 0).lines().count(), 5 + KEYS.len());
}

#[test]
fn corrupted_proc_text_is_refused_or_read_without_panic() {
    let good = fed(&mini_pc_start()).proc_text("shown", 1);
    assert!(Steps::from_proc(&alloc::format!("{good}junk line\n\n")).is_some());
    for bad in ["step", "step kernel", "step kernel done", "step kernel done x", "step nope done 1 d", "step kernel ??? 1 d", "seq x", "generation -1"] {
        assert!(Steps::from_proc(bad).is_none(), "{bad}");
    }
    // Cut anywhere (DS-5 may read a short file): no panic.
    for n in 0..good.len() {
        if good.is_char_boundary(n) {
            let _ = Steps::from_proc(&good[..n]);
        }
    }
}

#[test]
fn a_desktop_that_ended_after_it_took_the_screen_fails_its_step() {
    let mut s = fed(&mini_pc_start());
    s.feed(&line(2000, 'I', "console", "desktop.start", "program=/bin/bitscomp attempt=1"));
    s.feed(&line(2100, 'W', "console", "desktop.ended", "program=/bin/bitscomp attempt=1 reason=crashed"));
    assert_eq!(s.get(Key::Desktop).state, State::Failed);
    assert!(s.get(Key::Desktop).detail.contains("abgestürzt"), "{}", s.get(Key::Desktop).detail);
}

/// The font rasteriser stays a build dependency: the kernel only blends the atlases (rust-ecosystem.md: fontdue
/// belongs on the host or in user space). Moving it to `[dependencies]` turns this red.
#[test]
fn the_rasteriser_is_only_a_build_dependency() {
    let toml = include_str!("../Cargo.toml");
    let mut section = "";
    for l in toml.lines().map(str::trim) {
        if l.starts_with('[') {
            section = l;
            continue;
        }
        if l.starts_with("fontdue") || l.starts_with("ttf-parser") || l.starts_with("ab_glyph") {
            assert_eq!(section, "[build-dependencies]", "{l}");
        }
    }
    assert!(toml.contains("[build-dependencies]\nfontdue"), "the atlases need fontdue at build time");
}
