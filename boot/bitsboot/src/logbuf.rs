//! Log lines go to the UEFI console (screen, and serial where the firmware mirrors it) and into a growing
//! buffer that becomes the LOADER_LOG record. After ExitBootServices logging is off; the few lines after it go
//! straight to COM1 (`serial_line`).

use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

struct Buf(UnsafeCell<Vec<u8>>);
// SAFETY: the loader runs on one CPU at TPL_APPLICATION and nothing logs from a firmware callback, so the
// buffer is never touched from two places at once.
unsafe impl Sync for Buf {}

static BUF: Buf = Buf(UnsafeCell::new(Vec::new()));
static CONSOLE: AtomicBool = AtomicBool::new(true);

struct Logger;
static LOGGER: Logger = Logger;

pub fn init() {
    let _ = ::log::set_logger(&LOGGER);
    ::log::set_max_level(::log::LevelFilter::Info);
}

/// Boot services (and with them the console and the allocator) end now.
pub fn console_off() {
    CONSOLE.store(false, Ordering::SeqCst);
}

/// ExitBootServices failed: the console is usable again for the error screen.
pub fn console_back() {
    CONSOLE.store(true, Ordering::SeqCst);
}

pub fn console_on() -> bool {
    CONSOLE.load(Ordering::SeqCst)
}

fn buf() -> &'static mut Vec<u8> {
    // SAFETY: see `Sync` above; callers never keep two of these references alive at once.
    unsafe { &mut *BUF.0.get() }
}

/// Everything logged so far.
pub fn text() -> &'static [u8] {
    buf()
}

pub fn len() -> usize {
    buf().len()
}

struct ToBuf;
impl Write for ToBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = buf();
        // Out of memory only loses log text; the boot goes on.
        if b.try_reserve(s.len()).is_ok() {
            b.extend_from_slice(s.as_bytes());
        }
        Ok(())
    }
}

impl ::log::Log for Logger {
    fn enabled(&self, _: &::log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &::log::Record) {
        if !console_on() {
            return;
        }
        let prefix = match r.level() {
            ::log::Level::Error => "error: ",
            ::log::Level::Warn => "warning: ",
            _ => "",
        };
        let _ = writeln!(ToBuf, "{prefix}{}", r.args());
        uefi::system::with_stdout(|o| {
            let _ = writeln!(o, "{prefix}{}", r.args());
        });
    }
    fn flush(&self) {}
}

const COM1: u16 = 0x3F8;

fn outb(port: u16, v: u8) {
    // SAFETY: COM1's data register; a write to an absent UART is ignored by the chipset.
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let v: u8;
    // SAFETY: COM1's line status register; reading it has no side effect.
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack)) };
    v
}

/// One line straight to COM1 (after ExitBootServices, no console exists). Uses the UART as the firmware left
/// it; waits a bounded time per byte, so a machine without COM1 only loses the text.
pub fn serial_line(parts: &[&[u8]]) {
    // The Mini-PC has no COM1: no UART keeps a scratch pattern, and then every byte would wait out its bound.
    for pat in [0x5Au8, 0xA5] {
        outb(COM1 + 7, pat);
        if inb(COM1 + 7) != pat {
            return;
        }
    }
    for &b in parts.iter().flat_map(|p| p.iter()).chain(b"\r\n".iter()) {
        for _ in 0..100_000 {
            if inb(COM1 + 5) & 0x20 != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        outb(COM1, b);
    }
}

/// A number as decimal text without allocating.
pub fn dec(mut v: u64, out: &mut [u8; 20]) -> &[u8] {
    let mut i = out.len();
    loop {
        i -= 1;
        out[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            return &out[i..];
        }
    }
}
