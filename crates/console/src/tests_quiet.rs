//! Host tests of IN-quiet: the console's log filter (levels, the important Info events, the line head parser on
//! klog's own rendering), `log level` on the monitor, the open line carried below a log line (`write_above`), and
//! the echo drawn before a finished line reaches its reader.

extern crate std;

use crate::keymap::Key;
use crate::logfilter::{self, ALL, DEBUG, ERROR, INFO, OFF, WARN};
use crate::screen::{self, Cell, Screen};
use crate::tty::Tty;
use alloc::string::String;
use alloc::vec::Vec;
use klog::{Level, Record};

/// A line exactly as klog renders it for its sinks.
fn line(level: Level, subsystem: &'static str, name: &'static str, text: &str) -> String {
    Record { seq: 1, time_ns: 12_345_678_901, cpu: 3, thread: 7, level, subsystem, name, text: String::from(text) }.render()
}

#[test]
fn levels_parse_and_name() {
    for (s, n) in [("off", OFF), ("none", OFF), ("error", ERROR), ("warn", WARN), ("info", INFO), ("debug", DEBUG), ("all", ALL), ("trace", ALL), (" info ", INFO), ("3", INFO)] {
        assert_eq!(logfilter::parse(s), Some(n), "{s}");
    }
    for bad in ["", "loud", "warning", "6", "Info"] {
        assert_eq!(logfilter::parse(bad), None, "{bad}");
    }
    for n in OFF..=ALL {
        assert_eq!(logfilter::parse(logfilter::name(n)), Some(n));
    }
    assert_eq!(logfilter::name(200), "all");
    assert_eq!(logfilter::DEFAULT, WARN, "the console is quiet by default");
}

#[test]
fn head_reads_klogs_rendering() {
    let l = line(Level::Info, "net", "dhcp.lease", "ifname=igc0 addr=192.168.100.113/24 | lease 864000 s");
    assert!(l.starts_with("[   12.345678] I net dhcp.lease "), "{l}");
    assert_eq!(logfilter::head(&l), Some(('I', "net", "dhcp.lease")));
    // Without fields, and with a time wider than the padding.
    let bare = Record { seq: 0, time_ns: 123_456_000_000_000, cpu: 0, thread: 0, level: Level::Warn, subsystem: "kernel", name: "ready", text: String::new() }.render();
    assert_eq!(logfilter::head(&bare), Some(('W', "kernel", "ready")));
    // A message that imitates a head does not fool it: the head is the line's start.
    let fake = line(Level::Info, "user", "kmsg", "| [    1.000000] I kernel ready");
    assert_eq!(logfilter::head(&fake), Some(('I', "user", "kmsg")));
    for not in ["[... 5 records lost]", "", "[", "[ 1.0] ", "[ 1.0] I", "[ 1.0] I net", "[ 1.0] II net x", "no bracket I net x"] {
        assert_eq!(logfilter::head(not), None, "{not:?}");
    }
}

#[test]
fn quiet_console_shows_warnings_and_important_events_only() {
    let noise = [
        line(Level::Info, "mcp", "http.request", "peer=192.168.100.20 method=POST path=/mcp"),
        line(Level::Info, "net", "tcp.listen", "port=8765"),
        line(Level::Info, "mm", "report", "free_mb=30000"),
        line(Level::Info, "usb", "device.attached", "slot=1"),
        line(Level::Info, "ul", "test.init_once", ""),
        line(Level::Info, "net", "dhcp.renewed", ""),
        line(Level::Info, "user", "kmsg", "| kernel ready"),
    ];
    let important = [
        line(Level::Info, "kernel", "start", "version=0.2 gen_reason=reload"),
        line(Level::Info, "kernel", "ready", "ms=750"),
        line(Level::Info, "net", "link.up", "ifname=igc0 speed_mbps=1000"),
        line(Level::Info, "net", "iface.link_up", "ifname=igc0"),
        line(Level::Info, "net", "dhcp.lease", "addr=192.168.100.113/24"),
        line(Level::Info, "mcp", "server.start", "port=8765"),
        line(Level::Info, "watchdog", "armed", "timeout_s=60"),
        line(Level::Info, "ul", "init.start", "path=/bin/init"),
        line(Level::Info, "kexec", "reload.requested", ""),
        line(Level::Info, "power", "reboot.requested", ""),
        line(Level::Info, "input", "device.registered", "name=\"Logitech USB Receiver\""),
    ];
    let warn = line(Level::Warn, "net", "link.down", "ifname=igc0");
    let err = line(Level::Error, "ul", "init.died", "code=1");
    let debug = line(Level::Debug, "net", "dhcp.discover", "");
    let trace = line(Level::Trace, "sched", "switch", "");
    let lost = String::from("[... 12 records lost]");

    for l in &noise {
        assert!(!logfilter::shows(WARN, Level::Info, l), "quiet console shows noise: {l}");
        assert!(logfilter::shows(INFO, Level::Info, l), "info console hides {l}");
    }
    for l in &important {
        assert!(logfilter::shows(WARN, Level::Info, l), "quiet console hides an important event: {l}");
        assert!(!logfilter::shows(ERROR, Level::Info, l));
        assert!(!logfilter::shows(OFF, Level::Info, l));
    }
    // An important name at Debug stays hidden at warn (only Info is lifted).
    assert!(!logfilter::shows(WARN, Level::Debug, &line(Level::Debug, "kernel", "ready", "")));
    assert!(logfilter::shows(WARN, Level::Warn, &warn) && logfilter::shows(WARN, Level::Error, &err) && logfilter::shows(WARN, Level::Warn, &lost));
    assert!(!logfilter::shows(WARN, Level::Debug, &debug) && !logfilter::shows(INFO, Level::Debug, &debug) && logfilter::shows(DEBUG, Level::Debug, &debug));
    assert!(!logfilter::shows(DEBUG, Level::Trace, &trace) && logfilter::shows(ALL, Level::Trace, &trace));
    assert!(logfilter::shows(ERROR, Level::Error, &err) && !logfilter::shows(ERROR, Level::Warn, &warn));
    assert!(!logfilter::shows(OFF, Level::Error, &err) && !logfilter::shows(OFF, Level::Warn, &lost));
    // Prefix entries match only their prefix; exact ones only themselves.
    assert!(logfilter::important("ul", "init.whatever") && !logfilter::important("ul", "initx"));
    assert!(logfilter::important("kexec", "reload.fetched") && !logfilter::important("kexec", "ktest.parked"));
    assert!(!logfilter::important("net", "link.upx") && !logfilter::important("netx", "link.up"));
}

/// Every important (subsystem, name) is emitted somewhere in the kernel's sources, so a rename elsewhere cannot
/// silently make the quiet console miss a milestone.
#[test]
fn important_events_exist_in_the_sources() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut text = String::new();
    let mut stack: Vec<std::path::PathBuf> = alloc::vec![root.join("crates"), root.join("kernel")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())).flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("tests_quiet.rs") && !p.ends_with("logfilter.rs") {
                text.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
                text.push('\n');
            }
        }
    }
    let emitted = |s: &str, n: &str| {
        let open = alloc::format!("\"{s}\"");
        text.match_indices(&open).any(|(i, _)| {
            let rest = text[i + open.len()..].trim_start();
            let Some(rest) = rest.strip_prefix(',') else { return false };
            let Some(rest) = rest.trim_start().strip_prefix('"') else { return false };
            if n.ends_with('.') { rest.starts_with(n) } else { rest.strip_prefix(n).is_some_and(|r| r.starts_with('"')) }
        })
    };
    for &(s, n) in logfilter::IMPORTANT {
        assert!(emitted(s, n), "no event \"{s}\", \"{n}\" in crates/ or kernel/: update logfilter::IMPORTANT");
    }
}

#[test]
fn monitor_log_level_shows_and_sets() {
    let before = crate::log_level();
    let (ok, out) = crate::monitor::execute("log level info", true, false);
    assert!(ok && out.starts_with("log level info:"), "{out}");
    assert_eq!(crate::log_level(), INFO);
    let (ok, out) = crate::monitor::execute("log level", false, false);
    assert!(ok && out.starts_with("log level info:"), "{out}");
    let (ok, out) = crate::monitor::execute("log level loud", false, false);
    assert!(!ok && out.contains("want off, error, warn"), "{out}");
    assert_eq!(crate::log_level(), INFO, "a bad level changed nothing");
    let (ok, out) = crate::monitor::execute("log 5 6", false, false);
    assert!(!ok && out.contains("want log [n]"), "{out}");
    let (ok, out) = crate::monitor::execute("log x", false, false);
    assert!(!ok && out.contains("not a number"), "{out}");
    let (ok, out) = crate::monitor::execute("log 3", false, false);
    assert!(ok, "{out}");
    crate::set_log_level(before, "test");
    for n in OFF..=ALL {
        assert!(crate::monitor::level_text(n).starts_with(&alloc::format!("log level {}:", logfilter::name(n))));
    }
}

fn text_of(s: &Screen) -> Vec<String> {
    s.text_lines()
}

#[test]
fn log_line_goes_above_the_typed_line() {
    let mut s = Screen::new(20, 6);
    s.write("hello\n");
    s.attr = 10;
    s.write("bits> ");
    s.attr = screen::FG_INDEX;
    s.write("echo Grü");
    s.write_above("[ 1.0] W net link.down", 3);
    assert_eq!(text_of(&s), ["hello", "[ 1.0] W net link.do", "wn", "bits> echo Grü"]);
    assert_eq!((s.col, s.row), (14, 3), "cursor at the end of the typed text");
    assert_eq!(s.open_line(), "bits> echo Grü");
    // Colours kept: prompt green, typed text default, the log line in its own colour.
    assert_eq!(s.visible_row(3)[0], Cell { ch: 'b', attr: 10 });
    assert_eq!(s.visible_row(3)[6], Cell { ch: 'e', attr: screen::FG_INDEX });
    assert_eq!(s.visible_row(1)[0].attr, 3);
    // Typing goes on where it was; the next log line moves the longer line again.
    s.write("ße");
    s.write_above("second", 1);
    assert_eq!(text_of(&s), ["hello", "[ 1.0] W net link.do", "wn", "second", "bits> echo Grüße"]);
    assert_eq!(s.attr, screen::FG_INDEX, "the writer's attribute is kept");
}

#[test]
fn a_wrapped_typed_line_moves_whole_and_scrolls() {
    let mut s = Screen::new(10, 4);
    s.write("a\nb\n");
    s.write("bits> 0123456789abcdef"); // 22 cells: two full rows and two cells
    assert_eq!(s.open_line(), "bits> 0123456789abcdef");
    s.write_above("LOG", 7);
    // "b" scrolled into the scrollback; the log line sits right above the whole typed line.
    assert_eq!(text_of(&s), ["LOG", "bits> 0123", "456789abcd", "ef"]);
    assert_eq!(s.scrollback_len(), 2);
    assert_eq!(s.open_line(), "bits> 0123456789abcdef", "the open line still spans its wrapped rows");
    assert_eq!(s.col, 2);
    // A line exactly at the right edge (pending wrap) moves as a whole too.
    let mut s = Screen::new(10, 4);
    s.write("bits> 0123");
    assert_eq!(s.col, 10);
    s.write_above("L", 7);
    assert_eq!(text_of(&s), ["L", "bits> 0123"]);
    assert_eq!((s.col, s.row), (10, 1));
    s.write("x");
    assert_eq!(text_of(&s), ["L", "bits> 0123", "x"]);
}

#[test]
fn nothing_open_and_text_right_of_the_cursor() {
    // Nothing open: the log line is simply the next line.
    let mut s = Screen::new(20, 4);
    s.write("done\n");
    s.write_above("L1", 7);
    s.write_above("L2", 7);
    assert_eq!(text_of(&s), ["done", "L1", "L2"]);
    assert_eq!((s.col, s.row), (0, 3));
    // A progress line rewritten with \r: the text right of the cursor moves with it, the cursor stays in place.
    let mut s = Screen::new(20, 4);
    s.write("copy 50%\r");
    s.write_above("L", 7);
    assert_eq!(text_of(&s), ["L", "copy 50%"]);
    assert_eq!((s.col, s.row), (0, 1));
    s.write("copy 75%");
    assert_eq!(text_of(&s), ["L", "copy 75%"]);
}

#[test]
fn a_half_written_escape_survives_a_log_line() {
    let mut s = Screen::new(20, 4);
    s.write("x\x1b[3");
    s.write_above("L", 7);
    s.write("1mred");
    assert_eq!(text_of(&s), ["L", "xred"]);
    assert_eq!(s.visible_row(1)[1].attr & 15, 1, "the sequence finished after the log line still applies");
    // Control characters inside the log line itself cannot leave an escape open.
    let mut s = Screen::new(20, 4);
    s.write("p> ");
    s.write_above("bad\x1b[", 7);
    s.write("ok");
    assert_eq!(text_of(&s), ["bad", "p> ok"]);
}

#[test]
fn echo_is_drawn_before_the_reader_gets_the_line() {
    let t = Tty::new();
    for c in "mem".chars() {
        t.key(Key::Char(c), false);
    }
    let mut seen: Vec<(String, bool)> = Vec::new();
    t.key_with(Key::Enter, false, &mut |e| seen.push((String::from_utf8_lossy(e).into_owned(), t.has_input())));
    // n_tty: Enter is CR, ICRNL makes it LF, its echo goes through ONLCR.
    assert_eq!(seen, [(String::from("\r\n"), false)], "the line was readable before its echo was drawn");
    let l = t.read_line(0).expect("the line after the echo");
    assert_eq!(l.text, "mem");
    let n = t.interrupts();
    let mut at_draw = None;
    t.key_with(Key::Ctrl(3), false, &mut |e| at_draw = Some((String::from_utf8_lossy(e).into_owned(), t.interrupts())));
    assert_eq!(at_draw, Some((String::from("^C"), n)), "Ctrl+C reached readers before its echo was drawn");
    assert_eq!(t.interrupts(), n + 1);
    // A full input drops what does not fit, after the echo.
    for _ in 0..(crate::tty::BUF_SIZE / 8 + 2) {
        for c in "1234567".chars() {
            t.key(Key::Char(c), false);
        }
        t.key(Key::Enter, false);
    }
    let mut lines = 0;
    while t.read_line(0).is_some() {
        lines += 1;
    }
    assert!(lines > 0 && lines <= crate::tty::BUF_SIZE / 8, "{lines} lines kept");
    let _ = Cell::BLANK;
}
