//! The kernel's instance of the stack and its poll thread (class KernelRt): it sleeps until smoltcp's next
//! deadline (a `time::HrTimer`), a kick from a socket call, or the next look at the cards, whichever is first.

use super::{Hold, Poller, Stack, BUSY_PAUSE_NS, IRQ_SAFETY_NS, RX_POLL_ACTIVE_NS, RX_POLL_NS};
use core::sync::atomic::Ordering;

static KERNEL: ksync::Once<Stack> = ksync::Once::new();
static STARTED: ksync::Once<Result<(), &'static str>> = ksync::Once::new();

/// The kernel's stack, created and its poll thread started on first use. `Err` when the thread could not start.
pub fn start() -> Result<&'static Stack, &'static str> {
    let s: &'static Stack = KERNEL.get_or_init(|| Stack::new(time::now_ns, seed(), true));
    s.me.set(s);
    let r = *STARTED.get_or_init(|| {
        match sched::spawn("net-poll", sched::Class::KernelRt, move || run(s)) {
            Ok(_) => {
                klog::event!(Info, "net", "stack.start", cards = crate::netdev::devices().len(), poll_us = RX_POLL_NS / 1000,
                    ephemeral_start = s.first_ephemeral_port());
                Ok(())
            }
            Err(_) => Err("no thread for the network poll"),
        }
    });
    r.map(|_| s)
}

/// The kernel's stack if `start` ran (does not start it).
pub fn kernel() -> Option<&'static Stack> {
    STARTED.get().and_then(|r| r.ok()).and(KERNEL.get())
}

/// For the drivers' raw-frame ktests: keeps the kernel's stack (if it runs) away from the cards while the
/// guard lives, so the test sees every frame the card receives.
pub fn hold_cards() -> Option<Hold> {
    kernel().map(|s| s.hold())
}

/// The stack's seed: TCP initial sequence numbers and DHCP transaction ids (smoltcp, mixed per card with its
/// MAC and bring-up time, `Iface::new`) and the start of the ephemeral ports. The clock restarts at 0 with
/// every kernel (a Reload too), so it alone would give every boot the same seed; the TSC runs on across a
/// Reload and its low bits differ from boot to boot (firmware timing). That keeps a reloaded kernel off the old
/// 4-tuples and sequence numbers, but it is NOT unpredictable: an attacker who can estimate the boot time can
/// guess initial sequence numbers (off-path injection, RFC 6528). RDRAND belongs here as soon as hal has a safe
/// wrapper (change request to CP: `hal::cpu::rdrand64`); this function is the only place to change.
pub fn seed() -> u64 {
    super::mix64(hal::cpu::rdtsc() ^ time::now_ns().rotate_left(32) ^ 0x6274_736f_7332_6e33)
}

fn timer_fired(_: usize) {
    if let Some(s) = KERNEL.get() {
        s.wake_poll();
    }
}

/// The poll thread. While some card is polled (no MSI, `net_irq=off`, a stormed line), it looks at the cards every
/// 50 µs while traffic flows and every 500 µs when quiet. Once every card is interrupt-driven (NW-4: their
/// hard-IRQ hooks call `netdev::irq_wake`), it sleeps until a wake, smoltcp's next deadline or the safety look
/// (`IRQ_SAFETY_NS`, catches a lost interrupt as Linux igc's watchdog does with EICS). smoltcp's deadlines
/// (retransmits, delayed ACKs, DHCP) come from the `HrTimer`, armed only when such a deadline falls before the
/// next look. The mode is logged when it changes (`stack.poll_mode`).
fn run(s: &'static Stack) {
    let timer = time::HrTimer::new(timer_fired, 0);
    let mut p = Poller::new(s);
    let mut irq_mode = false;
    loop {
        let r = p.round();
        if r.busy {
            sched::sleep_ns(BUSY_PAUSE_NS);
            continue;
        }
        let irq = crate::netdev::all_irq_driven();
        if irq != irq_mode {
            irq_mode = irq;
            klog::event!(Info, "net", "stack.poll_mode", mode = if irq { "irq" } else { "polled" },
                look_us = if irq { IRQ_SAFETY_NS } else { RX_POLL_NS } / 1000);
        }
        let now = s.now();
        let every = if irq {
            IRQ_SAFETY_NS
        } else if s.active(now) {
            RX_POLL_ACTIVE_NS
        } else {
            RX_POLL_NS
        };
        let look = now.saturating_add(every);
        let wake = match r.next_ns {
            Some(n) if n < look => {
                timer.arm(n);
                n
            }
            _ => {
                if timer.is_armed() {
                    timer.cancel();
                }
                look
            }
        };
        // Bounded (B4): the timer, a kick or an interrupt ends it earlier.
        s.poll_wq.wait_until_timeout(|| s.kicked.swap(false, Ordering::AcqRel), wake.saturating_sub(now).max(1));
    }
}
