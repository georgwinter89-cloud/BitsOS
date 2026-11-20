//! GS-1 host tests: the graphics marks beside the step list.

use crate::bootmarks::Marks;
use crate::bootsteps::{Key, State, Steps};

#[test]
fn marks_come_from_the_lines_and_survive_the_proc_file() {
    let mut s = Steps::new();
    for l in [
        "[    1.200000] I gx boot.start bound_s=30 at_ms=1200",
        "[    2.900000] I gx boot.engines_up wrote=true at_ms=2900",
        "[    3.100000] I gx boot.up minor=128 after_ms=1900 at_ms=3100",
        "[    3.300000] I console desktop.start program=/bin/bitscomp attempt=1 at_ms=3300",
        "[    9.000000] I console desktop.start program=/bin/bitscomp attempt=2 at_ms=9000",
    ] {
        s.feed(l);
    }
    assert_eq!(s.marks.get("gfx_start"), Some(1200));
    assert_eq!(s.marks.get("engines"), Some(2900));
    assert_eq!(s.marks.get("node"), Some(3100));
    // The first start is the one the desktop's way began with.
    assert_eq!(s.marks.get("desktop_start"), Some(3300));
    // The mark is no step: the picture does not change for it (the gpu row is still the bring-up's).
    assert_eq!(s.get(Key::Gpu).state, State::Done);
    let text = s.proc_text("shown", 0);
    assert!(text.contains("\nmark gfx_start 1200\nmark engines 2900\nmark node 3100\nmark desktop_start 3300\n"), "{text}");
    let back = Steps::from_proc(&text).unwrap();
    assert_eq!(back.marks, s.marks);
    // Before any line: no mark line at all, and the file stays what DS-5's reader knew.
    assert!(!Steps::new().proc_text("shown", 0).contains("mark "));
    assert!(Steps::watched("gx", "boot.engines_up"), "lost below the log level");
}

#[test]
fn a_mark_at_the_first_millisecond_still_counts() {
    let mut m = Marks::new();
    m.note("gx", "boot.start", 0);
    assert_eq!(m.get("gfx_start"), Some(1));
    m.note("gx", "state.classified", 5);
    assert_eq!(m.get("engines"), None);
}

#[test]
fn a_card_attached_read_only_settles_the_gpu_row_without_a_bring_up() {
    // The Gaming-PC: its RDNA4 is classified and its display found, then the 780M's driver refuses it (GP-GPU1).
    let mut s = Steps::new();
    for l in [
        "[    1.000000] I dcn attached function=0000:03:00.0 bar5=0 size=0",
        "[    1.100000] I gx state.classified start=cold gc=Gfx12 writes=0",
        "[    1.200000] I gx boot.read_only cards=1",
    ] {
        s.feed(l);
    }
    let gpu = s.get(Key::Gpu);
    assert_eq!(gpu.state, State::Skipped, "not running for good: bitscomp would show the line for a late GPU");
    let text = s.proc_text("shown", 0);
    assert!(text.lines().any(|l| l.starts_with("step gpu skipped ") && l.ends_with("nur gop-bild")), "{text}");
    assert!(Steps::watched("gx", "boot.read_only"), "lost below the log level");
}
