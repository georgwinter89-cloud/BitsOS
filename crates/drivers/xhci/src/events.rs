//! Event handling: the event ring drained in bounded batches (one ring's worth), interrupt state acknowledged
//! before handling, ERDP advanced inside long batches and written with EHB at the end (V1 `xhcievent.asm`).
//! Command completions go to the waiting command, port changes are noted for the hotplug worker and wake it
//! (ports the controller does not have are dropped), transfer events go to their slot (`xfer.rs`); transfer
//! results and pipe reports are queued in event order and handed on after the event ring lock is dropped.

use crate::hc::{CmdCompletion, Controller, Env, Source, State, ERDP_EVERY};
use crate::regs::{self, iman, sts};
use crate::trb::{cc, ty, Trb};
use alloc::format;
use core::sync::atomic::Ordering;

klog::counter!(EVENTS, "usb.xhci.events");
klog::counter!(PORT_EVENTS, "usb.xhci.port_events");
klog::counter!(BAD_EVENTS, "usb.xhci.bad_events");
klog::counter!(RING_FULL, "usb.xhci.event_ring_full");

impl<E: Env> Controller<E> {

    /// True if a running controller has an event waiting (the poller asks, then wakes the event thread).
    pub fn event_pending(&self) -> bool {
        if self.state() != State::Running {
            return false;
        }
        self.ev.try_lock().is_some_and(|g| g.as_ref().is_some_and(|e| e.pending().unwrap_or(false)))
    }

    /// What the MSI-X handler will do (PC-3): note the interrupt's time (the first one not yet answered by a
    /// drain) and wake the event thread. Polled today: the poller calls it when it first sees an event pending,
    /// from the idle loop, so it must not allocate or wait.
    pub fn interrupt(&self) {
        if self.irq_at.load(Ordering::Relaxed) == 0 {
            let _ = self.irq_at.compare_exchange(0, self.now().max(1), Ordering::AcqRel, Ordering::Relaxed);
        }
        self.ev_wq.wake_one();
    }

    /// The input stamp (IN-6, B4) of what the current drain finds: the time `interrupt` noticed the event, or
    /// the drain's start if nothing noticed it first (the thread's tick, a waiter). Never goes back.
    pub(crate) fn irq_stamp(&self) -> u64 {
        match self.drain_at.load(Ordering::Acquire) {
            0 => self.now(),
            t => t,
        }
    }

    /// Handles every event the controller has written, up to one ring's worth (B4: bounded). Returns how many.
    /// Interrupt state is cleared **before** handling (V1 `xhcimsi.asm:58-72`), so an event arriving meanwhile
    /// raises the next interrupt. Skips the round if another context is draining.
    pub fn process_events(&self, src: Source) -> usize {
        let n = self.drain(src);
        // Also when nothing was drained: the event thread is woken for queued pipe events alone.
        self.hand_on(src);
        n
    }

    fn drain(&self, src: Source) -> usize {
        if self.state() != State::Running {
            return 0;
        }
        let Some(mut g) = self.ev.try_lock() else { return 0 };
        let Some(er) = g.as_mut() else { return 0 };
        // Under the event ring lock: one drain at a time sets the stamp, so `fetch_max` keeps it monotonic even
        // if `interrupt` noted a time just before an earlier drain took its own.
        let noticed = self.irq_at.swap(0, Ordering::AcqRel);
        let now = self.now();
        self.drain_at.fetch_max(if noticed != 0 && noticed <= now { noticed } else { now }, Ordering::AcqRel);
        let st = self.usbsts();
        if st != u32::MAX && st & (sts::EINT | sts::PCD) != 0 {
            self.wr(self.caps.op(regs::USBSTS), st & (sts::EINT | sts::PCD));
        }
        let im = self.rd(self.caps.ir(0, regs::IMAN));
        if im != u32::MAX && im & iman::IP != 0 {
            self.wr(self.caps.ir(0, regs::IMAN), (im & iman::IE) | iman::IP);
        }
        let mut n = 0;
        let budget = er.capacity();
        let mut error = None;
        while n < budget {
            match er.pop() {
                Ok(Some(t)) => {
                    n += 1;
                    self.handle(&t);
                    if n % ERDP_EVERY == 0 {
                        self.wr64(self.caps.ir(0, regs::ERDP), er.dequeue_ptr() | (er.dequeue_segment() as u64 & 7));
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        if n > 0 {
            self.wr64(self.caps.ir(0, regs::ERDP), er.dequeue_ptr() | (er.dequeue_segment() as u64 & 7) | regs::ERDP_EHB);
            EVENTS.add(n as u64);
            self.stats.events.fetch_add(n as u64, Ordering::Relaxed);
            match src {
                Source::Thread => self.stats.by_thread.fetch_add(n as u64, Ordering::Relaxed),
                Source::Waiter => self.stats.by_waiter.fetch_add(n as u64, Ordering::Relaxed),
            };
        }
        drop(g);
        if let Some(e) = error {
            self.fail(&format!("event ring memory: {e:?}"));
        }
        n
    }

    fn handle(&self, t: &Trb) {
        match t.trb_type() {
            ty::COMMAND_COMPLETION => self.command_event(t),
            ty::PORT_STATUS_CHANGE => {
                let p = t.port();
                if p == 0 || p > self.caps.max_ports {
                    // A port the controller does not have (corrupt event): its PORTSC would be reserved space.
                    BAD_EVENTS.inc();
                    self.stats.bad_port_events.fetch_add(1, Ordering::Relaxed);
                    klog::event!(Warn, "usb", "xhci.bad_port_event", ctrl = self.name.as_str(), port = p);
                    return;
                }
                PORT_EVENTS.inc();
                self.stats.port_events.fetch_add(1, Ordering::Relaxed);
                let mut pc = self.port_changes.lock();
                if !pc.contains(&p) {
                    pc.push(p);
                }
                drop(pc);
                klog::event!(Debug, "usb", "xhci.port_change", ctrl = self.name.as_str(), port = p,
                    portsc = klog::Hex(self.rd(self.caps.portsc(p)) as u64));
                self.kick();
            }
            ty::TRANSFER_EVENT => {
                self.stats.transfer_events.fetch_add(1, Ordering::Relaxed);
                self.transfer_event(t);
            }
            ty::HOST_CONTROLLER => {
                if t.code() == cc::EVENT_RING_FULL {
                    RING_FULL.inc();
                    self.stats.ring_full.fetch_add(1, Ordering::Relaxed);
                    klog::event!(Warn, "usb", "xhci.event_ring_full", ctrl = self.name.as_str());
                } else {
                    BAD_EVENTS.inc();
                    klog::event!(Error, "usb", "xhci.host_controller_event", ctrl = self.name.as_str(), code = t.code());
                }
            }
            other => {
                self.stats.other_events.fetch_add(1, Ordering::Relaxed);
                klog::event!(Debug, "usb", "xhci.event_other", ctrl = self.name.as_str(), trb_type = other, code = t.code());
            }
        }
    }

    fn command_event(&self, t: &Trb) {
        let addr = t.param & !0xF;
        let code = t.code();
        let mut g = self.cmd.lock();
        let Some(c) = g.as_mut() else { return };
        // Command Aborted only follows our own abort, after which the ring starts over at its base. While the
        // abort runs it answers the pending command (which fails as Timeout); later it would name the address the
        // next command reuses, so then it never completes a waiting command (stray).
        if code == cc::COMMAND_ABORTED && c.aborting && c.pending == Some(addr) {
            c.ring.consumed(1);
            c.pending = None;
            drop(g);
            self.stats.aborted_completions.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let ours = code != cc::COMMAND_RING_STOPPED && code != cc::COMMAND_ABORTED;
        if ours && c.pending == Some(addr) {
            c.ring.consumed(1);
            c.pending = None;
            c.done = Some(CmdCompletion { code, slot: t.slot(), param: t.status & 0x00FF_FFFF });
            self.stats.cmd_completions.fetch_add(1, Ordering::Relaxed);
            drop(g);
            self.cmd_wq.wake_all();
        } else if code != cc::COMMAND_RING_STOPPED {
            // A completion for a command nobody waits for any more (it completed after its abort).
            drop(g);
            self.stats.stray_completions.fetch_add(1, Ordering::Relaxed);
            klog::event!(Debug, "usb", "xhci.cmd_stray", ctrl = self.name.as_str(), trb = klog::Hex(addr), code = code);
        }
    }

    /// One round of the event thread: drain, health watch, log what Quiesce left behind.
    pub fn thread_round(&self) {
        self.log_quiesce_note();
        self.process_events(Source::Thread);
        self.check_health();
    }
}
