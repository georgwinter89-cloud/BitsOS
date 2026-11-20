//! GPY-PASTE: host tests of reading the clipboard's pipe (end, deadline, size cap, UTF-8) and of the first-line
//! rule of a single-line field.

use std::io::Write;
use std::time::{Duration, Instant};

use smithay::input::keyboard::{Keysym, ModifiersState};

use super::*;

const SHORT: Limits =
    Limits { cap: LINE_CAP, idle: Duration::from_millis(150), total: Duration::from_millis(600), line: true };

/// The pipe's reading end, its writing end given to `write` on a thread (it may block on a full pipe).
fn piped(write: impl FnOnce(std::io::PipeWriter) + Send + 'static) -> PipeReader {
    let (r, w) = std::io::pipe().expect("pipe");
    std::thread::spawn(move || write(w));
    r
}

fn mods(ctrl: bool, shift: bool, alt: bool, logo: bool) -> ModifiersState {
    ModifiersState { ctrl, shift, alt, logo, ..Default::default() }
}

#[test]
fn ctrl_v_and_shift_insert_are_a_paste_nothing_else() {
    assert!(is_paste(Keysym::v, &mods(true, false, false, false)));
    assert!(is_paste(Keysym::V, &mods(true, true, false, false)));
    assert!(is_paste(Keysym::Insert, &mods(false, true, false, false)));
    assert!(is_paste(Keysym::KP_Insert, &mods(false, true, false, false)));
    assert!(!is_paste(Keysym::v, &mods(false, false, false, false)), "a plain v is typed");
    assert!(!is_paste(Keysym::v, &mods(true, false, true, false)), "Ctrl+Alt+V is another shortcut");
    assert!(!is_paste(Keysym::v, &mods(false, false, false, true)));
    assert!(!is_paste(Keysym::Insert, &mods(false, false, false, false)));
    assert!(!is_paste(Keysym::Insert, &mods(true, false, false, false)), "Ctrl+Insert copies");
}

#[test]
fn a_single_line_field_takes_the_first_line_that_is_not_blank() {
    assert_eq!(first_line("ghp_abc\n").as_deref(), Some("ghp_abc"), "the trailing newline goes");
    assert_eq!(first_line("  ghp_abc \r\nzweite Zeile").as_deref(), Some("ghp_abc"));
    assert_eq!(first_line("\n \n\tghp_abc\n").as_deref(), Some("ghp_abc"), "blank lines before it are skipped");
    assert_eq!(first_line("alt\rneu").as_deref(), Some("alt"), "a lone CR ends the line too");
    assert_eq!(first_line("a\tb\u{7}c").as_deref(), Some("a bc"), "a tab is a space, other control characters go");
    assert_eq!(first_line("Grüße ✳").as_deref(), Some("Grüße ✳"));
    assert_eq!(first_line(""), None);
    assert_eq!(first_line(" \n\t\r\n"), None);
}

#[test]
fn the_pipe_is_read_to_its_end_or_to_the_first_line() {
    let r = piped(|mut w| {
        let _ = w.write_all(b"ghp_TOKEN\nmehr");
    });
    assert_eq!(read(r, SHORT), Ok("ghp_TOKEN".into()));
    // A line mode read stops at the line's end: an owner that keeps the pipe open after it does not hold it.
    let r = piped(|mut w| {
        let _ = w.write_all(b"\n  ghp_TOKEN  \n");
        std::thread::sleep(Duration::from_secs(3));
    });
    let t = Instant::now();
    assert_eq!(read(r, SHORT), Ok("ghp_TOKEN".into()));
    assert!(t.elapsed() < Duration::from_millis(500), "it waited for the pipe's end: {:?}", t.elapsed());
    // Not a single line: everything.
    let r = piped(|mut w| {
        let _ = w.write_all(b"eins\nzwei\n");
    });
    assert_eq!(read(r, Limits { line: false, ..SHORT }), Ok("eins\nzwei\n".into()));
}

#[test]
fn nothing_written_is_no_text() {
    let r = piped(drop);
    assert_eq!(read(r, SHORT), Err(Why::Empty));
    let r = piped(|mut w| {
        let _ = w.write_all(b" \n \n");
    });
    assert_eq!(read(r, SHORT), Err(Why::Empty));
    let r = piped(|mut w| {
        let _ = w.write_all(&[0xff, 0xfe, b'a']);
    });
    assert_eq!(read(r, SHORT), Err(Why::Empty), "no UTF-8 is no text");
}

#[test]
fn an_owner_that_gives_nothing_or_trickles_is_given_up() {
    // It never writes and keeps the pipe open: given up after `idle`, not after its 3 s.
    let r = piped(|w| {
        std::thread::sleep(Duration::from_secs(3));
        drop(w);
    });
    let t = Instant::now();
    assert_eq!(read(r, SHORT), Err(Why::Slow));
    let took = t.elapsed();
    assert!(took >= SHORT.idle && took < Duration::from_millis(1500), "{took:?}");
    // It writes a byte now and then, never a whole line: given up after `total`.
    let r = piped(|mut w| {
        for _ in 0..100 {
            if w.write_all(b"x").is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    let t = Instant::now();
    assert_eq!(read(r, SHORT), Err(Why::Slow));
    let took = t.elapsed();
    assert!(took >= SHORT.total && took < Duration::from_millis(2000), "{took:?}");
    assert_eq!(IDLE, Duration::from_secs(2), "the field's deadline");
}

#[test]
fn the_size_cap_holds() {
    // One line longer than the cap: refused, not stored cut.
    let r = piped(|mut w| {
        let _ = w.write_all(&vec![b'a'; LINE_CAP + 10]);
    });
    assert_eq!(read(r, SHORT), Err(Why::TooLong));
    // A short first line in front of a huge rest: the line, and the rest is never read (the writer gets EPIPE).
    let r = piped(|mut w| {
        let _ = w.write_all(b"ghp_TOKEN\n");
        let _ = w.write_all(&vec![b'b'; 16 << 20]);
    });
    assert_eq!(read(r, SHORT), Ok("ghp_TOKEN".into()));
    // Not a single line: its start, cut at the cap (a cut character is dropped).
    let r = piped(|mut w| {
        let _ = w.write_all("aä".repeat(100).as_bytes());
    });
    let got = read(r, Limits { cap: 8, line: false, ..SHORT }).expect("its start");
    assert_eq!(got, "aäaäa", "8 bytes are aäaäa plus half an ä");
    assert_eq!(LINE_CAP, 64 << 10);
}
