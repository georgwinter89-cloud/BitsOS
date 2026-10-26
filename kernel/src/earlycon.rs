//! Early screen console (desk): everything written to COM1 also appears on the GOP framebuffer until the real
//! console takes over, and again in a panic. The Mini-PC has no COM1, so without this an early hang or panic
//! leaves the screen at bitsboot's last line.
//!
//! No heap, no locks: it draws straight into the framebuffer bitsboot mapped (CP437 8x16 font, doubled at
//! high resolutions), wrapping to the top when the screen is full.

use bootinfo::{BootInfo, PixelFormat};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

static FONT: &[u8; 4096] = include_bytes!("../../crates/display/src/font/cp437-8x16.bin");

static BASE: AtomicU64 = AtomicU64::new(0);
static STRIDE: AtomicU32 = AtomicU32::new(0);
static COLS: AtomicU32 = AtomicU32::new(0);
static ROWS: AtomicU32 = AtomicU32::new(0);
static SCALE: AtomicU32 = AtomicU32::new(1);
static COL: AtomicU32 = AtomicU32::new(0);
static ROW: AtomicU32 = AtomicU32::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Starts mirroring COM1 output to the screen (a framebuffer with 32-bit pixels; otherwise nothing happens).
pub fn init(bi: &BootInfo<'_>) {
    let Some(fb) = bi.framebuffer() else { return };
    if fb.virt == 0 || fb.width == 0 || fb.height == 0 || matches!(fb.format, PixelFormat::None) {
        return;
    }
    let scale = if fb.width >= 1920 { 2 } else { 1 };
    BASE.store(fb.virt, Ordering::Relaxed);
    STRIDE.store(fb.stride, Ordering::Relaxed);
    SCALE.store(scale, Ordering::Relaxed);
    COLS.store(fb.width / (8 * scale), Ordering::Relaxed);
    ROWS.store(fb.height / (16 * scale), Ordering::Relaxed);
    // A green bar (rows 80..96, below bitsboot's probe bars): the kernel runs and reaches the screen, before any
    // text.
    for y in 80..96 {
        for x in 0..fb.width {
            px(x, y, 0x0000_C000);
        }
    }
    // Start below bitsboot's own lines (about a third of the screen), so both stay readable.
    ROW.store(ROWS.load(Ordering::Relaxed) / 3, Ordering::Relaxed);
    resume();
}

/// The real console has the screen now.
pub fn stop() {
    ACTIVE.store(false, Ordering::Release);
    hal::serial::set_mirror(None);
}

/// Back on (panic path).
pub fn resume() {
    if BASE.load(Ordering::Relaxed) == 0 {
        return;
    }
    ACTIVE.store(true, Ordering::Release);
    hal::serial::set_mirror(Some(put));
}

fn put(b: u8) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }
    let (cols, rows) = (COLS.load(Ordering::Relaxed), ROWS.load(Ordering::Relaxed));
    let (mut col, mut row) = (COL.load(Ordering::Relaxed), ROW.load(Ordering::Relaxed));
    match b {
        b'\r' => col = 0,
        b'\n' => {
            col = 0;
            row += 1;
        }
        _ => {
            if col >= cols {
                col = 0;
                row += 1;
            }
            if row >= rows {
                row = 0;
            }
            if col == 0 {
                clear_row(row, cols);
            }
            glyph(col, row, b);
            col += 1;
        }
    }
    if row >= rows {
        row = 0;
    }
    if b == b'\n' {
        clear_row(row, cols);
    }
    COL.store(col, Ordering::Relaxed);
    ROW.store(row, Ordering::Relaxed);
}

fn px(x: u32, y: u32, v: u32) {
    let off = (y as u64 * STRIDE.load(Ordering::Relaxed) as u64 + x as u64) * 4;
    // SAFETY: x < width and y < height of the framebuffer bitsboot mapped at BASE (record checked in `init`;
    // callers stay inside COLS x ROWS cells); a volatile 4-byte store into device memory.
    unsafe { core::ptr::write_volatile((BASE.load(Ordering::Relaxed) + off) as *mut u32, v) };
}

fn glyph(col: u32, row: u32, ch: u8) {
    let s = SCALE.load(Ordering::Relaxed);
    let (x0, y0) = (col * 8 * s, row * 16 * s);
    let rows = &FONT[ch as usize * 16..ch as usize * 16 + 16];
    for (gy, bits) in rows.iter().enumerate() {
        for gx in 0..8u32 {
            let on = bits & (0x80 >> gx) != 0;
            let v = if on { 0x00FF_FFFF } else { 0 };
            for dy in 0..s {
                for dx in 0..s {
                    px(x0 + gx * s + dx, y0 + gy as u32 * s + dy, v);
                }
            }
        }
    }
}

fn clear_row(row: u32, cols: u32) {
    for c in 0..cols {
        glyph(c, row, b' ');
    }
}
