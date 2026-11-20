//! Restarts: the explicit one, the rules' (MAC-only as Linux's `rtl_reset_work`, or the whole bring-up), what a
//! failed bring-up leaves behind, and the stop for good.

use super::*;
use crate::machine::Sleepy;
use crate::stage::Stage;

impl<M: Machine> Card<M> {

    /// Linux's recovery from a transmit timeout: `rtl_task` turns ASPM off (M:4899-4904) and runs `rtl_reset_work`.
    /// Bus master, PHY and link are not touched, so the link stays up. Caller holds `restarting` and has set the
    /// state to Down. On any error the card is stopped as after a failed bring-up.
    fn mac_restart(&self, sleepy: bool) -> Result<(), BringUpError> {
        if !self.claim_ocp_within(OCP_WAIT_NS) {
            return Err(BringUpError::Busy);
        }
        let r = if sleepy { self.mac_restart_steps(&Sleepy(&self.hw)) } else { self.mac_restart_steps(&self.hw) };
        if r.is_err() {
            self.stop_hw(false, true);
        }
        self.release_ocp();
        r
    }

    fn mac_restart_steps<W: Hw>(&self, w: &W) -> Result<(), BringUpError> {
        self.still_down()?;
        self.hw.aspm_off();
        let armed = self.irq_mode.load(Ordering::Acquire) == IRQ_ARMED;
        let over = self.start_mac(w)?;
        self.log_passed_over(&over);
        // `rtl_hw_start` ends with `rtl_irq_enable` (M:4136); the cleanup masked it.
        if armed {
            self.irq_on();
        }
        if let Err(s) = self.state.compare_exchange(State::Down as u8, State::Running as u8, Ordering::AcqRel, Ordering::Acquire) {
            return Err(BringUpError::Stopped(State::from(s)));
        }
        let now = self.hw.now_ns();
        self.next_link_look.store(now, Ordering::Relaxed);
        self.next_health.store(now.saturating_add(HEALTH_EVERY_NS), Ordering::Relaxed);
        self.report_irq();
        Ok(())
    }

    pub(super) fn after_failed_bring_up(&self, e: &BringUpError, during: &str) {
        let ifname = self.name.as_str();
        if let BringUpError::Stopped(_) = e {
            klog::event!(Warn, "net", "rtl8126.restart_aborted", ifname = ifname, error = e.text());
            return;
        }
        if e.fatal() {
            if self.state.compare_exchange(State::Down as u8, State::Stopped as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                klog::event!(Error, "net", "rtl8126.refused", ifname = ifname, during = during, stage = e.stage().name(), error = e.text());
            }
            return;
        }
        if self.state.compare_exchange(State::Down as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            klog::event!(Warn, "net", "rtl8126.restart_aborted", ifname = ifname, error = e.text());
            return;
        }
        let n = self.retries.fetch_add(1, Ordering::AcqRel);
        let delay = if n < RETRY_SOON_TIMES { RETRY_SOON_NS } else { RETRY_LATER_NS };
        self.next_restart.store(self.hw.now_ns().saturating_add(delay), Ordering::Relaxed);
        klog::event!(Error, "net", "rtl8126.bring_up_failed", ifname = ifname, during = during, stage = e.stage().name(),
            error = e.text(), retry_in_ms = delay / 1_000_000);
    }

    pub(super) fn log_quiesce_note(&self) {
        let v = self.quiesce_note.swap(0, Ordering::AcqRel);
        if v & 1 << 31 != 0 {
            let (was, failed) = (State::from((v & 0xff) as u8), (v >> 8) & 0xffff);
            if failed > 0 {
                klog::event!(Warn, "net", "rtl8126.quiesce", ifname = self.name.as_str(), was = was.as_str(), failed_steps = failed);
            } else {
                klog::event!(Info, "net", "rtl8126.quiesce", ifname = self.name.as_str(), was = was.as_str());
            }
        }
    }

    /// Stops and starts the card again from any state (ktests, the desk), from a thread: its waits sleep. Waits a
    /// bounded time for a restart that is already running.
    pub fn restart(&self, reason: &str) -> Result<(), BringUpError> {
        let deadline = self.hw.now_ns().saturating_add(RESTART_WAIT_NS);
        while self.restarting.swap(true, Ordering::AcqRel) {
            if self.hw.now_ns() >= deadline {
                klog::event!(Warn, "net", "rtl8126.restart_busy", ifname = self.name.as_str(), reason = reason);
                return Err(BringUpError::Stopped(self.state()));
            }
            self.hw.pause_us(1000);
        }
        *self.due.lock() = None;
        self.rx_retried.store(false, Ordering::Relaxed);
        self.tx_timeouts.store(0, Ordering::Relaxed);
        self.state.store(State::Down as u8, Ordering::Release);
        let r = self.restart_claimed(reason, true, Depth::Full);
        self.restarting.store(false, Ordering::Release);
        r
    }

    /// A restart rule fired (or a retry is due): queued for the worker thread if there is one, else run here.
    pub(super) fn want_restart(&self, from: State, reason: &'static str, depth: Depth) {
        if !self.worker.load(Ordering::Acquire) {
            self.auto_restart(from, reason, depth, false);
            return;
        }
        let mut due = self.due.lock();
        if due.is_none() {
            klog::event!(Info, "net", "rtl8126.restart_queued", ifname = self.name.as_str(), reason = reason);
        }
        // A whole restart that is already queued is not made smaller.
        let depth = if matches!(*due, Some((_, _, Depth::Full))) { Depth::Full } else { depth };
        *due = Some((from, reason, depth));
    }

    /// A rule's restart: only if the card is still in `from` (a Quiesce in between cancels it).
    pub(super) fn auto_restart(&self, from: State, reason: &str, depth: Depth, sleepy: bool) {
        if self.restarting.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.state.compare_exchange(from as u8, State::Down as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = self.restart_claimed(reason, sleepy, depth);
        }
        self.restarting.store(false, Ordering::Release);
    }

    /// Caller holds `restarting` and has set the state to Down. A MAC-only restart that does not come through is
    /// followed by the whole one at once.
    fn restart_claimed(&self, reason: &str, sleepy: bool, depth: Depth) -> Result<(), BringUpError> {
        self.log_quiesce_note();
        RESTARTS.inc();
        let (ifname, attempt) = (self.name.as_str(), self.retries.load(Ordering::Relaxed) + 1);
        if depth == Depth::Mac {
            klog::event!(Warn, "net", "rtl8126.restart", ifname = ifname, reason = reason, scope = "mac", attempt = attempt);
            match self.mac_restart(sleepy) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    klog::event!(Warn, "net", "rtl8126.mac_restart_failed", ifname = ifname, error = e.text());
                    if let BringUpError::Stopped(_) = e {
                        return Err(e);
                    }
                }
            }
        }
        if self.link_up.swap(false, Ordering::AcqRel) {
            klog::event!(Warn, "net", "link.down", ifname = ifname, reason = "restart");
        }
        klog::event!(Warn, "net", "rtl8126.restart", ifname = ifname, reason = reason, scope = "full", attempt = attempt);
        let r = self.bring_up(sleepy);
        if let Err(e) = &r {
            self.after_failed_bring_up(e, "restart");
        }
        r
    }

    /// The self-check found the card unusable: stopped for good, with its line.
    pub(super) fn halt(&self, stage: Stage, line: String) {
        if self.state.compare_exchange(State::Running as u8, State::Stopped as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.link_up.store(false, Ordering::Release);
            let failed = self.stop_hw(false, false);
            self.check.store(CHECK_DONE, Ordering::Release);
            self.say(stage, Verdict::Failed, line);
            klog::event!(Error, "net", "rtl8126.halted", ifname = self.name.as_str(), stage = stage.name(), failed_stop_steps = failed,
                bus_master = self.hw.bus_master());
            self.report_irq();
        }
    }
}
