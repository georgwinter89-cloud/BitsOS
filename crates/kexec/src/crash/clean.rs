//! RB-13: before the panic path resets the machine it leaves it as the MCP `reboot` does, as far as panic context
//! allows. On 27.10. (generation 30, `panic=reboot panic_delay=90`) a panic came back in Windows: the FCH watchdog
//! (armed with 30 s, no kicks once the CPUs stop) reset the machine 60 s before the panic's own reset was due, with
//! every xHCI controller still running (the stick busy), and the firmware did not find the stick. The MCP
//! `reboot` parks the CPUs, quiesces every driver (xHCI halted, the watchdog on its grace count) and resets
//! through the FADT register (`cf9_6` in the FCH's reset status).
//!
//! Panic context: the other CPUs are stopped by NMI and may hold locks, the heap is in emergency mode. So no
//! lock is waited for, nothing allocates, every wait is a bounded spin, and each step runs once (a fault inside
//! one re-enters `report`, which goes on to `finish` and the reset without it):
//! 1. `watchdog`, right after the NMI stop: the FCH watchdog counts from the panic delay plus a margin, so the
//!    panic's own reset comes first and the watchdog stays the safety net (fch-wdt's hook);
//! 2. `devices`, after this CPU's backtrace (so its lines sit early in the record, which a 16-CPU report cuts):
//!    every xHCI controller halted, reset (HCRST, CNR clear, bounded) and its bus master off
//!    (`xhci::panic_stop_all`, Linux `xhci_shutdown`), one record line each, and the reset order. The list is
//!    xhci's lock-free panic list (an idle CPU stopped inside the poller holds the driver's list lock for good),
//!    the waits run on `super::clock` (raw TSC at the boot rate, not the clock's sequence lock);
//! 3. `step`: each reset path logged and flushed before it is tried, so the mirror shows the one that reset.
//!
//! Only for `Action::Reboot`: a halt keeps the machine (and its devices) as the panic found it.

use super::text::Buf;
use super::Page;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::OnceRef;

/// What the watchdog hook did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watchdog {
    /// No hook, or no watchdog chip.
    Absent,
    /// The chip is there but not armed: nothing to do.
    NotArmed,
    /// The chip counts from `secs` now, without kicks.
    Grace { secs: u32 },
}

/// fch-wdt's panic hook: arm the chip with this many seconds from now, without kicks. No lock, heap or log.
pub type WatchdogHook = fn(u32) -> Watchdog;

static WATCHDOG_HOOK: OnceRef<WatchdogHook> = OnceRef::new();

/// Installs the watchdog's panic hook (the first call wins).
pub fn set_watchdog_hook(f: &'static WatchdogHook) -> bool {
    WATCHDOG_HOOK.set(f)
}

/// The sound controllers' panic hook (SN-12 change request, INT-W23): `now` the panic clock in ns, `line` one line
/// per controller; returns how many. Only core types, because kexec cannot depend on hda (hda -> amdgpu -> kexec).
/// The installed hook may be terminal (hda keeps its verb locks), so it runs only on the real panic path.
pub type SoundHook = fn(now: &(dyn Fn() -> u64 + Sync), line: &mut dyn FnMut(core::fmt::Arguments<'_>)) -> usize;

static SOUND_HOOK: OnceRef<SoundHook> = OnceRef::new();

/// Installs the sound controllers' panic hook (the first call wins).
pub fn set_sound_hook(f: &'static SoundHook) -> bool {
    SOUND_HOOK.set(f)
}

/// How long the watchdog keeps counting past the panic's own reset (it fires only when that reset failed).
pub const WATCHDOG_MARGIN_S: u64 = 30;
/// The chip's count register is 16 bits of seconds.
const WATCHDOG_MAX_S: u64 = 0xFFFF;

/// The watchdog's count for a reset `delay_s` from now (pure; host-tested).
pub fn watchdog_secs(delay_s: u64) -> u32 {
    delay_s.saturating_add(WATCHDOG_MARGIN_S).min(WATCHDOG_MAX_S) as u32
}

static WATCHDOG_DONE: AtomicBool = AtomicBool::new(false);
static DEVICES_DONE: AtomicBool = AtomicBool::new(false);

/// Step 1: the watchdog on the panic's delay plus the margin.
pub(super) fn watchdog(delay_s: u64, page: Option<&mut Page>) {
    if WATCHDOG_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let secs = watchdog_secs(delay_s);
    let w = WATCHDOG_HOOK.get().map_or(Watchdog::Absent, |f| f(secs));
    let mut l = Buf::<120>::new();
    match w {
        Watchdog::Grace { secs } => {
            klog::try_event!(Error, "crash", "watchdog.grace", count_s = secs, reset_in_s = delay_s;
                "the FCH watchdog counts from {secs} s without kicks: the panic's own reset comes first");
            let _ = write!(l, "watchdog: counts from {secs} s, the panic resets after {delay_s} s");
        }
        Watchdog::NotArmed => {
            klog::try_event!(Error, "crash", "watchdog.not_armed");
            let _ = write!(l, "watchdog: not armed");
        }
        Watchdog::Absent => {
            let _ = write!(l, "watchdog: none");
        }
    }
    if let Some(p) = page {
        p.line(l.as_str());
    }
    klog::flush_emergency();
}

/// Step 2: every xHCI controller halted and reset, bus master off; the reset order.
pub(super) fn devices(mut page: Option<&mut Page>) {
    if DEVICES_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let n = xhci::panic_stop_all(&super::clock, &mut |name, r| {
        let us = r.ns / 1000;
        klog::try_event!(Error, "crash", "xhci.stopped", ctrl = name, halted = r.halted, reset = r.reset, ready = r.ready,
            gone = r.gone, us = us);
        let mut l = Buf::<120>::new();
        let _ = write!(l, "{name}: halted={} reset={} ready={} gone={} us={us}", r.halted, r.reset, r.ready, r.gone);
        if let Some(p) = page.as_deref_mut() {
            p.line(l.as_str());
        }
    });
    if n == 0 {
        if let Some(p) = page.as_deref_mut() {
            p.line("xhci: no controller");
        }
    }
    // SN-12: the sound controllers' streams stopped and codecs shut down (HDMI audio flooded a reset otherwise).
    let n = SOUND_HOOK.get().map_or(0, |f| {
        f(&super::clock, &mut |args| {
            let mut l = Buf::<160>::new();
            let _ = l.write_fmt(args);
            klog::try_event!(Error, "crash", "sound.stopped"; "{}", l.as_str());
            if let Some(p) = page.as_deref_mut() {
                p.line(l.as_str());
            }
        })
    });
    if n == 0 {
        if let Some(p) = page.as_deref_mut() {
            p.line("sound: no controller");
        }
    }
    let mut order = Buf::<160>::new();
    let _ = write!(order, "reset: ");
    super::describe_reset(&mut order);
    klog::try_event!(Error, "crash", "reset.order"; "{}", order.as_str());
    if let Some(p) = page {
        p.line(order.as_str());
    }
    klog::flush_emergency();
}

/// Step 3: one reset path, logged and flushed before it is tried.
pub(super) fn step(n: usize, method: &str) {
    klog::try_event!(Error, "crash", "reset.step", n = n, method = method);
    klog::flush_emergency();
}
