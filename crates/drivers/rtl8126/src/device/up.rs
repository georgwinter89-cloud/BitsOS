//! Bring-up and stop of a card: Linux's order from `rtl_init_one` to `phy_start`, and back.

use super::*;
use crate::bringup::{self, ProbeError};
use crate::chip;
use crate::fwscript::Script;
use crate::machine::{FwFile, IrqSetup, Sleepy};
use crate::ocp::{self, Mdio};
use crate::phy;
use crate::ring::RingError;
use crate::seq::{PassedOver, StartCtx};
use crate::stage::text;
use alloc::vec::Vec;
use core::cell::Cell;

impl<M: Machine> Card<M> {

    /// First start. `sleepy`: on a thread that may sleep (the worker). A fatal error stops the card for good; any
    /// other leaves it Failed with a retry scheduled, because the cable may be the only way in. Only a card that is
    /// Down is started: probe registers the card for Quiesce before the worker gets to it, and one that was stopped
    /// in between stays stopped (`State::Quiesced`), as on every other path.
    pub fn start(&self, sleepy: bool) -> Result<(), BringUpError> {
        if self.restarting.swap(true, Ordering::AcqRel) {
            return Err(BringUpError::Busy);
        }
        let r = self.still_down().and_then(|()| self.bring_up(sleepy));
        if let Err(e) = &r {
            self.after_failed_bring_up(e, "start");
        }
        self.restarting.store(false, Ordering::Release);
        r
    }

    /// Caller holds `restarting` and has set the state to Down. On any error the card is stopped again (bus master
    /// off, interrupts masked, chip reset), so a half-started card cannot DMA during the back-off.
    pub(super) fn bring_up(&self, sleepy: bool) -> Result<(), BringUpError> {
        if !self.claim_ocp_within(OCP_WAIT_NS) {
            return Err(BringUpError::Busy);
        }
        self.report.reopen(Stage::Probe);
        let r = if sleepy { self.bring_up_steps(&Sleepy(&self.hw), true) } else { self.bring_up_steps(&self.hw, false) };
        if let Err(e) = &r {
            if !matches!(e, BringUpError::Stopped(_)) {
                self.say(e.stage(), Verdict::Failed, e.line());
            }
            self.stop_hw(false, true);
        }
        self.release_ocp();
        r
    }

    pub(super) fn still_down(&self) -> Result<(), BringUpError> {
        match self.state() {
            State::Down => Ok(()),
            s => Err(BringUpError::Stopped(s)),
        }
    }

    /// The firmware file, parsed. The first bring-up of a boot waits for the stick if the initramfs has no copy;
    /// never on a thread that must not sleep.
    fn firmware(&self, name: &str, may_wait: bool) -> Result<Option<(String, Vec<u8>)>, BringUpError> {
        let wait = if may_wait && !self.fw_waited.swap(true, Ordering::AcqRel) { self.cfg.fw_wait_ns } else { 0 };
        let end = self.hw.now_ns().saturating_add(wait);
        loop {
            match self.hw.firmware(name) {
                FwFile::Found { path, bytes } => return Ok(Some((path, bytes))),
                FwFile::Missing { why } => {
                    if self.hw.now_ns() >= end {
                        klog::event!(Warn, "net", "rtl8126.fw_missing", ifname = self.name.as_str(), name = name; "{}", why.as_str());
                        return Ok(None);
                    }
                }
            }
            self.still_down()?;
            self.hw.pause_us(FW_STEP_US);
        }
    }

    /// `rtl_init_one` (M:5608-5774), `rtl_open` (M:5081-5130) and `rtl8169_up` (M:5029-5043), in that order. `w` is the
    /// register access with the waits this thread may make.
    fn bring_up_steps<W: Hw>(&self, w: &W, may_wait: bool) -> Result<(), BringUpError> {
        let ifname = self.name.as_str();
        // Before the first touch: a card that Quiesce stopped since the caller looked is left alone.
        self.still_down()?;
        self.link_up.store(false, Ordering::Release);
        self.phy_errors.store(0, Ordering::Relaxed);
        self.irq_mode.store(IRQ_POLLED, Ordering::Release);
        // MSI and bus master off before anything: a ring the firmware or a previous kernel left running writes
        // nowhere. Then the header a link retrain may have lost, D0 and memory decoding (M:5639); ASPM off in the
        // link control (Linux: L1 at probe, M:5681-5685, and L0s too after a transmit timeout, M:4900-4904).
        self.hw.pci_off();
        let restored = self.hw.restore_config();
        if restored > 0 {
            klog::event!(Warn, "net", "rtl8126.config_restored", ifname = ifname, dwords = restored);
        }
        self.hw.enable(false).map_err(BringUpError::Pci)?;
        self.hw.aspm_off();

        let probed = bringup::probe_lenient(w);
        if !matches!(probed, Err(ProbeError::Chip(_))) {
            self.identified.store(true, Ordering::Release);
        }
        let (probe, mut over) = probed.map_err(BringUpError::Probe)?;
        self.cp_cmd.store(u32::from(probe.cp_cmd), Ordering::Relaxed);
        self.xid.store(u32::from(chip::xid_of(self.hw.rd32(regs::TX_CONFIG))), Ordering::Relaxed);
        self.mac.store(probe.mac.iter().enumerate().fold(0u64, |m, (i, b)| m | (*b as u64) << (8 * i)), Ordering::Relaxed);
        if ocp::mac_read(w, 0xd006).map_err(|e| BringUpError::Probe(ProbeError::Seq(e.into())))? & 0xff == 0x07 {
            return Err(BringUpError::Sfp);
        }
        self.say(Stage::Probe, Verdict::Ok, text::probe_ok(probe.chip.name, self.xid(), &self.slot));

        let file = self.firmware(probe.chip.firmware, may_wait)?;
        let script = match &file {
            None => {
                self.say(Stage::Firmware, Verdict::Note, text::firmware_missing());
                None
            }
            Some((path, bytes)) => match Script::parse(bytes) {
                Ok(s) => {
                    let version = String::from_utf8_lossy(s.version()).into_owned();
                    let expected = version == chip::FIRMWARE_3.version && bytes.len() == chip::FIRMWARE_3.bytes;
                    klog::event!(Info, "net", "rtl8126.fw_file", ifname = ifname, path = path.as_str(), bytes = bytes.len(),
                        version = version.as_str(), actions = s.len(), as_measured = expected);
                    let verdict = if expected { Verdict::Ok } else { Verdict::Note };
                    self.say(Stage::Firmware, verdict, text::firmware_ok(&version, expected));
                    Some(s)
                }
                Err(e) => {
                    klog::event!(Warn, "net", "rtl8126.fw_bad", ifname = ifname, path = path.as_str(), error = format!("{e:?}"));
                    self.say(Stage::Firmware, Verdict::Note, text::firmware_bad());
                    None
                }
            },
        };

        // M:5034: bus master, with the bridges above. The chip is in reset since the probe sequence: nothing moves
        // until ChipCmd enables it at the end of the start sequence.
        self.still_down()?;
        self.hw.enable(true).map_err(BringUpError::Pci)?;

        let mut mdio = Mdio::default();
        let up = phy::bring_up(w, &mut mdio, script.as_ref()).map_err(BringUpError::Phy)?;
        let modes = phy::capped(phy::abilities(w, &mdio).map_err(|e| BringUpError::Phy(e.into()))?, self.cfg.max_mbps);
        if let Some(p) = up.played {
            let (max_wait_us, slow_waits) = self.phy_waits();
            klog::event!(Info, "net", "rtl8126.fw_played", ifname = ifname, steps = p.steps, writes = p.writes, delay_ms = p.delay_ms,
                max_wait_us = max_wait_us, slow_waits = slow_waits);
        }
        self.say(Stage::Phy, Verdict::Ok, text::phy_ok(up.id));

        self.still_down()?;
        over.add(self.start_mac(w)?);
        self.log_passed_over(&over);
        // `phy_start` (M:5042): advertise and (re)start autonegotiation.
        phy::config_aneg(w, &mut mdio, phy::advert(modes)).map_err(|e| BringUpError::Phy(e.into()))?;

        // An interrupt that worked before is armed again at once; otherwise the card starts polled (module text).
        if self.irq_proven.load(Ordering::Acquire) && !self.irq_given_up.load(Ordering::Acquire) {
            if self.hw.setup_irq(self.hook()) != IrqSetup::Polled {
                self.irq_on();
            }
        }
        if let Err(s) = self.state.compare_exchange(State::Down as u8, State::Running as u8, Ordering::AcqRel, Ordering::Acquire) {
            return Err(BringUpError::Stopped(State::from(s)));
        }
        let now = self.hw.now_ns();
        self.next_link_look.store(now, Ordering::Relaxed);
        self.next_health.store(now.saturating_add(HEALTH_EVERY_NS), Ordering::Relaxed);
        self.retries.store(0, Ordering::Relaxed);
        self.rx_at_start.store(self.stats.rx_frames.load(Ordering::Relaxed), Ordering::Relaxed);
        self.begin_check(Stage::Link as u8, now);
        klog::event!(Info, "net", "rtl8126.up", ifname = ifname, chip = probe.chip.name, xid = klog::Hex(self.xid() as u64),
            mac = mac_str(probe.mac), firmware = script.is_some(), modes = klog::Hex(modes as u64), irq = self.irq_setup(),
            phy_max_wait_us = self.phy_waits().0);
        self.report_irq();
        Ok(())
    }

    /// `rtl_reset_work`'s body (M:4306-4319): the cleanup, every receive descriptor handed to the chip and the
    /// transmit ring emptied while it is in reset, then the start sequence. The last part of a bring-up and all of a
    /// MAC-only restart. Caller holds the OCP windows.
    pub(super) fn start_mac<W: Hw>(&self, w: &W) -> Result<PassedOver, BringUpError> {
        let (tx_ring, rx_ring) = (self.tx.lock().desc_addr(), self.rx.lock().desc_addr());
        let cp_cmd = self.cp_cmd.load(Ordering::Relaxed) as u16;
        let ctx = StartCtx::new(cp_cmd, tx_ring, rx_ring).map_err(|_| BringUpError::Ring("TX/RX", RingError::Layout("ring address")))?;
        let armed: Cell<Result<(), BringUpError>> = Cell::new(Ok(()));
        let now = self.hw.now_ns();
        let started = bringup::start_lenient(w, &ctx, || {
            let r = self.rx.lock().arm_all().map_err(|e| BringUpError::Ring("RX", e));
            armed.set(r.and_then(|()| self.tx.lock().reset(now).map_err(|e| BringUpError::Ring("TX", e))));
        });
        let over = started.map_err(BringUpError::Start)?;
        armed.into_inner()?;
        Ok(over)
    }

    pub(super) fn log_passed_over(&self, over: &PassedOver) {
        if over.any() {
            klog::event!(Warn, "net", "rtl8126.passed_over", ifname = self.name.as_str(), pci_settings = over.pci, waits = over.waits,
                first_wait = format!("{:?}", over.first_wait);
                "went on as Linux does: a wait ran out, or a config-space setting could not be made (ASPM is off)");
        }
    }

    /// Stops DMA and interrupts in Linux's order (`rtl8169_down`, M:5004-5027): bus master off first, then the chip
    /// (mask and acknowledge, accept nothing, RXDV gate, reset, exit-L1 bits off). Spinning only, no klog: Quiesce
    /// runs it on the panic path. Returns how many steps of the stop failed.
    pub(super) fn stop_hw(&self, drain: bool, have_ocp: bool) -> u32 {
        let hw = &self.hw;
        if drain {
            let mut left = DRAIN_US / 100;
            // A held TX lock (a panic inside a transmit) ends the drain: the frames cannot be counted then.
            while let Some(mut tx) = self.tx.try_lock() {
                let _ = tx.reclaim(hw, hw.now_ns());
                if tx.in_flight() == 0 || left == 0 {
                    break;
                }
                drop(tx);
                left -= 1;
                hw.delay_us(100);
            }
        }
        // On the panic path the other CPUs stand still, perhaps inside a link look: wait briefly, then go on anyway.
        let claimed = !have_ocp && {
            let mut tries = 200;
            loop {
                if self.claim_ocp() {
                    break true;
                }
                if tries == 0 {
                    break false;
                }
                tries -= 1;
                hw.delay_us(10);
            }
        };
        hw.pci_off();
        // A chip that never identified itself gets no register written: its sequences are not known to be these.
        let failed = if self.identified.load(Ordering::Acquire) { bringup::stop(hw) } else { 0 };
        self.irq_mode.store(IRQ_POLLED, Ordering::Release);
        if claimed {
            self.release_ocp();
        }
        if failed > 0 {
            QUIESCE_STEP_FAILURES.add(failed as u64);
        }
        failed
    }
}
