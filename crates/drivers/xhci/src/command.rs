//! The command ring in use: one command at a time per controller, each waited for at most `CMD_NS`; a command
//! without an answer aborts the ring (CRCR.CA), which is then set up afresh so only that command fails (V1
//! `xhcicmd.asm`). A controller that halted, reports HSE or does not stop its ring is parked.

use crate::hc::{CmdCompletion, CmdError, Controller, Env, Source, State, ABORT_NS, CMD_GATE_NS, CMD_NS, FALLBACK_NS, MS};
use crate::regs::{self, crcr, sts};
use crate::trb::{cc, Trb};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{fence, Ordering};

klog::counter!(COMMANDS, "usb.xhci.commands");
klog::counter!(COMMAND_TIMEOUTS, "usb.xhci.command_timeouts");

impl<E: Env> Controller<E> {

    /// Queues a command and waits for its completion (at most `CMD_NS`, then the ring is aborted). One command
    /// at a time per controller; a caller waits at most `CMD_GATE_NS` for its turn.
    pub fn command(&self, trb: Trb) -> Result<CmdCompletion, CmdError> {
        if self.state() != State::Running {
            return Err(CmdError::NotRunning);
        }
        let deadline = self.now().saturating_add(CMD_GATE_NS);
        while self.cmd_busy.swap(true, Ordering::AcqRel) {
            let now = self.now();
            if now >= deadline {
                klog::event!(Warn, "usb", "xhci.cmd_busy", ctrl = self.name.as_str(), trb_type = trb.trb_type());
                return Err(CmdError::Busy);
            }
            self.env.wait(&self.cmd_wq, &mut || !self.cmd_busy.load(Ordering::Acquire), deadline - now);
        }
        let r = self.command_owned(trb);
        self.cmd_busy.store(false, Ordering::Release);
        self.cmd_wq.wake_all();
        r
    }

    fn take_done(&self) -> Option<CmdCompletion> {
        self.cmd.lock().as_mut().and_then(|c| c.done.take())
    }

    fn command_owned(&self, trb: Trb) -> Result<CmdCompletion, CmdError> {
        {
            let mut g = self.cmd.lock();
            let c = g.as_mut().ok_or(CmdError::NotRunning)?;
            c.done = None;
            let addr = c.ring.push(trb).map_err(CmdError::Ring)?;
            c.pending = Some(addr);
        }
        fence(Ordering::SeqCst);
        self.wr(self.caps.doorbell(0), 0);
        COMMANDS.inc();
        self.stats.commands.fetch_add(1, Ordering::Relaxed);
        let start = self.now();
        let deadline = start.saturating_add(CMD_NS);
        loop {
            if let Some(d) = self.take_done() {
                return result(d);
            }
            if self.state() != State::Running {
                self.clear_pending();
                return Err(CmdError::ControllerFailed);
            }
            let now = self.now();
            if now >= deadline {
                break;
            }
            self.wait_for_done(start, deadline - now);
        }
        // Once more: the event may have come with the deadline (V1 `xhci_cmd`).
        self.process_events(Source::Waiter);
        if let Some(d) = self.take_done() {
            return result(d);
        }
        self.abort(trb.trb_type())
    }

    fn clear_pending(&self) {
        if let Some(c) = self.cmd.lock().as_mut() {
            c.pending = None;
        }
    }

    /// Sleeps until the pending command is done (the event thread delivers it) or `timeout_ns`. Drains the ring
    /// itself when no event thread runs or it has not answered for `FALLBACK_NS`.
    fn wait_for_done(&self, start: u64, timeout_ns: u64) {
        let mut cond = || {
            let thread = self.thread_alive.load(Ordering::Acquire);
            if !thread || self.now().saturating_sub(start) >= FALLBACK_NS {
                self.process_events(Source::Waiter);
            }
            self.state() != State::Running || self.cmd.lock().as_ref().is_some_and(|c| c.done.is_some())
        };
        self.env.wait(&self.cmd_wq, &mut cond, timeout_ns.min(FALLBACK_NS));
    }

    /// The command got no answer: abort the command ring (CRCR.CA, xHCI §4.6.1.2), wait for CRR clear, check
    /// the controller still runs, and set the ring up afresh. Only this command fails; a controller that halted,
    /// reports HSE or does not stop its ring is parked (V1 `xhci_abort`/`xhci_dead`).
    fn abort(&self, trb_type: u8) -> Result<CmdCompletion, CmdError> {
        COMMAND_TIMEOUTS.inc();
        self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
        klog::event!(Warn, "usb", "xhci.cmd_timeout", ctrl = self.name.as_str(), trb_type = trb_type, waited_ms = CMD_NS / MS;
            "aborting the command ring");
        if let Some(cs) = self.cmd.lock().as_mut() {
            cs.aborting = true;
        }
        let c = self.caps.op(regs::CRCR);
        self.wr(c, crcr::CA);
        self.wr(c + 4, 0);
        let deadline = self.now().saturating_add(ABORT_NS);
        let stopped = loop {
            self.process_events(Source::Waiter);
            let v = self.rd(c);
            if v != u32::MAX && v & crcr::CRR == 0 {
                break true;
            }
            if v == u32::MAX || self.now() >= deadline {
                break false;
            }
            self.env.relax(true);
        };
        let s = self.usbsts();
        if !stopped || s == u32::MAX || s & (sts::HCH | sts::HSE) != 0 {
            self.fail(if !stopped { "command ring did not stop after abort" } else { "halted or HSE after a command abort" });
            self.clear_pending();
            if let Some(cs) = self.cmd.lock().as_mut() {
                cs.aborting = false;
            }
            return Err(CmdError::ControllerFailed);
        }
        // The Command Aborted / Ring Stopped events may still be on the way; take what is there.
        self.process_events(Source::Waiter);
        let late = self.take_done();
        {
            let mut g = self.cmd.lock();
            let Some(cs) = g.as_mut() else { return Err(CmdError::ControllerFailed) };
            if cs.ring.init().is_err() {
                drop(g);
                self.fail("command ring memory after abort");
                return Err(CmdError::ControllerFailed);
            }
            cs.pending = None;
            cs.done = None;
            cs.aborting = false;
            self.wr64(c, cs.ring.base() | crcr::RCS as u64);
        }
        klog::event!(Info, "usb", "xhci.cmd_ring_reset", ctrl = self.name.as_str());
        match late {
            Some(d) if d.code != cc::COMMAND_ABORTED && d.code != cc::COMMAND_RING_STOPPED => result(d),
            _ => Err(CmdError::Timeout),
        }
    }

    /// Bring-up plus the boot health check: a No-Op proves the controller reads its command ring and writes its
    /// event ring, i.e. its DMA reaches memory (on the Mini-PC an HSE here means a bridge without bus master). A
    /// controller failing either is parked (RS off, bus master off) with the reason kept.
    pub fn start_checked(&self) -> Result<(), String> {
        self.start()?;
        match self.noop() {
            Ok(_) => {
                klog::event!(Info, "usb", "xhci.noop_ok", ctrl = self.name.as_str());
                Ok(())
            }
            Err(e) => {
                let why = format!("boot No-Op failed: {e:?}");
                klog::event!(Error, "usb", "xhci.noop_failed", ctrl = self.name.as_str(), error = format!("{e:?}"));
                // No-op when the command path already parked it (its reason stays) or a Quiesce came first.
                self.fail(&why);
                Err(why)
            }
        }
    }

    /// A No-Op command: proves the controller reads its command ring and writes its event ring (DMA works).
    pub fn noop(&self) -> Result<CmdCompletion, CmdError> {
        self.command(Trb::noop_command())
    }
}

fn result(d: CmdCompletion) -> Result<CmdCompletion, CmdError> {
    if d.code == cc::SUCCESS { Ok(d) } else { Err(CmdError::Failed { code: d.code, slot: d.slot }) }
}
