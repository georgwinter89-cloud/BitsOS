//! Host tests of `stock`: a prepared write makes no memory in the model, and writes what an unprepared one does.

use super::*;
use alloc::format;
use alloc::string::String;

/// A write as `vt::edit` does it; returns how often the stock had to grow.
fn put(s: &mut Screen, text: &str, above: bool) -> u32 {
    let mut stock = Stock::new();
    let mut grown = 0;
    while !s.prepare(&mut stock, text, above) {
        stock.grow();
        grown += 1;
        assert!(grown <= 2, "the same screen asked a third time for {text:?}");
    }
    let (rows, cap) = (s.hist.len(), s.hist.capacity());
    if above {
        s.write_above(text, 3);
    } else {
        s.write(text);
    }
    assert_eq!(s.took_unprepared(), 0, "the model made memory itself for {text:?} (above {above})");
    assert!(s.hist.capacity() == cap || s.hist.len() < rows, "the list of rows grew inside the write of {text:?}");
    s.unprepare(&mut stock);
    assert!(s.spare.is_empty(), "rows stayed in the screen");
    grown
}

fn same(a: &Screen, b: &Screen, what: &str) {
    assert_eq!(a.text_lines(), b.text_lines(), "{what}: text");
    assert_eq!((a.col, a.row, a.scrollback_len(), a.view), (b.col, b.row, b.scrollback_len(), b.view), "{what}: cursor and scrollback");
    assert_eq!(a.hist, b.hist, "{what}: rows and colours");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

/// A piece of terminal output: text of any length, line feeds, tabs, and the sequences that move the cursor,
/// feed lines, change what scrolls and switch screens.
fn piece(r: &mut Rng) -> String {
    const SEQ: [&str; 16] = [
        "\n", "\r\n", "\u{b}", "\t", "\r", "\u{8}", "\x1b[999G", "\x1bD", "\x1bE", "\x1bM", "\x1b[H", "\x1b[99;1H", "\x1b[3;7r", "\x1b[r", "\x1b[?1049h",
        "\x1b[?1049l",
    ];
    let mut s = String::new();
    for _ in 0..1 + r.next(6) {
        match r.next(4) {
            0 => s.push_str(SEQ[r.next(SEQ.len() as u64) as usize]),
            1 => s.push_str(&"\n".repeat(r.next(30) as usize)),
            _ => s.extend((0..r.next(120)).map(|i| (b'a' + (i % 26) as u8) as char)),
        }
    }
    s
}

/// 4000 random writes, half of them log lines above the open line, on a screen whose scrollback fills and
/// overflows: the prepared screen never makes memory itself and stays equal to a twin written without a stock.
#[test]
fn a_prepared_write_makes_no_memory_and_writes_the_same() {
    let (mut a, mut b) = (Screen::new(40, 10), Screen::new(40, 10));
    a.set_limit(300);
    b.set_limit(300);
    let mut r = Rng(7);
    let mut grown = 0;
    for i in 0..4000 {
        let text = piece(&mut r);
        let above = r.next(2) == 0;
        grown += put(&mut a, &text, above);
        if above {
            b.write_above(&text, 3);
        } else {
            b.write(&text);
        }
        same(&a, &b, &format!("write {i} {text:?} (above {above})"));
        if i == 2000 {
            // A new font: rows of another width.
            a.resize(57, 12);
            b.resize(57, 12);
        }
    }
    assert!(grown > 0 && a.scrollback_len() == 300, "the scrollback did not fill ({grown} grows)");
    assert!(b.took_unprepared() > 0, "the twin's count of its own allocations");
}

/// Plain log lines on an empty screen: one row per line that scrolls, none for a line that does not, and the
/// list of rows doubles a few times instead of growing at every line.
#[test]
fn log_lines_bring_one_row_each_and_the_list_doubles() {
    let mut s = Screen::new(80, 25);
    let (mut asked, mut lists) = (0, 0);
    for i in 0..1000 {
        let line = format!("[{i:5}.000000] I sched thread.spawn id={i}");
        let mut stock = Stock::new();
        while !s.prepare(&mut stock, &line, true) {
            asked += stock.want.rows;
            lists += u32::from(stock.want.hist > 0);
            assert!(stock.want.rows <= 1, "line {i}: {} rows asked for one log line", stock.want.rows);
            stock.grow();
        }
        s.write_above(&line, 7);
        s.unprepare(&mut stock);
    }
    assert_eq!(s.took_unprepared(), 0);
    // The first 24 lines fill the screen without a scroll.
    assert_eq!(asked, 1000 - 24);
    assert_eq!(s.scrollback_len(), 1000 - 24);
    assert!((1..=7).contains(&lists), "the list of rows was replaced {lists} times for 1000 lines");
}

/// With the window full nothing is asked for and nothing is made, whatever is written: the oldest row is the
/// new one. This is the state a running machine is in.
#[test]
fn a_full_scrollback_needs_no_memory_at_all() {
    let mut s = Screen::new(60, 20);
    s.set_limit(100);
    put(&mut s, &"\n".repeat(200), false);
    // The buffer for the open line is made once, by the first log line, and kept.
    assert_eq!(put(&mut s, "the first log line", true), 1);
    assert_eq!(s.scrollback_len(), 100);
    let cap = s.hist.capacity();
    let rows: alloc::vec::Vec<*const Cell> = s.hist.iter().map(|r| r.as_ptr()).collect();
    for i in 0..500 {
        let line = format!("line {i} {}", "x".repeat(i % 150));
        let mut stock = Stock::new();
        assert!(s.prepare(&mut stock, &line, i % 2 == 0), "line {i}: memory asked for with the window full");
        assert!(stock.rows.is_empty() && stock.hist.capacity() == 0 && stock.open.capacity() == 0);
        if i % 2 == 0 {
            s.write_above(&line, 2);
        } else {
            s.write(&line);
            s.write("\n");
        }
        s.unprepare(&mut stock);
    }
    assert_eq!(s.took_unprepared(), 0);
    assert_eq!(s.hist.capacity(), cap);
    let mut now: alloc::vec::Vec<*const Cell> = s.hist.iter().map(|r| r.as_ptr()).collect();
    let mut before = rows;
    now.sort();
    before.sort();
    assert_eq!(now, before, "the rows are the ones the window had: none was made, none freed");
}

/// The screen changed between the look and the write (another writer filled rows, the font changed): what was
/// made for the old state is not used where it does not fit; the next look says what is missing now.
#[test]
fn a_stock_made_for_another_screen_state_is_not_used() {
    let mut s = Screen::new(40, 5);
    let text = "\n".repeat(8);
    let mut stock = Stock::new();
    assert!(!s.prepare(&mut stock, &text, false));
    stock.grow();
    // A wider font meanwhile: the rows made are too short.
    s.resize(64, 5);
    assert!(!s.prepare(&mut stock, &text, false), "rows of 40 cells taken for a screen of 64");
    assert_eq!(stock.want.cols, 64);
    stock.grow();
    // Another writer scrolled meanwhile and used up the list's room.
    let mut other = Stock::new();
    let lines = "\n".repeat(40);
    while !s.prepare(&mut other, &lines, false) {
        other.grow();
    }
    s.write(&lines);
    s.unprepare(&mut other);
    while !s.prepare(&mut stock, &text, false) {
        stock.grow();
    }
    s.write(&text);
    s.unprepare(&mut stock);
    assert_eq!(s.took_unprepared(), 0);
    // The first four feeds only move the cursor down the five rows.
    assert_eq!(s.scrollback_len(), 8 + 40 - 4);
}
