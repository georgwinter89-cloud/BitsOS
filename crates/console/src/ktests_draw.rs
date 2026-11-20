//! GP-START: what one console frame costs at the display's size, split into drawing (cells into a RAM frame) and
//! copying (the RAM frame to the screen), so a late `input latency.over_budget path=console` can be told apart.
//! The drawing runs on a private model and frame (the screen shows nothing of it); the copy presents the live frame
//! whole again, which changes no pixel. Minimum of a few rounds: a busy host only ever adds time.

use crate::render;
use crate::screen::{self, Cell, Screen};
use display::RamFrame;
use ktest::{TestResult, kassert, ktest};

const ROUNDS: usize = 5;
const BUDGET_US: u64 = crate::latency::BUDGET_NS / 1000;
/// A report-like row: German text with umlauts (outside ASCII the old per-pixel lookup searched a table).
const LINE: &str = "Gerät 80:14.0 USB-Steuerung läuft, Anschlüsse 24, Größe überprüft (ÄÖÜß) € ";

fn min_us(mut f: impl FnMut()) -> u64 {
    let mut best = u64::MAX;
    for _ in 0..ROUNDS {
        let t0 = time::now_ns();
        f();
        best = best.min(time::now_ns().saturating_sub(t0) / 1000);
    }
    best
}

fn timing() -> TestResult {
    let (w, h) = display::with_frame(|f| (f.width, f.height)).ok_or("no display")?;
    let font = crate::vt::font().ok_or("no console")?;
    let (cols, rows) = (w / font.w, h / font.h);
    kassert!(cols > 0 && rows > 1, "a {w}x{h} display holds no text grid");
    let mut s = Screen::new(cols, rows);
    let text: alloc::string::String = LINE.chars().cycle().take(cols as usize - 1).collect();
    for r in 0..rows {
        s.write(&text);
        if r + 1 < rows {
            s.write("\r\n");
        }
    }
    let mut frame = RamFrame::try_new(w, h).ok_or("no memory for a second frame")?;
    let mut last = None;
    let full = min_us(|| {
        s.full = true;
        render::render(&mut frame, &font, &mut s, &mut last);
        frame.take_damage();
    });
    let cell = min_us(|| {
        render::draw_cell(&mut frame, &font, 2, 2, Cell { ch: 'ä', attr: screen::DEFAULT_ATTR }, false);
        frame.take_damage();
    });
    let scroll = min_us(|| {
        frame.scroll_up(font.h, screen::PALETTE[screen::BG_INDEX as usize]);
        render::draw_row(&mut frame, &font, &s, rows - 1);
        frame.take_damage();
    });
    let copy = min_us(|| {
        display::with_frame(|f| f.mark_all());
        display::present();
    });
    klog::event!(Info, "console", "draw.timing", width = w, height = h, cell_w = font.w, cell_h = font.h,
        full_us = full, cell_us = cell, scroll_us = scroll, copy_us = copy, budget_us = BUDGET_US);
    kassert!(full + copy <= BUDGET_US, "a whole screen took {full} us to draw and {copy} us to copy (budget {BUDGET_US})");
    kassert!(scroll + copy <= BUDGET_US, "a scroll took {scroll} us to draw and {copy} us to copy (budget {BUDGET_US})");
    Ok(())
}
ktest!("in.draw.timing", Screen, timing);
