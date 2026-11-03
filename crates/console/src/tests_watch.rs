//! IN-9 host tests: the step events klog's watch hands over below the level filter (`Steps::watched`), and the
//! step list a start at `loglevel=error` builds from them (it must equal the one at info).

extern crate std;

use crate::bootsteps::{Init, Key, State, Steps};
use alloc::string::String;
use alloc::vec::Vec;

fn line(t_ms: u64, level: char, subsystem: &str, name: &str, fields: &str) -> String {
    alloc::format!("[{:5}.{:06}] {level} {subsystem} {name} {fields}", t_ms / 1000, (t_ms % 1000) * 1000)
}

/// One line for every named (not Error-only) arm of the step mapping.
fn every_named() -> Vec<String> {
    let l = |s, n, f| line(10, 'I', s, n, f);
    alloc::vec![
        l("kernel", "start", "version=0.1.0 generation=3"),
        l("kernel", "ready", ""),
        l("kernel", "mcp.not_started", "reason=no_net"),
        l("mm", "frames", "total_mb=100 free_mb=90"),
        l("smp", "prepared", ""),
        l("smp", "done", "cpus=4 failed=0"),
        l("smp", "not_started", "reason=acpi"),
        l("usb", "xhci.up", "ctrl=xhci0"),
        l("nvme", "ctrl.ready", "ctrl=nvme0"),
        l("block", "disk.stick", "disk=sda"),
        l("fat", "stick.mounted", "disk=sda"),
        l("net", "igc.up", "ifname=igc0"),
        l("net", "virtio.up", "ifname=eth0"),
        l("net", "link.up", "ifname=igc0"),
        l("net", "iface.link_up", "ifname=igc0"),
        l("net", "dhcp.lease", "ifname=igc0 ip=192.0.2.5"),
        l("net", "addr.static", "ifname=igc0 ip=192.0.2.5"),
        l("wlan", "netdev.registered", "ifname=wlan0"),
        l("snd", "hda.probe", ""),
        l("snd", "hda.output", ""),
        l("dcn", "attached", ""),
        l("gx", "state.classified", ""),
        l("gx", "gfx.booted", ""),
        l("mcp", "server.start", "port=8765"),
        l("mcp", "server.not_started", ""),
        l("mcp", "server.start_failed", "reason=bind"),
        l("ul", "init.start", "pid=1"),
        l("ul", "init.off", ""),
        l("ul", "init.died", "code=1"),
        l("ul", "init.given_up", ""),
        l("console", "desktop.off", "why=off"),
        l("console", "desktop.wait", ""),
        l("console", "desktop.start", "program=/bin/bitscomp attempt=1"),
        l("console", "desktop.failed", "attempt=1 reason=ended"),
        l("console", "desktop.up", "program=/bin/bitscomp"),
        l("console", "desktop.given_up", "attempts=3 reason=ended"),
        l("console", "desktop.ended", "reason=crashed"),
        l("console", "desktop.slow", ""),
        l("console", "desktop.no_init", ""),
    ]
}

fn head(l: &str) -> (&str, &str) {
    let mut w = l.split("] ").nth(1).unwrap().split(' ');
    w.next();
    (w.next().unwrap(), w.next().unwrap())
}

#[test]
fn every_named_step_event_is_watched() {
    for l in every_named() {
        let (sub, name) = head(&l);
        assert!(Steps::watched(sub, name), "not watched: {l}");
        assert!(Steps::new().feed(&l), "the mapping ignores {l} (the list in the test is stale)");
    }
}

#[test]
fn the_watch_leaves_what_the_ring_carries() {
    // Error lines always pass the level filter: they reach the step sink through the ring, never twice.
    for (sub, name) in [("net", "dhcp.timeout"), ("usb", "xhci.halted"), ("gx", "cp.busy"), ("mm", "oom")] {
        assert!(!Steps::watched(sub, name), "{sub} {name}");
    }
    for (sub, name) in [("console", "desktop.mode"), ("boot", "loader.line"), ("klog", "levels.set"), ("kernel", "initcall.failed"), ("", "")] {
        assert!(!Steps::watched(sub, name), "{sub} {name}");
    }
    // The ktest's own event goes through the watch but changes nothing.
    assert!(Steps::watched("ktest", "in.boot.watch"));
    assert!(!Steps::new().feed(&line(5, 'I', "ktest", "in.boot.watch", "n=1")));
}

/// A whole start with the desktop, as the Mini-PC logs it at info, plus a Warn and an Error line.
fn start_with_desktop() -> Vec<String> {
    alloc::vec![
        line(12, 'I', "kernel", "start", "version=0.1.0 generation=68 reload=0"),
        line(14, 'I', "boot", "loader.line", "text=x"),
        line(40, 'I', "mm", "frames", "total_mb=31744 free_mb=30876"),
        line(95, 'I', "smp", "prepared", ""),
        line(180, 'I', "smp", "done", "cpus=16 failed=0"),
        line(200, 'I', "console", "desktop.mode", "on=true why=auto program=/bin/bitscomp"),
        line(260, 'I', "usb", "xhci.up", "ctrl=xhci0"),
        line(270, 'W', "usb", "port.slow", "port=3"),
        line(290, 'E', "wlan", "fw.timeout", "step=boot"),
        line(300, 'I', "net", "igc.up", "ifname=igc0"),
        line(320, 'I', "snd", "hda.probe", ""),
        line(330, 'I', "snd", "hda.output", ""),
        line(350, 'I', "dcn", "attached", ""),
        line(360, 'I', "gx", "state.classified", "state=after_reload"),
        line(410, 'I', "block", "disk.stick", "disk=sda"),
        line(450, 'I', "fat", "stick.mounted", "disk=sda"),
        line(520, 'I', "mcp", "server.start", "port=8765"),
        line(530, 'I', "kernel", "ready", ""),
        line(540, 'I', "console", "desktop.wait", ""),
        line(600, 'I', "ul", "init.start", "pid=1 path=/bin/init"),
        line(610, 'I', "console", "desktop.start", "program=/bin/bitscomp attempt=1"),
        line(700, 'I', "console", "desktop.up", "program=/bin/bitscomp after_ms=100"),
        line(900, 'I', "net", "link.up", "ifname=igc0"),
        line(1500, 'I', "net", "dhcp.lease", "ifname=igc0 ip=192.0.2.7"),
    ]
}

/// What the step model gets at a level: lines at or above it through the ring (the step sink), the watched ones
/// below it through klog's watch.
fn at_level(lines: &[String], keep: &[char]) -> (Steps, usize) {
    let mut s = Steps::new();
    let mut watched = 0;
    for l in lines {
        let level = l.split("] ").nth(1).unwrap().chars().next().unwrap();
        let (sub, name) = head(l);
        if keep.contains(&level) {
            s.feed(l);
        } else if Steps::watched(sub, name) {
            watched += 1;
            s.feed(l);
        }
    }
    (s, watched)
}

#[test]
fn a_start_at_loglevel_error_or_warn_lists_what_info_lists() {
    let lines = start_with_desktop();
    let (info, none) = at_level(&lines, &['E', 'W', 'I']);
    assert_eq!(none, 0, "nothing is dropped at info");
    for keep in [&['E', 'W'][..], &['E'][..]] {
        let (s, watched) = at_level(&lines, keep);
        assert!(watched >= 20, "{watched}");
        assert_eq!(s, info, "at {keep:?}");
    }
    assert_eq!(info.init, Init::Running);
    assert_eq!(info.get(Key::Desktop).state, State::Done);
    assert_eq!(info.get(Key::Network).state, State::Done);
    assert_eq!(info.get(Key::Kernel).detail, "version 0.1.0");
    assert_eq!(info.generation, 68);
}

#[test]
fn an_error_after_a_watched_success_keeps_the_success() {
    // At error a watched Info line is fed at once, an Error line later from the ring (klogd): a later failure
    // never hides an earlier success of the same step, so the order does not matter.
    let ok = line(900, 'I', "net", "dhcp.lease", "ifname=igc0 ip=192.0.2.7");
    let err = line(800, 'E', "net", "dhcp.timeout", "ifname=wlan0");
    let mut a = Steps::new();
    a.feed(&ok);
    a.feed(&err);
    let mut b = Steps::new();
    b.feed(&err);
    b.feed(&ok);
    assert_eq!(a.get(Key::Network).state, State::Done);
    assert_eq!(b.get(Key::Network).state, State::Done);
}

#[test]
fn a_start_with_desktop_off_at_error_settles_without_the_desktop() {
    let lines: Vec<String> = start_with_desktop()
        .into_iter()
        .filter(|l| !head(l).1.starts_with("desktop."))
        .chain([line(545, 'I', "console", "desktop.off", "why=off")])
        .collect();
    let (s, _) = at_level(&lines, &['E']);
    assert_eq!(s.get(Key::Desktop).state, State::Skipped);
    assert_eq!(s.get(Key::Desktop).detail, "desktop=off");
    // The rest is what info lists (this start's GPU stays at "wird geprüft" at every level).
    assert_eq!(s, at_level(&lines, &['E', 'W', 'I']).0);
    assert_eq!(s.init, Init::Running);
}

#[test]
fn a_detail_is_cut_and_clean_without_the_heap() {
    use crate::stepdetail::{CHARS, Detail};
    let d = Detail::from("a\u{1b}[31mb\nc\u{7f}");
    assert_eq!(d, "a[31mbc");
    // 4-byte characters fill the inline buffer exactly; nothing past `CHARS` is kept, cut on a whole character.
    let wide: String = core::iter::repeat('😀').take(CHARS + 5).collect();
    let d = Detail::from(wide.as_str());
    assert_eq!(d.chars().count(), CHARS);
    assert!(d.chars().all(|c| c == '😀'));
    let mixed: String = core::iter::repeat("aé").take(CHARS).collect();
    assert_eq!(Detail::from(mixed.as_str()).chars().count(), CHARS);
    assert_eq!(Detail::of(format_args!("{} kerne {}", 16, "bereit")), "16 kerne bereit");
}
