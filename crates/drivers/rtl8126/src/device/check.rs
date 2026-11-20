//! The stages after the start: link, first frame, address, router, interrupt. Each has a deadline and says one line.

use super::*;
use crate::machine::IrqSetup;
use crate::stage::text;

impl<M: Machine> Card<M> {

    pub(super) fn begin_check(&self, stage: u8, now: u64) {
        self.check_since.store(now, Ordering::Relaxed);
        self.check_said.store(false, Ordering::Relaxed);
        self.ping_tries.store(0, Ordering::Relaxed);
        self.check.store(stage, Ordering::Release);
    }

    /// One step of the stages after the start: link, first frame, address, router, interrupt. Called a few times a
    /// second from a thread that may sleep (the ping waits up to a second); every stage has a deadline and says one
    /// line. Only "no frame at all" stops the card; a missing cable, a late router or a silent interrupt leave it
    /// running, because each of them can still come.
    pub fn advance(&self, up: &dyn Uplink) {
        if self.state() != State::Running {
            return;
        }
        let now = self.hw.now_ns();
        let since = now.saturating_sub(self.check_since.load(Ordering::Relaxed));
        let rx = self.stats.rx_frames.load(Ordering::Relaxed);
        let link = self.link_up.load(Ordering::Acquire);
        let late = |limit: u64| since >= limit && !self.check_said.swap(true, Ordering::AcqRel);
        match self.check.load(Ordering::Acquire) {
            s if s == Stage::Link as u8 => {
                if link {
                    self.say(Stage::Link, Verdict::Ok, text::link_ok(self.speed_mbps.load(Ordering::Relaxed)));
                    self.begin_check(Stage::Rings as u8, now);
                } else if late(LINK_WAIT_NS) {
                    self.say(Stage::Link, Verdict::Failed, text::link_none(LINK_WAIT_NS / 1_000_000_000));
                }
            }
            s if s == Stage::Rings as u8 => {
                if !link {
                    self.begin_check(Stage::Link as u8, now);
                } else if rx > self.rx_at_start.load(Ordering::Relaxed) {
                    self.rx_retried.store(false, Ordering::Relaxed);
                    self.say(Stage::Rings, Verdict::Ok, text::rings_ok());
                    self.begin_check(Stage::Lease as u8, now);
                } else if since >= RX_WAIT_NS {
                    self.hw.dma_check();
                    let (sent, done) = (self.stats.tx_frames.load(Ordering::Relaxed), self.stats.tx_done.load(Ordering::Relaxed));
                    if self.rx_retried.swap(true, Ordering::AcqRel) {
                        self.halt(Stage::Rings, text::rings_stopped(RX_WAIT_NS / 1_000_000_000, sent, done));
                    } else {
                        self.say(Stage::Rings, Verdict::Note, text::rings_retry(RX_WAIT_NS / 1_000_000_000));
                        self.check.store(CHECK_DONE, Ordering::Release);
                        self.want_restart(State::Running, "no frame received", Depth::Full);
                    }
                }
            }
            s if s == Stage::Lease as u8 => {
                if let Some((addr, _)) = up.lease(&self.name) {
                    self.say(Stage::Lease, Verdict::Ok, text::lease_ok(addr));
                    self.begin_check(Stage::Gateway as u8, now);
                } else if late(LEASE_WAIT_NS) {
                    let (sent, done) = (self.stats.tx_frames.load(Ordering::Relaxed), self.stats.tx_done.load(Ordering::Relaxed));
                    self.say(Stage::Lease, Verdict::Failed, text::lease_none(LEASE_WAIT_NS / 1_000_000_000, rx, sent, done));
                }
            }
            s if s == Stage::Gateway as u8 => match up.lease(&self.name) {
                None => self.begin_check(Stage::Lease as u8, now),
                Some((_, None)) => {
                    self.say(Stage::Gateway, Verdict::Note, text::gateway_unknown());
                    self.begin_check(Stage::Irq as u8, now);
                }
                Some((_, Some(router))) => match up.ping(router, PING_NS) {
                    Some(ns) => {
                        self.say(Stage::Gateway, Verdict::Ok, text::gateway_ok(ns / 1000));
                        self.begin_check(Stage::Irq as u8, now);
                    }
                    None if self.ping_tries.fetch_add(1, Ordering::AcqRel) + 1 >= PING_TRIES => {
                        self.say(Stage::Gateway, Verdict::Failed, text::gateway_none(PING_TRIES));
                        self.begin_check(Stage::Irq as u8, now);
                    }
                    None => {}
                },
            },
            s if s == Stage::Irq as u8 => self.check_irq_arm(now, rx),
            CHECK_IRQ_PROOF => self.check_irq_proof(up, since, rx),
            _ => {}
        }
    }

    fn check_irq_arm(&self, now: u64, rx: u64) {
        if self.irq_mode.load(Ordering::Acquire) == IRQ_ARMED {
            // Armed by the bring-up because it worked before.
            self.say(Stage::Irq, Verdict::Ok, text::irq_ok("wie zuvor"));
            self.check.store(CHECK_DONE, Ordering::Release);
        } else if self.irq_given_up.load(Ordering::Acquire) {
            self.say(Stage::Irq, Verdict::Note, text::irq_none());
            self.check.store(CHECK_DONE, Ordering::Release);
        } else {
            match self.hw.setup_irq(self.hook()) {
                IrqSetup::Polled => {
                    self.irq_given_up.store(true, Ordering::Release);
                    self.say(Stage::Irq, Verdict::Note, text::irq_none());
                    self.check.store(CHECK_DONE, Ordering::Release);
                }
                kind => {
                    self.irq_count_at_arm.store(self.irq_count.load(Ordering::Relaxed), Ordering::Relaxed);
                    self.rx_at_arm.store(rx, Ordering::Relaxed);
                    klog::event!(Info, "net", "rtl8126.irq_armed", ifname = self.name.as_str(), kind = kind.name());
                    self.irq_on();
                    self.begin_check(CHECK_IRQ_PROOF, now);
                    self.report_irq();
                }
            }
        }
    }

    /// Armed: the proof is one interrupt. Frames that came in without one after `IRQ_PROOF_NS` mean it does not
    /// arrive on this machine: back to polling for this boot. The ping makes sure frames do come; when none comes at
    /// all (cable pulled, a silent network) the stage ends at `IRQ_PROOF_LIMIT_NS` with the question open: polled,
    /// but not given up, so the next bring-up asks again.
    fn check_irq_proof(&self, up: &dyn Uplink, since: u64, rx: u64) {
        if self.irq_count.load(Ordering::Relaxed) > self.irq_count_at_arm.load(Ordering::Relaxed) {
            self.irq_proven.store(true, Ordering::Release);
            self.say(Stage::Irq, Verdict::Ok, text::irq_ok("Unterbrechung"));
            self.check.store(CHECK_DONE, Ordering::Release);
            self.report_irq();
        } else if since >= IRQ_PROOF_NS && rx > self.rx_at_arm.load(Ordering::Relaxed) {
            self.irq_given_up.store(true, Ordering::Release);
            self.irq_off();
            self.report_irq();
            self.say(Stage::Irq, Verdict::Note, text::irq_none());
            self.check.store(CHECK_DONE, Ordering::Release);
        } else if since >= IRQ_PROOF_LIMIT_NS {
            self.irq_off();
            self.report_irq();
            self.say(Stage::Irq, Verdict::Note, text::irq_unproven(IRQ_PROOF_LIMIT_NS / 1_000_000_000));
            self.check.store(CHECK_DONE, Ordering::Release);
        } else if let Some((_, Some(router))) = up.lease(&self.name) {
            let _ = up.ping(router, PING_NS / 4);
        }
    }
}
