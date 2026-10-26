//! Keyboard LEDs from the console's lock state (`console::evdev` toggles Caps/Num/Scroll Lock). The sink's
//! listener only wakes the driver's worker thread (`worker`), because it runs in whatever context pushed the
//! lock key (a USB event thread must not sleep). The worker reads the lock state itself when it sends, so two
//! changes on two CPUs can never leave an older state on the keyboards. It sends the output reports: on the
//! interrupt OUT endpoint when the interface has one, else SET_REPORT(Output) on endpoint 0 (Linux
//! `__usbhid_submit_report` with `hid_submit_out`/`hid_submit_ctrl`,
//! linux-6.6/drivers/hid/usbhid/hid-core.c:335-420, 521-640). A new keyboard gets the current state at once
//! (Linux sets the LEDs of boot keyboards at start: "some keyboards don't work until their LEDs have been
//! set", hid-core.c:1179-1189). A report that fails is sent again on the worker's next tick (1 s), up to
//! `TRIES` times per lock state; then it waits for the next lock change.

use crate::iface::HidIface;
use crate::worker;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::evdev;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use hidparse::InputMap;

/// `sent`/`tried` before the first report went out.
const NEVER: u32 = u32::MAX;
const SEND_TIMEOUT_NS: u64 = 1_000_000_000;
/// Sends of one lock state before the worker waits for the next change (a retry bound, not a count of things).
pub const TRIES: u32 = 4;

pub const STATUS_NONE: u8 = 0;
pub const STATUS_OK: u8 = 1;
pub const STATUS_FAILED: u8 = 2;

pub struct Leds {
    /// Groups of the interface with LED outputs.
    groups: Vec<usize>,
    /// The lock state the worker last saw.
    pub want: AtomicU32,
    /// The state the keyboard shows (last report sent without error), NEVER before.
    pub sent: AtomicU32,
    /// The state of the last send, and how many sends of it failed in a row.
    tried: AtomicU32,
    pub failures: AtomicU32,
    /// Result of the last send (`STATUS_*`).
    pub status: AtomicU8,
    pub sends: AtomicU64,
    listener: AtomicU64,
}

impl Leds {
    pub fn new(map: &InputMap) -> Leds {
        Leds {
            groups: map.groups.iter().enumerate().filter(|(_, g)| !g.leds.is_empty()).map(|(i, _)| i).collect(),
            want: AtomicU32::new(0),
            sent: AtomicU32::new(NEVER),
            tried: AtomicU32::new(NEVER),
            failures: AtomicU32::new(0),
            status: AtomicU8::new(STATUS_NONE),
            sends: AtomicU64::new(0),
            listener: AtomicU64::new(0),
        }
    }
    pub fn present(&self) -> bool {
        !self.groups.is_empty()
    }
}

/// Listens to the lock state for a keyboard with LEDs and has the current state sent.
pub fn attach(h: &Arc<HidIface>) {
    if !h.leds.present() {
        return;
    }
    let sink = evdev::sink();
    // The bits are not used: the worker reads the state when it sends (see the module doc).
    let id = sink.add_led_listener(Arc::new(|_bits| worker::kick()));
    h.leds.listener.store(id, Ordering::Release);
    h.leds.want.store(sink.locks(), Ordering::Release);
    worker::kick();
}

pub fn detach(h: &HidIface) {
    let id = h.leds.listener.swap(0, Ordering::AcqRel);
    if id != 0 {
        evdev::sink().remove_led_listener(id);
    }
}

/// The worker's part for one interface: sends the current lock state unless the keyboard shows it already or
/// its sends failed `TRIES` times.
pub fn service(h: &HidIface) {
    let l = &h.leds;
    if !l.present() || l.listener.load(Ordering::Acquire) == 0 {
        return;
    }
    let want = evdev::sink().locks();
    l.want.store(want, Ordering::Release);
    if l.sent.load(Ordering::Acquire) == want {
        return;
    }
    if l.tried.swap(want, Ordering::AcqRel) != want {
        l.failures.store(0, Ordering::Release);
    } else if l.failures.load(Ordering::Acquire) >= TRIES {
        return;
    }
    match send(h, want) {
        Ok(()) => {
            l.sent.store(want, Ordering::Release);
            l.failures.store(0, Ordering::Release);
        }
        Err(e) => {
            let n = l.failures.fetch_add(1, Ordering::AcqRel) + 1;
            if n >= TRIES {
                klog::event!(Warn, "input", "hid.leds_given_up", device = h.dev.path(), interface = h.iface,
                    leds = klog::Hex(want as u64), tries = n, error = e.as_str(); "tried again at the next lock change");
            }
        }
    }
}

/// Sends the output reports with LEDs of every LED group (`bits` = `1 << LED_*`).
pub fn send(h: &HidIface, bits: u32) -> Result<(), String> {
    let mut failed = None;
    let mut n = 0usize;
    for &g in &h.leds.groups {
        for r in hidparse::led_reports(&h.desc, &h.map, g, |code| code < 32 && bits >> code & 1 != 0) {
            let res = match h.out_ep {
                Some(ep) => h.dev.out_bytes(ep, &r.bytes, SEND_TIMEOUT_NS).map(|_| ()),
                None => {
                    // `led_reports` leaves out reports longer than u16::MAX bytes.
                    let len = r.bytes.len() as u16;
                    h.dev.control_out(crate::class::set_report_output(h.iface, r.id, len), &r.bytes).map(|_| ())
                }
            };
            n += 1;
            if let Err(e) = res {
                failed = Some(e);
            }
        }
    }
    h.leds.sends.fetch_add(1, Ordering::Relaxed);
    let how = if h.out_ep.is_some() { "interrupt" } else { "set_report" };
    match failed {
        None => {
            h.leds.status.store(STATUS_OK, Ordering::Release);
            klog::event!(Info, "input", "hid.leds", device = h.dev.path(), interface = h.iface, leds = klog::Hex(bits as u64),
                reports = n, how = how);
            Ok(())
        }
        Some(e) => {
            h.leds.status.store(STATUS_FAILED, Ordering::Release);
            klog::event!(Warn, "input", "hid.leds_failed", device = h.dev.path(), interface = h.iface,
                leds = klog::Hex(bits as u64), how = how, error = e.as_str(); "sent again on the next tick");
            Err(String::from(e.as_str()))
        }
    }
}
