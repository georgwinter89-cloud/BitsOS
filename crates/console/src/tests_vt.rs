//! The VT's escape sequences (Linux's set, TERM=linux): BusyBox 1.36.1's line editing and vi replayed from what the
//! real binary wrote on a 40x12 pty (`fixtures/busybox/capture.py`; a NUL marks each checkpoint), log lines arriving
//! in the middle of it, and every sequence on its own.

use crate::font::{Face, Font};
use crate::render;
use crate::screen::{self, Cell, Screen};
use alloc::string::String;
use alloc::vec::Vec;
use display::RamFrame;

/// A program's bytes as `write_program` draws them (the pty already applied ONLCR: a newline is a line feed).
fn out(s: &mut Screen, b: &[u8]) {
    s.write(&core::str::from_utf8(b).unwrap().replace('\n', "\u{b}"));
}

fn parts(fixture: &[u8]) -> Vec<&[u8]> {
    let mut v: Vec<&[u8]> = fixture.split(|&b| b == 0).collect();
    v.pop();
    v
}

/// Replays a fixture, checking the text and the cursor at each checkpoint.
fn check(fixture: &[u8], want: &[(&[&str], (u32, u32))]) {
    let mut s = Screen::new(40, 12);
    let p = parts(fixture);
    assert_eq!(p.len(), want.len(), "checkpoints");
    for (i, (part, (text, cur))) in p.iter().zip(want).enumerate() {
        out(&mut s, part);
        assert_eq!(s.text_lines(), *text, "text at checkpoint {i}");
        assert_eq!((s.col, s.row), *cur, "cursor at checkpoint {i}");
    }
}

const GEORG: &[u8] = include_bytes!("../fixtures/busybox/georg.bin");
const MIDDLE: &[u8] = include_bytes!("../fixtures/busybox/middle.bin");
const WRAP: &[u8] = include_bytes!("../fixtures/busybox/wrap.bin");
const HISTORY: &[u8] = include_bytes!("../fixtures/busybox/history.bin");
const KILL: &[u8] = include_bytes!("../fixtures/busybox/kill.bin");
const VI: &[u8] = include_bytes!("../fixtures/busybox/vi.bin");

const L1: &str = "$ echo 0123456789abcdefghijklmnopqrstuvw";
const LONG: &str = "$ echo long-0123456789-0123456789-012345";

#[test]
fn ash_georgs_case_left_backspace_delete_with_umlauts() {
    // `echo Grüße xyz`, Left x5, Backspace, Delete: the move back of 5 cells is `ESC[5D`. Without it the screen
    // showed `echo Grüe xyz xyz`.
    check(GEORG, &[(&["$ echo Grü xyz"], (10, 0)), (&["$ echo Grü xyz", "Grü xyz", "$"], (2, 2))]);
}

#[test]
fn ash_delete_and_backspace_in_the_middle() {
    check(MIDDLE, &[
        (&["$ echo abcdefgij"], (14, 0)),
        (&["$ echo abcdfgij"], (11, 0)),
        (&["$ echo abcdfgij", "abcdfgij", "$"], (2, 2)),
    ]);
}

#[test]
fn ash_line_over_two_rows_left_right_backspace_across_the_wrap() {
    check(WRAP, &[
        (&[L1, "xyzABCD"], (7, 1)),
        (&[L1, "xyzABCD"], (39, 0)),
        (&["$ echo 0123456789abcdefghijklmnopqrstuwx", "yzABCD"], (38, 0)),
        (&["$ echo 0123456789abcdefghijklmnopqrstuwx", "yzABCD"], (1, 1)),
        (&["$ echo 0123456789abcdefghijklmnopqrstuwz", "ABCD"], (39, 0)),
        (&["$ echo 0123456789abcdefghijklmnopqrstuwz", "ABCD", "0123456789abcdefghijklmnopqrstuwzABCD", "$"], (2, 3)),
    ]);
}

#[test]
fn ash_history_longer_then_shorter_line() {
    let ran: &[&str] = &[LONG, "6789", "long-0123456789-0123456789-0123456789", "$ echo hi", "hi"];
    let with = |tail: &[&'static str]| -> Vec<&'static str> {
        let mut v: Vec<&str> = ran.to_vec();
        v.extend_from_slice(tail);
        v
    };
    let mut s = Screen::new(40, 12);
    let p = parts(HISTORY);
    let want = [(with(&["$"]), (2, 5)), (with(&["$ echo hi"]), (9, 5)), (with(&[LONG, "6789"]), (4, 6)), (with(&["$ echo hi"]), (9, 5))];
    assert_eq!(p.len(), want.len());
    for (i, (part, (text, cur))) in p.iter().zip(&want).enumerate() {
        out(&mut s, part);
        assert_eq!(s.text_lines(), *text, "text at checkpoint {i}");
        assert_eq!((s.col, s.row), *cur, "cursor at checkpoint {i}");
    }
}

#[test]
fn ash_ctrl_u_and_ctrl_k() {
    check(KILL, &[(&["$  three"], (2, 0)), (&["$  t"], (4, 0)), (&["$  t", "ash: t: not found", "$"], (2, 2))]);
}

#[test]
fn vi_session_status_line_command_line_and_back_to_the_shell() {
    let screen = |l3: &'static str, status: &'static str| -> Vec<&'static str> {
        let mut v = alloc::vec!["first line", "second line", l3];
        v.extend_from_slice(&["~"; 8]);
        v.push(status);
        v
    };
    let mut s = Screen::new(40, 12);
    let p = parts(VI);
    assert_eq!(p.len(), 4);
    out(&mut s, p[0]);
    assert_eq!(s.text_lines(), screen("third line", "- t.txt 1/3 33%"));
    assert_eq!((s.col, s.row), (0, 0));
    assert_eq!(s.scrollback_len(), 0);
    out(&mut s, p[1]);
    assert_eq!(s.text_lines(), screen("third line more", "- t.txt [Modified] 3/3 100%"));
    assert_eq!((s.col, s.row), (14, 2));
    out(&mut s, p[2]);
    assert_eq!(s.text_lines(), screen("third line more", ":q!"), "the command line is on the bottom row");
    assert_eq!((s.col, s.row), (3, 11));
    out(&mut s, p[3]);
    assert_eq!(s.text_lines(), ["$ vi t.txt", "$"], "leaving the alternate screen brings the shell back");
    assert_eq!((s.col, s.row), (2, 1));
    assert_eq!(s.scrollback_len(), 0, "vi's screen never reaches the scrollback");
}

#[test]
fn log_lines_while_ash_edits_a_wrapped_line() {
    // The open line spans both rows (`\r\n` at the right edge continues it), also after the shell moved the cursor
    // up into its first row; the shell's relative moves keep working below the log lines.
    let mut s = Screen::new(40, 12);
    let p = parts(WRAP);
    out(&mut s, p[0]);
    s.write_above("LOG1", 3);
    assert_eq!(s.text_lines(), ["LOG1", L1, "xyzABCD"]);
    assert_eq!((s.col, s.row), (7, 2));
    out(&mut s, p[1]);
    assert_eq!((s.col, s.row), (39, 1));
    s.write_above("LOG2", 3);
    assert_eq!(s.text_lines(), ["LOG1", "LOG2", L1, "xyzABCD"]);
    assert_eq!((s.col, s.row), (39, 2), "the cursor stays inside the first row of the line");
    for part in &p[2..] {
        out(&mut s, part);
    }
    let end = "$ echo 0123456789abcdefghijklmnopqrstuwz";
    assert_eq!(s.text_lines(), ["LOG1", "LOG2", end, "ABCD", "0123456789abcdefghijklmnopqrstuwzABCD", "$"]);
}

#[test]
fn log_line_while_ash_shows_a_recalled_long_line() {
    let mut s = Screen::new(40, 12);
    let p = parts(HISTORY);
    for part in &p[..3] {
        out(&mut s, part);
    }
    s.write_above("LOG", 3);
    assert_eq!(&s.text_lines()[5..], ["LOG", LONG, "6789"]);
    out(&mut s, p[3]);
    assert_eq!(&s.text_lines()[4..], ["hi", "LOG", "$ echo hi"], "Down replaced the whole recalled line");
    assert_eq!((s.col, s.row), (9, 6));
    // A line that ended exactly at the right edge with `\r\n`: the cursor stays at the start of the next row.
    let mut s = Screen::new(10, 4);
    out(&mut s, b"$ 01234567\r\r\n");
    s.write_above("L", 3);
    assert_eq!(s.text_lines(), ["L", "$ 01234567"]);
    assert_eq!((s.col, s.row), (0, 2));
    out(&mut s, b"8\x1b[J\r\x1b[1A\x1b[9C");
    assert_eq!((s.col, s.row), (9, 1));
}

fn screen_of(text: &str) -> Screen {
    let mut s = Screen::new(10, 5);
    s.write(text);
    s
}

#[test]
fn cursor_moves_clamp_at_the_edges() {
    let mut s = screen_of("\x1b[3;4H");
    assert_eq!((s.col, s.row), (3, 2));
    for (seq, at) in [
        ("\x1b[A", (3, 1)),
        ("\x1b[9A", (3, 0)),
        ("\x1b[2B", (3, 2)),
        ("\x1b[99B", (3, 4)),
        ("\x1b[3C", (6, 4)),
        ("\x1b[99C", (9, 4)),
        ("\x1b[4D", (5, 4)),
        ("\x1b[99D", (0, 4)),
        ("\x1b[5G", (4, 4)),
        ("\x1b[2`", (1, 4)),
        ("\x1b[0G", (0, 4)),
        ("\x1b[2d", (0, 1)),
        ("\x1b[3a", (3, 1)),
        ("\x1b[2e", (3, 3)),
        ("\x1b[2F", (0, 1)),
        ("\x1b[E", (0, 2)),
        ("\x1b[H", (0, 0)),
        ("\x1b[0;0f", (0, 0)),
        ("\x1b[99;99H", (9, 4)),
    ] {
        s.write(seq);
        assert_eq!((s.col, s.row), at, "{seq:?}");
    }
    // A pending wrap counts as the last column: one left is the column before it, no wrap follows.
    let mut s = screen_of("0123456789\x1b[D");
    assert_eq!(s.col, 8);
    s.write("x");
    assert_eq!(s.text_lines(), ["01234567x9"]);
}

#[test]
fn erasing_lines_and_the_display() {
    let rows = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\ndddddddddd";
    let mut s = screen_of(rows);
    s.write("\x1b[2;5H\x1b[K");
    assert_eq!(s.text_lines()[1], "bbbb");
    s.write("\x1b[1K");
    assert_eq!(s.text_lines()[1], "");
    s.write("\x1b[3;5H\x1b[2K");
    assert_eq!(s.text_lines(), ["aaaaaaaaaa", "", "", "dddddddddd"]);
    let mut s = screen_of(rows);
    s.write("\x1b[2;5H\x1b[1J");
    assert_eq!(s.text_lines(), ["", "     bbbbb", "cccccccccc", "dddddddddd"]);
    s.write("\x1b[J");
    assert!(s.text_lines().is_empty());
    s.write("bb\x1b[2J");
    assert!(s.text_lines().is_empty());
    assert_eq!((s.col, s.row), (6, 1), "ED keeps the cursor (Linux)");
    let mut s = screen_of("1\n2\n3\n4\n5\n6\n7");
    assert_eq!(s.scrollback_len(), 2);
    s.write("\x1b[3J");
    assert_eq!((s.scrollback_len(), s.text_lines().len()), (0, 0), "ED 3 drops the scrollback too");
}

#[test]
fn erasing_inserting_and_deleting_characters() {
    let mut s = screen_of("0123456789\x1b[1;3H\x1b[2X");
    assert_eq!(s.text_lines(), ["01  456789"]);
    s.write("\x1b[3P");
    assert_eq!(s.text_lines(), ["0156789"]);
    s.write("\x1b[2@");
    assert_eq!(s.text_lines(), ["01  56789"]);
    s.write("\x1b[99@");
    assert_eq!(s.text_lines(), ["01"]);
    s.write("\x1b[99P\x1b[99X");
    assert_eq!((s.text_lines(), s.col), (alloc::vec![String::from("01")], 2));
    // Insert mode (IRM): typed text pushes the rest right.
    let s = screen_of("abcdef\x1b[1;2H\x1b[4hXY\x1b[4lZ");
    assert_eq!(s.text_lines(), ["aXYZcdef"], "Z overwrites again");
}

#[test]
fn inserting_and_deleting_lines_and_scrolling() {
    let rows = "a\nb\nc\nd\ne";
    let mut s = screen_of(rows);
    s.write("\x1b[2;1H\x1b[2L");
    assert_eq!(s.text_lines(), ["a", "", "", "b", "c"]);
    s.write("\x1b[M");
    assert_eq!(s.text_lines(), ["a", "", "b", "c"]);
    let mut s = screen_of(rows);
    s.write("\x1b[2S");
    assert_eq!(s.text_lines(), ["c", "d", "e"]);
    s.write("\x1b[T");
    assert_eq!(s.text_lines(), ["", "c", "d", "e"]);
    assert_eq!(s.scrollback_len(), 0, "SU, SD, IL and DL never fill the scrollback");
}

#[test]
fn scroll_region_line_feed_and_reverse_index() {
    let mut s = screen_of("a\nb\nc\nd\ne");
    s.write("\x1b[2;4r");
    assert_eq!((s.col, s.row), (0, 0), "DECSTBM homes the cursor");
    s.write("\x1b[4;1H\nX");
    assert_eq!(s.text_lines(), ["a", "c", "d", "X", "e"], "the line feed at the region's bottom scrolls only the region");
    assert_eq!(s.scrollback_len(), 0);
    s.write("\x1b[2;1H\x1bMY");
    assert_eq!(s.text_lines(), ["a", "Y", "c", "d", "e"], "RI at the region's top scrolls it down");
    s.write("\x1b[1;1H\x1bM");
    assert_eq!(s.row, 0, "RI above the region stops at the top");
    // Origin mode: positions count from the region's top and stay inside it.
    s.write("\x1b[?6h\x1b[1;1HO\x1b[99B");
    assert_eq!(s.text_lines()[1], "O");
    assert_eq!(s.row, 3);
    s.write("\x1b[?6l\x1b[r\x1b[5;1H\n");
    assert_eq!(s.scrollback_len(), 1, "the full region again scrolls into the scrollback");
    // A region of one row is refused.
    s.write("\x1b[3;3r\x1b[5;1H\n");
    assert_eq!(s.scrollback_len(), 2);
}

#[test]
fn saved_cursor_reset_and_cursor_visibility() {
    let mut s = screen_of("\x1b[2;3H\x1b[31m\x1b7\x1b[0m\x1b[5;5Hx\x1b8y");
    assert_eq!(s.visible_row(1)[2], Cell { ch: 'y', attr: 1 }, "ESC 8 brings back position and colour");
    s.write("\x1b[1;1H\x1b[s\x1b[4;4H\x1b[uz");
    assert_eq!(s.visible_row(0)[0].ch, 'z');
    assert!(s.cursor().is_some());
    s.write("\x1b[?25l");
    assert_eq!(s.cursor(), None);
    s.write("\x1b[?25h");
    assert_eq!(s.cursor(), Some((1, 0)));
    s.write("\x1b[?25l\x1b[2;4r\x1b[7m\x1bc");
    assert!(s.text_lines().is_empty());
    assert_eq!((s.cursor(), s.attr), (Some((0, 0)), screen::DEFAULT_ATTR), "ESC c resets modes, colours, cursor");
    s.write("\x1b[5;1H\n");
    assert_eq!(s.scrollback_len(), 1, "and the scroll region");
}

#[test]
fn reverse_video_and_more_colours() {
    let s = screen_of("\x1b[34;42m\x1b[7mR\x1b[27mN\x1b[7m\x1b[K");
    assert_eq!(s.visible_row(0)[0].attr, screen::attr(2, 4));
    assert_eq!(s.visible_row(0)[1].attr, screen::attr(4, 2));
    assert_eq!(s.visible_row(0)[2].attr >> 4, 4, "erased cells take the reversed background");
    let s = screen_of("\x1b[38;5;9ma\x1b[48;5;200mb\x1b[38;2;1;2;3mc\x1b[38:5:3md");
    let r = s.visible_row(0);
    assert_eq!((r[0].attr, r[1].attr, r[2].attr, r[3].attr), (9, 9, 9, 3));
    assert_eq!(s.text_lines(), ["abcd"]);
}

#[test]
fn alternate_screen_keeps_the_main_screen() {
    let mut s = screen_of("main 1\nmain 2\n$ ");
    s.write("\x1b[?1049h");
    assert!(s.text_lines().is_empty());
    for i in 0..20 {
        s.write(&alloc::format!("full {i}\n"));
    }
    assert_eq!(s.scrollback_len(), 0);
    s.write("\x1b[?1049l");
    assert_eq!(s.text_lines(), ["main 1", "main 2", "$"]);
    assert_eq!((s.col, s.row), (2, 2));
    s.write("\x1b[?47h\x1b[?47h\x1b[3;3Hx\x1b[?47l");
    assert_eq!(s.text_lines(), ["main 1", "main 2", "$"]);
    assert_eq!((s.col, s.row), (3, 2), "?47 does not restore the cursor");
}

#[test]
fn reports_for_the_terminal_input() {
    let mut s = screen_of("\x1b[3;7H\x1b[6n\x1b[5n\x1b[c\x1b[>c");
    assert_eq!(s.take_reply(), b"\x1b[3;7R\x1b[0n\x1b[?6c");
    assert!(s.take_reply().is_empty());
    for _ in 0..100 {
        s.write("\x1b[6n");
    }
    assert!(s.take_reply().len() <= 256, "reports nobody reads are bounded");
}

#[test]
fn unknown_and_hostile_sequences_print_nothing() {
    let mut s = screen_of("a\x1b]0;title\x07b\x1b]2;t\x1b\\c\x1b]P0123456d\x1b(Be\x1b#8f\x1b[!pg\x1b[2 qh\x1b[?1;2$pi");
    assert_eq!(s.text_lines(), ["abcdefghi"]);
    s.write("\x1b[4294967295;99999999999A\x1b[99999999999@\x1b[99999999999L\x1b[99999999999S\x1b[4294967295G");
    assert_eq!((s.col, s.row), (9, 0));
    s.write("\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17Hx\x1b[1;2\x1b[Hy");
    assert_eq!(s.visible_row(0)[0].ch, 'y', "ESC inside a sequence starts the next one");
    s.write("\x1b[20h\x1b[?1000h\x1b[?3l\x1b[g\x1b[q\x1b[3;3]");
    assert_eq!(s.cursor(), Some((1, 0)));
}

#[test]
fn autowrap_off_overwrites_the_last_column() {
    let s = screen_of("\x1b[?7l0123456789ABC\x1b[?7h");
    assert_eq!(s.text_lines(), ["012345678C"]);
}

/// A full-screen program's drawing (alternate screen, region scrolls, inserts) drawn incrementally gives the
/// same pixels as a full redraw.
#[test]
fn rendering_after_editing_sequences_equals_a_full_redraw() {
    let (w, h) = (320, 200);
    let font = Font::for_mode(Face::Vga, w, h);
    let mut s = Screen::new(w / font.w, h / font.h);
    let mut f = RamFrame::new(w, h);
    let mut last = None;
    let steps = [
        "shell line\n$ vi\n",
        "\x1b[?1049h\x1b[H\x1b[J",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\n",
        "\x1b[2;6r\x1b[6;1H\nX\x1b[2;1H\x1bMY\x1b[r",
        "\x1b[3;2H\x1b[2L\x1b[4;1H\x1b[M\x1b[1;1H\x1b[3@\x1b[5P\x1b[7mrev\x1b[m\x1b[2S",
        "\x1b[?25l",
        "\x1b[?25h\x1b[?1049l$ ",
    ];
    for st in steps {
        s.write(st);
        render::render(&mut f, &font, &mut s, &mut last);
        let mut fresh = RamFrame::new(w, h);
        s.full = true;
        let mut l2 = None;
        render::render(&mut fresh, &font, &mut s, &mut l2);
        assert!(f.pixels() == fresh.pixels(), "after {st:?} the incremental picture differs from a full redraw");
    }
}
