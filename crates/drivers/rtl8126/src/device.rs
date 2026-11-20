//! One RTL8126A: bring-up in Linux's order with a self-check that stops by itself, the `NetDevice` data path, link
//! watch, restart rules and `Quiesce`. Generic over `Machine`, so host tests run exactly this code against the fake
//! card. Shaped after `igc::device`; what differs comes from the chip:
//! - both OCP windows are two-step (address, then data), so one owner at a time holds them (`ocp`);
//! - the card starts polled; its interrupt is armed only after the router answered, and taken away again when frames
//!   move without it. A dead interrupt can then never be the reason for a round without network.

use crate::hw::Hw;
use crate::irq;
use crate::machine::{IrqHook, Machine};
use crate::ocp::{self, Mdio};
use crate::phy;
use crate::regs;
use crate::ring::{self, RxDrop, RxOut, RxRing, TxError, TxRing};
use crate::stage::{text, Line, Report, Stage, Verdict};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use devmodel::net::{LinkState, MacAddr, NetStats};
use devmodel::DevError;
use ksync::SpinLock;

mod check;
mod restart;
mod types;
mod up;

pub use types::*;

#[derive(Default)]
struct Stats {
    rx_frames: AtomicU64,
    tx_frames: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_dropped: AtomicU64,
    rx_errors: AtomicU64,
    tx_dropped: AtomicU64,
    tx_done: AtomicU64,
}

const IRQ_POLLED: u8 = 0;
const IRQ_ARMED: u8 = 1;
const REPORTED_NONE: u8 = 0;
const REPORTED_POLLED: u8 = 1;
const REPORTED_IRQ: u8 = 2;
/// `check` after the last stage: waiting for the first interrupt, and nothing left to prove.
const CHECK_IRQ_PROOF: u8 = 8;
const CHECK_DONE: u8 = 9;

pub struct Card<M: Machine> {
    name: String,
    /// The PCI location, for the report line.
    slot: String,
    hw: M,
    cfg: Config,
    rx: SpinLock<RxRing<M::Mem>>,
    tx: SpinLock<TxRing<M::Mem>>,
    /// The two OCP windows have an owner (bring-up, a link look, the stop).
    ocp: AtomicBool,
    state: AtomicU8,
    /// MAC in the low 48 bits (byte 0 lowest); 0 until the first bring-up read it.
    mac: AtomicU64,
    xid: AtomicU32,
    /// CPlusCmd as the probe found it: the start sequence of a MAC-only restart needs it again.
    cp_cmd: AtomicU32,
    link_up: AtomicBool,
    speed_mbps: AtomicU32,
    full_duplex: AtomicBool,
    next_link_look: AtomicU64,
    next_health: AtomicU64,
    next_restart: AtomicU64,
    retries: AtomicU32,
    restarting: AtomicBool,
    housekeeping: AtomicBool,
    phy_errors: AtomicU32,
    tx_timeouts: AtomicU32,
    /// The firmware wait for the stick was spent (it happens once per boot).
    fw_waited: AtomicBool,
    /// The chip answered as an RTL8126A once. Before that not one of its registers is written, not even to stop it.
    identified: AtomicBool,
    /// Quiesce does not log; bit 31 = note present, low byte = the state before, bits 8.. = failed stop steps.
    quiesce_note: AtomicU32,
    irq_mode: AtomicU8,
    /// An interrupt arrived once: later bring-ups arm it at once.
    irq_proven: AtomicBool,
    /// Frames moved without an interrupt: polled for the rest of this boot.
    irq_given_up: AtomicBool,
    pending: Arc<AtomicBool>,
    irq_count: Arc<AtomicU64>,
    irq_reported: AtomicU8,
    /// The stage the self-check is proving now (a `Stage`, or `CHECK_*`), since when, and its counters.
    check: AtomicU8,
    check_since: AtomicU64,
    check_said: AtomicBool,
    link_since: AtomicU64,
    rx_at_start: AtomicU64,
    rx_retried: AtomicBool,
    ping_tries: AtomicU32,
    irq_count_at_arm: AtomicU64,
    rx_at_arm: AtomicU64,
    irq_seen: AtomicU64,
    rx_seen: AtomicU64,
    irq_silent: AtomicU32,
    worker: AtomicBool,
    due: SpinLock<Option<(State, &'static str, Depth)>>,
    report: Report,
    stats: Stats,
}

fn mac_str(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl<M: Machine> Card<M> {
    /// Takes the ring memory (4 KiB + 4 KiB of descriptors, 4 MiB of receive buffers in four runs, 512 KiB of
    /// transmit slots) and checks every address; the card itself is not touched.
    pub fn new(name: String, slot: String, hw: M, cfg: Config) -> Result<Self, String> {
        let mk = |what: &str, len: usize| hw.alloc_dma(len).map_err(|e| format!("{what}: {e}"));
        let mut chunks = Vec::new();
        for _ in 0..ring::RX_CHUNKS {
            chunks.push(mk("RX buffers", ring::RX_CHUNK_BYTES)?);
        }
        let rx = RxRing::new(mk("RX descriptors", crate::desc::RING_BYTES)?, chunks, DMA_LIMIT).map_err(|e| format!("RX ring: {e:?}"))?;
        let tx = TxRing::new(mk("TX descriptors", crate::desc::RING_BYTES)?, mk("TX buffers", ring::TX_BUF_BYTES)?, DMA_LIMIT)
            .map_err(|e| format!("TX ring: {e:?}"))?;
        Ok(Card {
            name,
            slot,
            hw,
            cfg,
            rx: SpinLock::named("rtl8126.rx", rx),
            tx: SpinLock::named("rtl8126.tx", tx),
            ocp: AtomicBool::new(false),
            state: AtomicU8::new(State::Down as u8),
            mac: AtomicU64::new(0),
            xid: AtomicU32::new(0),
            cp_cmd: AtomicU32::new(0),
            link_up: AtomicBool::new(false),
            speed_mbps: AtomicU32::new(0),
            full_duplex: AtomicBool::new(false),
            next_link_look: AtomicU64::new(0),
            next_health: AtomicU64::new(0),
            next_restart: AtomicU64::new(0),
            retries: AtomicU32::new(0),
            restarting: AtomicBool::new(false),
            housekeeping: AtomicBool::new(false),
            phy_errors: AtomicU32::new(0),
            tx_timeouts: AtomicU32::new(0),
            fw_waited: AtomicBool::new(false),
            identified: AtomicBool::new(false),
            quiesce_note: AtomicU32::new(0),
            irq_mode: AtomicU8::new(IRQ_POLLED),
            irq_proven: AtomicBool::new(false),
            irq_given_up: AtomicBool::new(false),
            pending: Arc::new(AtomicBool::new(false)),
            irq_count: Arc::new(AtomicU64::new(0)),
            irq_reported: AtomicU8::new(REPORTED_NONE),
            check: AtomicU8::new(CHECK_DONE),
            check_since: AtomicU64::new(0),
            check_said: AtomicBool::new(false),
            link_since: AtomicU64::new(0),
            rx_at_start: AtomicU64::new(0),
            rx_retried: AtomicBool::new(false),
            ping_tries: AtomicU32::new(0),
            irq_count_at_arm: AtomicU64::new(0),
            rx_at_arm: AtomicU64::new(0),
            irq_seen: AtomicU64::new(0),
            rx_seen: AtomicU64::new(0),
            irq_silent: AtomicU32::new(0),
            worker: AtomicBool::new(false),
            due: SpinLock::named("rtl8126.due", None),
            report: Report::default(),
            stats: Stats::default(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn hw(&self) -> &M {
        &self.hw
    }
    pub fn report(&self) -> &Report {
        &self.report
    }
    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub fn xid(&self) -> u16 {
        self.xid.load(Ordering::Relaxed) as u16
    }
    pub fn mac_bytes(&self) -> [u8; 6] {
        let m = self.mac.load(Ordering::Relaxed);
        core::array::from_fn(|i| (m >> (8 * i)) as u8)
    }
    pub fn link(&self) -> LinkState {
        LinkState {
            up: self.state() == State::Running && self.link_up.load(Ordering::Acquire),
            speed_mbps: self.speed_mbps.load(Ordering::Relaxed),
            full_duplex: self.full_duplex.load(Ordering::Relaxed),
        }
    }
    /// The stage the self-check is at, `None` when it has nothing left to prove.
    pub fn checking(&self) -> Option<Stage> {
        match self.check.load(Ordering::Acquire) {
            CHECK_DONE => None,
            CHECK_IRQ_PROOF => Some(Stage::Irq),
            v => Stage::from_u8(v),
        }
    }
    /// (failed restarts in a row, next restart time).
    pub fn retry_info(&self) -> (u32, u64) {
        (self.retries.load(Ordering::Relaxed), self.next_restart.load(Ordering::Relaxed))
    }
    /// (frames the card holds, frames it finished).
    pub fn tx_info(&self) -> (u32, u64) {
        let tx = self.tx.lock();
        (tx.in_flight(), tx.done())
    }
    pub fn irq_setup(&self) -> &'static str {
        if self.irq_mode.load(Ordering::Acquire) == IRQ_ARMED { "irq" } else { "polled" }
    }
    pub fn irq_count(&self) -> u64 {
        self.irq_count.load(Ordering::Relaxed)
    }
    /// (the longest wait for the PHY window in microseconds, accesses slower than Linux's ten looks of 25 us).
    pub fn phy_waits(&self) -> (u32, u32) {
        self.hw.phy_waits().map_or((0, 0), |w| (w.max_us(), w.over_linux()))
    }
    pub fn set_restart_worker(&self, on: bool) {
        self.worker.store(on, Ordering::Release);
    }
    pub fn restart_due(&self) -> bool {
        self.due.lock().is_some()
    }
    /// The worker's part: runs the queued restart, if any, with sleeping waits.
    pub fn run_due_restart(&self) {
        let due = self.due.lock().take();
        if let Some((from, reason, depth)) = due {
            self.auto_restart(from, reason, depth, true);
        }
    }

    pub(super) fn say(&self, stage: Stage, verdict: Verdict, text: String) {
        let (ifname, s, v) = (self.name.as_str(), stage.name(), verdict.name());
        match verdict {
            Verdict::Ok => klog::event!(Info, "net", "rtl8126.stage", ifname = ifname, stage = s, verdict = v; "{}", text.as_str()),
            Verdict::Note => klog::event!(Warn, "net", "rtl8126.stage", ifname = ifname, stage = s, verdict = v; "{}", text.as_str()),
            Verdict::Failed => klog::event!(Error, "net", "rtl8126.stage", ifname = ifname, stage = s, verdict = v; "{}", text.as_str()),
        }
        self.report.set(Line { stage, verdict, text });
    }

    // --- the OCP windows' owner -------------------------------------------------------------------------------

    pub(super) fn claim_ocp(&self) -> bool {
        !self.ocp.swap(true, Ordering::AcqRel)
    }
    pub(super) fn release_ocp(&self) {
        self.ocp.store(false, Ordering::Release);
    }
    pub(super) fn claim_ocp_within(&self, limit_ns: u64) -> bool {
        let end = self.hw.now_ns().saturating_add(limit_ns);
        loop {
            if self.claim_ocp() {
                return true;
            }
            if self.hw.now_ns() >= end {
                return false;
            }
            self.hw.relax();
        }
    }

    // --- interrupt ----------------------------------------------------------------------------------------------

    /// The hard-IRQ hook: note it, count it, wake the stack (lock-free, no heap, no klog). The stack's next round
    /// calls `receive`, which acknowledges and re-arms.
    pub(super) fn hook(&self) -> IrqHook {
        let (pending, count) = (self.pending.clone(), self.irq_count.clone());
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
            pending.store(true, Ordering::Release);
            net::netdev::irq_wake();
        })
    }

    /// `rtl_irq_enable` (M:1661). Linux enables right after its mask-and-acknowledge; here the card may have run
    /// polled for a while, so what collected meanwhile is acknowledged first: a cause left standing would hold the
    /// line high, and the first new frame would raise no message.
    pub(super) fn irq_on(&self) {
        self.pending.store(false, Ordering::Release);
        self.irq_mode.store(IRQ_ARMED, Ordering::Release);
        self.hw.wr32(regs::INTR_STATUS, u32::MAX);
        self.hw.wr32(regs::INTR_MASK, irq::IRQ_MASK);
    }

    pub(super) fn irq_off(&self) {
        self.irq_mode.store(IRQ_POLLED, Ordering::Release);
        self.hw.wr32(regs::INTR_MASK, 0);
    }

    /// Tells the stack whether this card wakes it by interrupt (only on change). Thread context.
    pub fn report_irq(&self) {
        // Only a proven interrupt lets the stack stop polling: while the proof runs, frames must keep moving.
        let driven = self.state() == State::Running
            && self.irq_mode.load(Ordering::Acquire) == IRQ_ARMED
            && self.irq_proven.load(Ordering::Acquire)
            && !self.hw.irq_degraded();
        let now = if driven { REPORTED_IRQ } else { REPORTED_POLLED };
        if self.irq_reported.swap(now, Ordering::AcqRel) != now {
            net::netdev::set_irq_driven(&self.name, driven);
        }
    }

    /// `rtl8169_interrupt`'s body (M:4857-4881), run by the stack's thread: mask, acknowledge what was seen, and
    /// look at the link at once if that changed.
    fn irq_service(&self) {
        if let irq::Irq::Events { status, link_changed } = irq::classify(self.hw.rd32(regs::INTR_STATUS)) {
            self.hw.wr32(regs::INTR_MASK, 0);
            self.hw.wr32(regs::INTR_STATUS, status);
            if link_changed {
                self.next_link_look.store(0, Ordering::Relaxed);
            }
        }
    }

    /// The ring is empty: interrupts may come again (`rtl8169_poll`'s end, M:4925-4926). Written every time, so a
    /// lost note can never leave the card masked.
    fn irq_rearm(&self) {
        if self.irq_mode.load(Ordering::Relaxed) == IRQ_ARMED {
            self.hw.wr32(regs::INTR_MASK, irq::IRQ_MASK);
        }
    }

    // --- link and health ------------------------------------------------------------------------------------------

    /// The PHY's link state; logs edges. On link-up the MAC's "send low-power idle" bits are cleared: what
    /// `r8169_phylink_handler` does when EEE was not negotiated (M:4972-4973, :4959).
    fn look_link(&self) {
        if !self.claim_ocp() {
            return;
        }
        let link = phy::read_link(&self.hw, &Mdio::for_look());
        let lpi_off = match &link {
            Ok(l) if l.up && !self.link_up.load(Ordering::Acquire) => ocp::mac_modify(&self.hw, 0xe040, 0x0003, 0).is_ok(),
            _ => true,
        };
        self.release_ocp();
        let ifname = self.name.as_str();
        let Ok(l) = link else {
            if self.phy_errors.fetch_add(1, Ordering::Relaxed) + 1 >= PHY_ERRORS_RESTART {
                self.phy_errors.store(0, Ordering::Relaxed);
                self.want_restart(State::Running, "the PHY window does not answer", Depth::Full);
            }
            return;
        };
        self.phy_errors.store(0, Ordering::Relaxed);
        if l.up {
            self.speed_mbps.store(l.speed_mbps.unwrap_or(0), Ordering::Relaxed);
            self.full_duplex.store(l.full_duplex, Ordering::Relaxed);
        }
        if self.link_up.swap(l.up, Ordering::AcqRel) != l.up {
            if l.up {
                // Frames that waited out a link-down are not late: the transmit clock starts again, as Linux's
                // watchdog does when the carrier comes back (`dev_activate`).
                if let Some(mut tx) = self.tx.try_lock() {
                    tx.touch(self.hw.now_ns());
                }
                self.link_since.store(self.hw.now_ns(), Ordering::Relaxed);
                klog::event!(Info, "net", "link.up", ifname = ifname, speed_mbps = l.speed_mbps.unwrap_or(0), full_duplex = l.full_duplex,
                    lpi_off = lpi_off);
            } else {
                klog::event!(Warn, "net", "link.down", ifname = ifname);
            }
        }
    }

    /// Once a second: the card still answers, sent frames come back, the interrupt still arrives.
    fn health(&self) {
        let hw = &self.hw;
        if hw.rd32(regs::TX_CONFIG) == u32::MAX {
            self.want_restart(State::Running, "registers read all ones", Depth::Full);
            return;
        }
        let now = hw.now_ns();
        let link = self.link_up.load(Ordering::Acquire);
        if let Some(mut tx) = self.tx.try_lock() {
            let _ = tx.reclaim(hw, now);
            let (held, progress, done) = (tx.in_flight(), tx.progress_ns(), tx.done());
            drop(tx);
            self.note_tx_done(done);
            if link && held > 0 && now.saturating_sub(progress) >= TX_TIMEOUT_NS {
                let n = self.tx_timeouts.fetch_add(1, Ordering::AcqRel) + 1;
                // Link up but nothing leaves: a bridge without bus master shows in this check.
                hw.dma_check();
                if n > TX_TIMEOUT_STOP {
                    self.halt(Stage::Rings, text::tx_stuck(TX_TIMEOUT_STOP));
                } else {
                    // Linux's recovery first; when that sent nothing either, the whole card.
                    let depth = if n == 1 { Depth::Mac } else { Depth::Full };
                    self.want_restart(State::Running, "transmit timeout", depth);
                }
                return;
            }
        }
        if self.irq_mode.load(Ordering::Acquire) != IRQ_ARMED || !self.irq_proven.load(Ordering::Acquire) {
            return;
        }
        // Frames in the last second and no interrupt in it, several seconds running: it stopped arriving.
        let (irqs, rx) = (self.irq_count.load(Ordering::Relaxed), self.stats.rx_frames.load(Ordering::Relaxed));
        let quiet = self.rx_seen.swap(rx, Ordering::Relaxed) < rx && self.irq_seen.swap(irqs, Ordering::Relaxed) == irqs;
        let silent = if quiet { self.irq_silent.fetch_add(1, Ordering::Relaxed) + 1 } else { 0 };
        if !quiet {
            self.irq_silent.store(0, Ordering::Relaxed);
        }
        if hw.irq_degraded() || silent >= IRQ_SILENT_TICKS {
            self.irq_given_up.store(true, Ordering::Release);
            self.irq_off();
            klog::event!(Warn, "net", "rtl8126.irq_lost", ifname = self.name.as_str(), silent_seconds = silent;
                "the interrupt no longer arrives (or its vector was released): polled for the rest of this boot");
        }
    }

    /// `total`: frames the card has finished so far. Any new one ends a run of transmit timeouts.
    fn note_tx_done(&self, total: u64) {
        if self.stats.tx_done.swap(total, Ordering::Relaxed) < total {
            self.tx_timeouts.store(0, Ordering::Relaxed);
        }
    }

    // --- NetDevice, as inherent methods (the trait impl in lib.rs forwards; host tests call these) ---------------

    fn count_tx(&self, len: usize) {
        self.stats.tx_frames.fetch_add(1, Ordering::Relaxed);
        self.stats.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
        TX_FRAMES.inc();
        TX_BYTES.add(len as u64);
    }

    /// Running -> Failed with a retry in a second, the card stopped at once: no DMA into rings the driver now
    /// considers broken. Never touches another state.
    fn fail(&self, during: &str, error: &str) {
        if self.state.compare_exchange(State::Running as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.link_up.store(false, Ordering::Release);
            if !self.restarting.swap(true, Ordering::AcqRel) {
                self.stop_hw(false, false);
                self.restarting.store(false, Ordering::Release);
            }
            self.next_restart.store(self.hw.now_ns().saturating_add(FAIL_RETRY_NS), Ordering::Relaxed);
            klog::event!(Error, "net", "rtl8126.failed", ifname = self.name.as_str(), during = during, error = error);
        }
    }

    pub fn transmit(&self, frame: &[u8]) -> Result<(), DevError> {
        // No frame to a card that is stopped or has no link (Linux carrier-off): the stack retries.
        if self.state() != State::Running || !self.link_up.load(Ordering::Acquire) {
            self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(DevError::Io);
        }
        let mut tx = self.tx.lock();
        match tx.put(&self.hw, frame, self.hw.now_ns()) {
            Ok(()) => {
                drop(tx);
                self.count_tx(frame.len());
                Ok(())
            }
            Err(TxError::BadFrame) => Err(DevError::InvalidArgument),
            Err(TxError::Full) => {
                drop(tx);
                TX_RING_FULL.inc();
                Err(DevError::Timeout)
            }
            Err(TxError::Memory) => {
                drop(tx);
                self.fail("transmit", "TX descriptor memory refused");
                Err(DevError::Io)
            }
        }
    }

    pub fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        if self.state() != State::Running {
            return None;
        }
        if self.irq_mode.load(Ordering::Acquire) == IRQ_ARMED && self.pending.swap(false, Ordering::AcqRel) {
            self.irq_service();
        }
        let mut rx = self.rx.lock();
        // Bounded: a peer sending only unusable frames cannot keep this loop going past one ring.
        for _ in 0..crate::desc::NUM_RX {
            match rx.take(buf) {
                Ok(RxOut::Empty) => break,
                Ok(RxOut::Frame(n)) => {
                    drop(rx);
                    self.stats.rx_frames.fetch_add(1, Ordering::Relaxed);
                    self.stats.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    RX_FRAMES.inc();
                    RX_BYTES.add(n as u64);
                    return Some(n);
                }
                Ok(RxOut::Dropped(why)) => {
                    self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    RX_DROPPED.inc();
                    if let RxDrop::Error { .. } = why {
                        self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                        RX_ERRORS.inc();
                    }
                }
                Err(_) => {
                    drop(rx);
                    self.fail("receive", "RX descriptor memory refused");
                    return None;
                }
            }
        }
        drop(rx);
        // `rtl8169_poll` cleans the transmit ring in the same round (M:4921).
        if let Some(mut tx) = self.tx.try_lock() {
            let _ = tx.reclaim(&self.hw, self.hw.now_ns());
            let done = tx.done();
            drop(tx);
            self.note_tx_done(done);
        }
        self.irq_rearm();
        None
    }

    /// Emergency transmit (panic mirror): no blocking lock, no klog, no allocation.
    pub fn poll_tx(&self, frame: &[u8]) -> bool {
        let ok = self.state() == State::Running
            && self.link_up.load(Ordering::Acquire)
            && self.tx.try_lock().is_some_and(|mut tx| tx.put(&self.hw, frame, self.hw.now_ns()).is_ok());
        if ok {
            self.count_tx(frame.len());
        } else {
            POLL_TX_DROPPED.inc();
        }
        ok
    }

    /// Housekeeping from the poller: link look (100 ms), health (1 s), retries of a failed card. One caller at a
    /// time; the poller runs on every idle CPU.
    pub fn poll(&self) {
        if self.housekeeping.swap(true, Ordering::AcqRel) {
            return;
        }
        self.log_quiesce_note();
        let now = self.hw.now_ns();
        match self.state() {
            State::Running => {
                if now >= self.next_link_look.load(Ordering::Relaxed) {
                    self.next_link_look.store(now.saturating_add(LINK_EVERY_NS), Ordering::Relaxed);
                    self.look_link();
                    self.report_irq();
                }
                if now >= self.next_health.load(Ordering::Relaxed) {
                    self.next_health.store(now.saturating_add(HEALTH_EVERY_NS), Ordering::Relaxed);
                    self.health();
                }
            }
            State::Failed => {
                self.report_irq();
                if now >= self.next_restart.load(Ordering::Relaxed) {
                    self.want_restart(State::Failed, "retry after a failure", Depth::Full);
                }
            }
            State::Down | State::Quiesced | State::Stopped => {}
        }
        self.housekeeping.store(false, Ordering::Release);
    }

    pub fn stats(&self) -> NetStats {
        let s = &self.stats;
        NetStats {
            rx_frames: s.rx_frames.load(Ordering::Relaxed),
            tx_frames: s.tx_frames.load(Ordering::Relaxed),
            rx_bytes: s.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: s.tx_bytes.load(Ordering::Relaxed),
            rx_dropped: s.rx_dropped.load(Ordering::Relaxed),
            tx_dropped: s.tx_dropped.load(Ordering::Relaxed),
        }
    }

    pub fn mac(&self) -> MacAddr {
        MacAddr(self.mac_bytes())
    }

    /// Stop for Reload, reboot and panic. Lock-free, no klog; the event is logged later from a note.
    pub fn quiesce(&self) {
        // Carrier off first: transmit and poll_tx refuse from here on.
        let was = State::from(self.state.swap(State::Quiesced as u8, Ordering::AcqRel));
        let link = self.link_up.swap(false, Ordering::AcqRel);
        let failed = self.stop_hw(was == State::Running && link, false);
        self.quiesce_note.store(1 << 31 | failed.min(0xffff) << 8 | was as u32, Ordering::Release);
    }

    /// `klog::shutdown::TxDone`: everything handed to the card is sent. `None` when the card is not running or the
    /// TX lock is held (the mirror then settles by time).
    pub fn tx_done(&self) -> Option<bool> {
        if self.state() != State::Running {
            return None;
        }
        let mut tx = self.tx.try_lock()?;
        let _ = tx.reclaim(&self.hw, self.hw.now_ns());
        Some(tx.in_flight() == 0)
    }

    /// Test hook: runs `f` with the TX lock held, as a panic in the middle of a transmit leaves it.
    pub fn with_tx_locked<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.tx.lock();
        f()
    }
}
