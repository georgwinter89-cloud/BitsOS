//! GP-BUS fixer: a driver that tried a function and gave up stays visible on the Gaming-PC's report page, with its
//! reason, after the binding record is freed; "no driver exists" keeps its own text.

use super::gpworld::{self, gaming_pc};
use super::{find, no_keys};
use crate::enumerate::Root;
use crate::failed::{self, Failure};
use crate::registry::Candidate;
use crate::{bind, build, gp, BindError, Loc, Match, PciFunction};
use alloc::string::String;
use alloc::vec::Vec;

const ROOTS: [Root; 2] = [Root { segment: 0, bus_start: 0x00, bus_end: 0x7f }, Root { segment: 0, bus_start: 0x80, bus_end: 0xdf }];
const XHCI: [Match; 1] = [Match::Class { class: 0x0C_03_30, mask: 0xFF_FF_FF }];

fn gp_keys(n: &crate::Node) -> Option<&'static str> {
    gp::never_touch_key(true, n.vendor, n.device, n.class)
}

fn gives_up(_: &PciFunction) -> Result<(), String> {
    Err(String::from("map BAR0: OutOfBounds"))
}

fn takes_it(_: &PciFunction) -> Result<(), String> {
    Ok(())
}

fn line_of(lines: &[String], place: &str) -> String {
    lines.iter().find(|l| l.trim_start().starts_with(place)).cloned().unwrap_or_default()
}

#[test]
fn the_page_tells_a_driver_that_gave_up_from_no_driver() {
    let w = gaming_pc();
    let nodes = build(&w, &ROOTS, no_keys);
    let pch = Loc::new(0, 0x80, 0x14, 0);
    let driver = |at: Loc| (at == Loc::new(0, 0x00, 0x0d, 0)).then(|| String::from("xhci"));
    // The chipset's controller (the keyboard's) was tried and dropped; the CPU's runs; nobody tried the network card.
    let gave_up = |at: Loc| (at == pch || at == Loc::new(0, 0x00, 0x0d, 0)).then(|| Failure { driver: "xhci", error: String::from("map BAR0: OutOfBounds") });
    let lines = gp::report_lines(&nodes, &ROOTS, &[], &driver, &gave_up);
    assert_eq!(line_of(&lines, "80:14.0"), "  80:14.0 USB-Anschlüsse [8086:7f6e]: Treiber xhci hat aufgegeben (map BAR0: OutOfBounds)");
    assert_eq!(line_of(&lines, "83:00.0"), "  83:00.0 Netzwerkkarte (Kabel) [10ec:8126]: erkannt, ohne Treiber");
    // A bound driver wins over an old note.
    assert_eq!(line_of(&lines, "00:0d.0"), "  00:0d.0 USB-Anschlüsse [8086:7ec0]: läuft mit xhci");
    // Without a note the line is the one it was.
    let lines = gp::report_lines(&nodes, &ROOTS, &[], &driver, &|_| None);
    assert_eq!(line_of(&lines, "80:14.0"), "  80:14.0 USB-Anschlüsse [8086:7f6e]: erkannt, ohne Treiber");
}

#[test]
fn a_never_touched_function_stays_never_touched_whatever_a_note_says() {
    let w = gaming_pc();
    let nodes = build(&w, &ROOTS, gp_keys);
    let lines = gp::report_lines(&nodes, &ROOTS, &[], &|_| None, &|_| Some(Failure { driver: "x", error: String::from("y") }));
    let at = gpworld::FLASH_PCH;
    assert_eq!(line_of(&lines, &alloc::format!("{:02x}:{:02x}.{}", at.0, at.1, at.2)), "  80:1f.5 Baustein mit der Firmware des Mainboards [8086:7f24]: wird nie angefasst");
}

/// The real path: `bind_node` with a driver whose probe fails. The binding record is freed as before, the note
/// stays; the next offer drops it, and a driver that binds leaves none.
#[test]
fn bind_keeps_the_reason_of_a_failed_probe_until_the_next_offer() {
    let w = gaming_pc();
    let nodes = build(&w, &ROOTS, no_keys);
    // A segment no other test uses: the tree and the binding records are shared by every test of this crate.
    let mut n = find(&nodes, 0x80, 0x14, 0).unwrap().clone();
    n.loc.segment = 0x7777;
    n.parent = None;
    crate::NODES.set(alloc::vec![n.clone()]);
    let cand = |probe: fn(&PciFunction) -> Result<(), String>| Candidate { name: "xhci", matches: &XHCI, probe };
    assert_eq!(failed::last(n.loc), None);

    assert_eq!(bind::bind_node(&n, &[cand(gives_up)]), Err(BindError::NoDriver));
    assert!(bind::binding(n.loc).is_none(), "the record of a failed probe is freed, as before");
    assert_eq!(failed::last(n.loc), Some(Failure { driver: "xhci", error: String::from("map BAR0: OutOfBounds") }));
    let roots = [Root { segment: 0x7777, bus_start: 0x80, bus_end: 0xdf }];
    let page = |nodes: &[crate::Node]| gp::report_lines(nodes, &roots, &[], &|at| bind::binding(at).map(|b| String::from(b.driver)), &failed::last);
    let lines: Vec<String> = page(core::slice::from_ref(&n));
    assert!(lines.iter().any(|l| l.ends_with("USB-Anschlüsse [8086:7f6e]: Treiber xhci hat aufgegeben (map BAR0: OutOfBounds)")), "{lines:?}");

    // Offered again with no driver that matches: nothing tried, so nothing gave up.
    assert_eq!(bind::bind_node(&n, &[]), Err(BindError::NoDriver));
    assert_eq!(failed::last(n.loc), None);

    // Two drivers: the first gives up, the second binds. No note is left behind a bound function.
    static WIDE: [Match; 1] = [Match::Class { class: 0x0C_03_00, mask: 0xFF_FF_00 }];
    let two = [cand(gives_up), Candidate { name: "usb-wide", matches: &WIDE, probe: takes_it }];
    assert_eq!(bind::bind_node(&n, &two), Ok("usb-wide"));
    assert_eq!(failed::last(n.loc), None);
    assert!(page(core::slice::from_ref(&n)).iter().any(|l| l.ends_with("läuft mit usb-wide")));
    let rec = crate::device::STATE.lock().remove(&n.loc);
    drop(rec);
    crate::NODES.set(Vec::new());
}
