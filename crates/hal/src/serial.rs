//! COM1 (desk): the log line QEMU, the gate and xtask parse. CONFIG `serial=auto|on|off` (D1, 19.10.): `auto`
//! (the default) uses the port only under a hypervisor; on bare metal nobody listens to COM1 (logs go to the
//! mirror, the screen and MCP), and the Mini-PC's FCH UART answers at 0x3F8 without a connector: every byte
//! there waits ~87 us for the line. `off`, or `auto` on bare metal, touches no port at all; bytes still reach
//! the mirror hook (the early screen console).
//!
//! No writer keeps interrupts off while it waits for the line: a slow UART is waited for with interrupts as the
//! caller had them, and only the burst into an empty FIFO (and taking the line flag) runs masked. Under QEMU the
//! transmitter is always empty, so a whole line still goes out in one masked section, as before.

use crate::portio::raw;
use core::iter::Peekable;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// A second place every byte goes (the kernel's early screen console on machines without COM1, like the
/// Mini-PC). 0 = none; otherwise a `fn(u8)`.
static MIRROR: AtomicUsize = AtomicUsize::new(0);

const COM1: u16 = 0x3F8;
const LSR_THR_EMPTY: u8 = 0x20;
/// Line status reads before a burst goes out anyway (a UART that stopped draining must not hang a writer).
const THR_WAIT_READS: u32 = 100_000;
/// Looks at another CPU's line flag before writing anyway (a holder that never lets go: a stopped CPU).
const LINE_WAIT_SPINS: u32 = 4_000_000;

/// The port is in use: a UART answered the scratch test and the mode wants it.
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Bytes one look at an empty transmitter may write (16 with the 16550 FIFO, else 1).
static BURST: AtomicUsize = AtomicUsize::new(1);

/// CONFIG `serial=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The port under a hypervisor (QEMU), none on bare metal.
    Auto,
    /// The port wherever a UART answers (a real cable on bare metal).
    On,
    /// Never a port.
    Off,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "auto" | "" => Some(Mode::Auto),
            "on" => Some(Mode::On),
            "off" => Some(Mode::Off),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::On => "on",
            Mode::Off => "off",
        }
    }
}

/// Whether `mode` wants the port on a machine with (`hypervisor`) or without the CPUID hypervisor bit.
pub fn wanted(mode: Mode, hypervisor: bool) -> bool {
    match mode {
        Mode::On => true,
        Mode::Off => false,
        Mode::Auto => hypervisor,
    }
}

/// The CPUID hypervisor bit (leaf 1, ECX bit 31). No allocation: it runs before the heap.
pub fn under_hypervisor() -> bool {
    crate::cpu::cpuid(1, 0).ecx & (1 << 31) != 0
}

/// Whether the port is in use (a usable sink: a UART answered and the mode wants it).
pub fn present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// Whether bytes go anywhere: the port, or the mirror hook.
pub fn has_output() -> bool {
    present() || MIRROR.load(Ordering::Acquire) != 0
}

/// Sends every later byte also to `f` (`None` stops it). The hook must not lock or allocate.
pub fn set_mirror(f: Option<fn(u8)>) {
    MIRROR.store(f.map_or(0, |f| f as usize), Ordering::Release);
}

/// The first decision, at kernel entry, before CONFIG is read: `auto`. On bare metal no port is touched.
pub fn init() {
    apply(wanted(Mode::Auto, under_hypervisor()));
}

/// CONFIG `serial=` once it is read (only the boot CPU runs then). Returns whether the port is in use.
pub fn configure(mode: Mode) -> bool {
    apply(wanted(mode, under_hypervisor()));
    present()
}

fn apply(want: bool) {
    if !want {
        PRESENT.store(false, Ordering::Release);
        return;
    }
    if present() {
        return;
    }
    // The scratch register keeps what is written to it on every 16550; nothing there reads back the pattern.
    for pat in [0x5A, 0xA5] {
        raw::outb(COM1 + 7, pat);
        if raw::inb(COM1 + 7) != pat {
            return;
        }
    }
    raw::outb(COM1 + 1, 0x00); // no interrupts
    raw::outb(COM1 + 3, 0x80); // DLAB
    raw::outb(COM1, 0x01); // 115200 baud
    raw::outb(COM1 + 1, 0x00);
    raw::outb(COM1 + 3, 0x03); // 8N1
    raw::outb(COM1 + 2, 0xC7); // FIFO on, cleared
    raw::outb(COM1 + 4, 0x03);
    // IIR bits 7:6 = 11: the FIFO exists and is on (a 16450 has none).
    let fifo = raw::inb(COM1 + 2) & 0xC0 == 0xC0;
    BURST.store(if fifo { 16 } else { 1 }, Ordering::Release);
    PRESENT.store(true, Ordering::Release);
}

/// `s` as it goes out on the line: `\r` before every `\n`.
pub(crate) struct Crlf<'a> {
    s: &'a [u8],
    i: usize,
    lf: bool,
}

impl<'a> Crlf<'a> {
    pub(crate) fn new(s: &'a [u8]) -> Self {
        Crlf { s, i: 0, lf: false }
    }
}

impl Iterator for Crlf<'_> {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        if self.lf {
            self.lf = false;
            return Some(b'\n');
        }
        let b = *self.s.get(self.i)?;
        self.i += 1;
        if b == b'\n' {
            self.lf = true;
            return Some(b'\r');
        }
        Some(b)
    }
}

fn thr_empty() -> bool {
    // A missing UART reads 0xFF, which also says "empty": no one waits on it.
    raw::inb(COM1 + 5) & LSR_THR_EMPTY != 0
}

/// Sends `bytes` to the port in bursts, each into an empty transmitter. Called with interrupts off; `open` =
/// the caller had them on: they are on again while the line drains, and off for each burst. `each_burst`: also
/// let interrupts in between two bursts when the transmitter is already empty (plain byte streams such as a
/// program's console output, which may be long; a line keeps one masked section where the port is fast).
fn port_out(bytes: &mut Peekable<impl Iterator<Item = u8>>, open: bool, each_burst: bool) {
    let burst = BURST.load(Ordering::Acquire).max(1);
    let mut first = true;
    while bytes.peek().is_some() {
        if open && each_burst && !first {
            crate::cpu::enable_interrupts();
            // One instruction after `sti` (its shadow), so a pending interrupt is taken before the `cli`.
            core::hint::spin_loop();
            crate::cpu::disable_interrupts();
        }
        first = false;
        if !thr_empty() {
            if open {
                crate::cpu::enable_interrupts();
            }
            for _ in 0..THR_WAIT_READS {
                if thr_empty() {
                    break;
                }
                core::hint::spin_loop();
            }
            crate::cpu::disable_interrupts();
        }
        for b in bytes.by_ref().take(burst) {
            raw::outb(COM1, b);
        }
    }
}

fn mirror_out(s: &[u8]) {
    let m = MIRROR.load(Ordering::Acquire);
    if m == 0 {
        return;
    }
    // SAFETY: MIRROR only ever holds 0 or a `fn(u8)` stored by `set_mirror`.
    let f: fn(u8) = unsafe { core::mem::transmute::<usize, fn(u8)>(m) };
    for b in Crlf::new(s) {
        f(b);
    }
}

/// Bytes of a program's stream one take of the line flag covers (about 22 ms at 115200 baud), so a long write
/// lets kernel lines in between and a waiting line writer is never held for long.
const CHUNK_BYTES: usize = 256;

/// `s` to the mirror and, if in use, the port (`\n` as `\r\n`), under the line flag per chunk: a chunk ends
/// after a newline or after `CHUNK_BYTES`, and the flag is let go between chunks. So a kernel line is never cut
/// by these bytes; they may only be split between chunks by whole lines (Linux takes `port->lock` per write,
/// serial_core.c / 8250_port.c). Interrupts are masked only per burst.
pub fn write_bytes(s: &[u8]) {
    let mut rest = s;
    while !rest.is_empty() {
        let end = rest.iter().take(CHUNK_BYTES).position(|&b| b == b'\n').map(|i| i + 1).unwrap_or(rest.len().min(CHUNK_BYTES));
        let (chunk, tail) = rest.split_at(end);
        rest = tail;
        let open = crate::cpu::interrupts_enabled();
        let held = take_line(open);
        mirror_out(chunk);
        if present() {
            port_out(&mut Crlf::new(chunk).peekable(), open, true);
        }
        if held {
            LINE.store(0, Ordering::Release);
        }
        if open {
            crate::cpu::enable_interrupts();
        }
    }
}

/// `write_bytes` without the line flag: the panic and NMI paths (klog::emergency), which must never wait for a
/// CPU that may be stopped (like Linux's `oops_in_progress` trylock in 8250_port.c).
pub fn write_bytes_unlocked(s: &[u8]) {
    mirror_out(s);
    if !present() {
        return;
    }
    let open = crate::cpu::interrupts_enabled();
    crate::cpu::disable_interrupts();
    port_out(&mut Crlf::new(s).peekable(), open, true);
    if open {
        crate::cpu::enable_interrupts();
    }
}

pub fn write_byte(b: u8) {
    write_bytes(&[b]);
}

/// Whole lines from several CPUs (klog's lines, the ktest protocol lines xtask parses) stay apart: the CPU tag
/// (index + 1) of the line's writer, 0 = free.
static LINE: AtomicUsize = AtomicUsize::new(0);

/// Takes the line flag: tried with interrupts off only around the attempt, waited for with them as the caller
/// had them (`open`); a holder that never lets go (a stopped CPU) only costs the bounded wait. A writer that
/// finds its own CPU's tag (an interrupt that came while this CPU waited for the UART) goes on at once. Returns
/// with interrupts off; true when this call took the flag (the caller lets it go).
fn take_line(open: bool) -> bool {
    for _ in 0..LINE_WAIT_SPINS {
        let seen = LINE.load(Ordering::Relaxed);
        if seen == 0 || seen == tag() {
            crate::cpu::disable_interrupts();
            let me = tag();
            match LINE.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(o) if o == me => return false,
                Err(_) => {}
            }
            if open {
                crate::cpu::enable_interrupts();
            }
        }
        core::hint::spin_loop();
    }
    crate::cpu::disable_interrupts();
    false
}

/// Writes `s` and a newline as one line under the line flag (`take_line`).
pub fn write_line(s: &[u8]) {
    let open = crate::cpu::interrupts_enabled();
    let held = take_line(open);
    mirror_out(s);
    mirror_out(b"\n");
    if present() {
        port_out(&mut Crlf::new(s).chain(Crlf::new(b"\n")).peekable(), open, false);
    }
    if held {
        LINE.store(0, Ordering::Release);
    }
    if open {
        crate::cpu::enable_interrupts();
    }
}

fn tag() -> usize {
    crate::percpu::this_cpu() + 1
}

pub struct Writer;

/// The panic report's writer: no line flag (`write_bytes_unlocked`).
impl core::fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        write_bytes_unlocked(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_uses_the_port_only_under_a_hypervisor() {
        assert!(wanted(Mode::Auto, true));
        assert!(!wanted(Mode::Auto, false), "bare metal: the Mini-PC's FCH UART has no connector");
        assert!(wanted(Mode::On, false) && wanted(Mode::On, true));
        assert!(!wanted(Mode::Off, true) && !wanted(Mode::Off, false));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse(" auto "), Some(Mode::Auto));
        assert_eq!(Mode::parse(""), Some(Mode::Auto));
        assert_eq!(Mode::parse("on"), Some(Mode::On));
        assert_eq!(Mode::parse("off"), Some(Mode::Off));
        assert_eq!(Mode::parse("maybe"), None);
        for m in [Mode::Auto, Mode::On, Mode::Off] {
            assert_eq!(Mode::parse(m.name()), Some(m));
        }
    }

    #[test]
    fn line_feeds_go_out_as_crlf() {
        let out: alloc::vec::Vec<u8> = Crlf::new(b"a\nb\n\n").collect();
        assert_eq!(out, b"a\r\nb\r\n\r\n");
        assert_eq!(Crlf::new(b"").count(), 0);
        let both: alloc::vec::Vec<u8> = Crlf::new(b"KTEST ok x").chain(Crlf::new(b"\n")).collect();
        assert_eq!(both, b"KTEST ok x\r\n");
    }

    #[test]
    fn no_port_without_a_uart() {
        // Host portio reads 0xFF: the scratch test fails, so even `on` leaves the port unused.
        assert!(!configure(Mode::On));
        assert!(!configure(Mode::Auto));
        write_line(b"nothing happens"); // no port, no mirror: returns
        assert!(!has_output());
    }
}
