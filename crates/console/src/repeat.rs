//! Key repeat by the console's own timer, not the device (V1 `usb/hidkbd.asm:6-7`, spec usb.md §4.1): the newest
//! key held repeats after 500 ms, then every 33 ms; modifiers and lock keys never repeat; releasing the key (or
//! pressing another) stops it. Injected keys (MCP `send_keys`) do not repeat: MCP sends whole strokes, and a
//! client that died between its key down and key up must not type on forever.

pub const DELAY_NS: u64 = 500_000_000;
pub const RATE_NS: u64 = 33_000_000;

use core::sync::atomic::{AtomicU32, Ordering};

/// The console's repeat in ms (KDKBDREP changes it; the VT uses it from the next key press on).
static DELAY_MS: AtomicU32 = AtomicU32::new((DELAY_NS / 1_000_000) as u32);
static RATE_MS: AtomicU32 = AtomicU32::new((RATE_NS / 1_000_000) as u32);

/// (delay ms, period ms) of the console's repeat.
pub fn rate_ms() -> (u32, u32) {
    (DELAY_MS.load(Ordering::Acquire), RATE_MS.load(Ordering::Acquire))
}
pub fn set_rate_ms(delay: u32, period: u32) {
    DELAY_MS.store(delay.max(1), Ordering::Release);
    RATE_MS.store(period.max(1), Ordering::Release);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Repeat {
    key: Option<u16>,
    next_ns: u64,
    pub delay_ns: u64,
    pub rate_ns: u64,
}

impl Repeat {
    pub const fn new() -> Repeat {
        Repeat { key: None, next_ns: 0, delay_ns: DELAY_NS, rate_ns: RATE_NS }
    }
    /// A key went down at `now`.
    pub fn press(&mut self, code: u16, injected: bool, now: u64) {
        if injected || crate::keymap::is_modifier(code) {
            // A modifier pressed while a key repeats does not stop it (Shift held later shifts the repeats).
            if injected {
                self.key = None;
            }
            return;
        }
        self.key = Some(code);
        self.next_ns = now.saturating_add(self.delay_ns);
    }
    /// A key went up.
    pub fn release(&mut self, code: u16) {
        if self.key == Some(code) {
            self.key = None;
        }
    }
    pub fn stop(&mut self) {
        self.key = None;
    }
    /// When the next repeat is due (None: nothing repeats).
    pub fn deadline(&self) -> Option<u64> {
        self.key.map(|_| self.next_ns)
    }
    /// The key to repeat now, if one is due (each call gives at most one repeat; the next is `rate` later). A
    /// thread that slept long does not burst: the next one is due a full period after `now`.
    pub fn due(&mut self, now: u64) -> Option<u16> {
        let k = self.key?;
        if now < self.next_ns {
            return None;
        }
        self.next_ns = now.saturating_add(self.rate_ns);
        Some(k)
    }
    pub fn key(&self) -> Option<u16> {
        self.key
    }
}
