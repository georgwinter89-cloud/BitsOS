//! One I226-V / 82576 card: bring-up in the spec's order, the `NetDevice` data path (polled at M0), link watch,
//! the restart rules and `Quiesce`. Generic over `Hw`, so host tests run exactly this code against a fake card.

use crate::hw::{until, Hw, IrqHook, PciFail, Sleepy};
use crate::irq::{self, IrqSetup};
use crate::phy::{self, PhyReset};
use crate::regs::{self, Model};
use crate::reset::{self, ResetKind};
use crate::ring::{RingError, Rx, RxDrop, RxRing, TxError, TxRing};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use devmodel::net::{LinkState, MacAddr, NetStats};
use devmodel::DevError;
use ksync::SpinLock;

/// Ring sizes (parameters, spec §3; not limits on a count of things, R1), whether to ask for interrupts (NW-4;
/// CONFIG `net_irq=off` turns them off) and the queue vector's gap between interrupts (CONFIG `igc_itr_us`).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub rx_ring: usize,
    pub tx_ring: usize,
    pub irqs: bool,
    pub itr_us: u32,
}

/// NW-7: 1024 RX descriptors (Linux: 256 by default, up to 4096, igc.h:385-387). QEMU's igb lost frames from
/// a full ring of 256 when 8 downloads sent their windows at once (the senders then waited 1.5 s for their
/// retransmit timer); 1024 frames are 12 ms of a 1 Gbit/s line, 2 MiB of buffers per card.
impl Default for Config {
    fn default() -> Self {
        Config { rx_ring: 1024, tx_ring: 256, irqs: true, itr_us: regs::ITR_US_DEFAULT }
    }
}

/// Link looks: STATUS every 100 ms (cheap); health and the restart rules once a second (`V1:netwatch.asm:20`).
pub const LINK_EVERY_NS: u64 = 100_000_000;
pub const HEALTH_EVERY_NS: u64 = 1_000_000_000;
/// A transmit that finds the ring full counts as "late" when the card finished nothing for this long; three in
/// a row with link restart the card (`V1:netwatch.asm:7-8`, NET_SEND_MS).
pub const TX_LATE_NS: u64 = 100_000_000;
pub const TX_LATE_RESTART: u32 = 3;
/// Given-back RX descriptors are handed to the card in groups of this many (one RDT write per group; Linux
/// `IGC_RX_BUFFER_WRITE`, linux-6.6/drivers/net/ethernet/intel/igc/igc.h:440): a burst never finds the ring
/// short of descriptors that were already free.
pub const RX_WRITE_EVERY: usize = 16;
/// Retry a card that did not come back: every 10 s six times, then every 60 s: the cable may be the only way
/// in (`V1:nicreset.asm:8-10`).
pub const RETRY_SOON_NS: u64 = 10_000_000_000;
pub const RETRY_SOON_TIMES: u32 = 6;
pub const RETRY_LATER_NS: u64 = 60_000_000_000;
/// Quiesce lets the TX ring drain this long (the mirror's last lines, ≤ 70 ms measured in V1).
pub const DRAIN_US: u64 = 100_000;
/// How long an explicit `restart` waits for one the poller is running (a bring-up takes up to ~4 s).
const RESTART_WAIT_NS: u64 = 10_000_000_000;
/// After a data-path failure (`fail`) the poller restarts the card this much later.
pub const FAIL_RETRY_NS: u64 = 1_000_000_000;
/// The I226's restart tries CTRL.DEV_RST instead of CTRL.RST on every other attempt from this many failed
/// restarts in a row on (Linux igc's reset bit; RST stays the default, it is the one proven on the Mini-PC).
pub const DEV_RST_AFTER: u32 = 3;

klog::counter!(RX_FRAMES, "net.igc.rx_frames");
klog::counter!(TX_FRAMES, "net.igc.tx_frames");
klog::counter!(RX_BYTES, "net.igc.rx_bytes");
klog::counter!(TX_BYTES, "net.igc.tx_bytes");
klog::counter!(RX_DROPPED, "net.igc.rx_dropped");
klog::counter!(TX_RING_FULL, "net.igc.tx_ring_full");
klog::counter!(POLL_TX_DROPPED, "net.igc.poll_tx_dropped");
klog::counter!(RESTARTS, "net.igc.restarts");
klog::counter!(HW_MISSED, "net.igc.hw_missed");
klog::counter!(HW_NO_BUFFER, "net.igc.hw_no_buffer");
klog::counter!(HW_CRC_ERRORS, "net.igc.hw_crc_errors");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Down = 0,
    Running = 1,
    /// Stopped by `Quiesce` (Reload, reboot, panic): stays stopped until `restart` is called explicitly.
    Quiesced = 2,
    /// Bring-up failed or a restart rule fired and the restart failed: the poller retries with back-off.
    Failed = 3,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Running => "running",
            State::Quiesced => "quiesced",
            State::Failed => "failed",
        }
    }
    fn from(v: u8) -> State {
        match v {
            1 => State::Running,
            2 => State::Quiesced,
            3 => State::Failed,
            _ => State::Down,
        }
    }
}

/// Why a bring-up failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BringUpError {
    /// Registers read all ones even after restoring the BARs: nothing answers there.
    Gone,
    /// No valid MAC address in RAL/RAH: the card is refused (spec §2.2).
    NoMac,
    Phy(phy::PhyError),
    Ring(&'static str, RingError),
    /// Quiesce (or another state change) came in between.
    Stopped(State),
    /// PC-2's `enable_device` refused (`refused`: R8d/R8a, final) or failed (power state stuck, busy: retried).
    Pci(PciFail),
}

impl BringUpError {
    /// Errors a retry cannot fix: the card is refused instead of retried.
    pub fn fatal(&self) -> bool {
        matches!(self, BringUpError::NoMac | BringUpError::Pci(PciFail { refused: true, .. }))
    }
    pub fn text(&self) -> String {
        match self {
            BringUpError::Gone => "registers read all ones".into(),
            BringUpError::NoMac => "no MAC address in RAL/RAH".into(),
            BringUpError::Phy(e) => e.as_str().into(),
            BringUpError::Ring(q, e) => format!("{q} queue: {e:?}"),
            BringUpError::Stopped(s) => format!("stopped during bring-up (state {})", s.as_str()),
            BringUpError::Pci(e) => e.text.clone(),
        }
    }
}

#[derive(Default)]
struct Stats {
    rx_frames: AtomicU64,
    tx_frames: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_dropped: AtomicU64,
    tx_dropped: AtomicU64,
    hw_missed: AtomicU64,
    hw_no_buffer: AtomicU64,
    hw_crc: AtomicU64,
}

/// Card statistics the hardware keeps (missed = no room in the FIFO; no buffer = no descriptor).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwStats {
    pub missed: u64,
    pub no_buffer: u64,
    pub crc_errors: u64,
}

const NO_TDH: u32 = u32::MAX;
const IRQ_POLLED: u8 = 0;
const IRQ_MSI: u8 = 1;
const IRQ_MSIX: u8 = 2;
/// `irq_reported`: nothing said yet / said polled / said interrupt-driven.
const REPORTED_NONE: u8 = 0;
const REPORTED_POLLED: u8 = 1;
const REPORTED_IRQ: u8 = 2;

pub struct Card<H: Hw> {
    name: String,
    model: Model,
    hw: H,
    rx: SpinLock<RxRing<H::Mem>>,
    tx: SpinLock<TxRing<H::Mem>>,
    state: AtomicU8,
    /// MAC in the low 48 bits (byte 0 lowest); 0 until the first bring-up read it.
    mac: AtomicU64,
    link_up: AtomicBool,
    speed_mbps: AtomicU32,
    full_duplex: AtomicBool,
    next_link_look: AtomicU64,
    next_health: AtomicU64,
    next_restart: AtomicU64,
    /// Failed restarts since the card last ran (for the 10 s x 6, then 60 s rule).
    retries: AtomicU32,
    restarting: AtomicBool,
    /// Quiesce does not log (it may run on the panic path); it leaves a note the next `poll`/`restart` logs.
    /// Bit 7 = note present, low bits = the state before.
    quiesce_note: AtomicU8,
    tx_late: AtomicU32,
    tdh_seen: AtomicU32,
    irq_mode: AtomicU8,
    /// MSI-X: the vectors of the other causes (low byte) and of queue 0 (high byte).
    irq_vecs: AtomicU32,
    /// Vectors that fired since the driver last looked (bit = index), set by the hard-IRQ hook.
    pending: Arc<AtomicU32>,
    /// What `net::netdev::set_irq_driven` was last told.
    irq_reported: AtomicU8,
    /// Interrupts wanted (`Config::irqs`) and the queue vector's EITR value.
    want_irqs: bool,
    itr: u32,
    /// A restart worker thread runs this card's restarts (`set_restart_worker`): the poller only queues them.
    worker: AtomicBool,
    /// The restart the poller queued for the worker: (state it must still be in, reason).
    due: SpinLock<Option<(State, &'static str)>>,
    stats: Stats,
}

fn mac_str(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl<H: Hw> Card<H> {
    /// Allocates the rings (DMA memory) without touching the card.
    pub fn new(name: String, model: Model, hw: H, cfg: Config) -> Result<Self, String> {
        let mk = |what: &str, n: usize, size: usize| hw.alloc_dma(n * size).map_err(|e| format!("{what}: {e}"));
        if !crate::ring::valid_count(cfg.rx_ring) || !crate::ring::valid_count(cfg.tx_ring) {
            return Err(format!("ring sizes {}/{}: want multiples of 8 from 8", cfg.rx_ring, cfg.tx_ring));
        }
        let rx = RxRing::new(mk("RX descriptors", cfg.rx_ring, regs::DESC_SIZE)?, mk("RX buffers", cfg.rx_ring, regs::BUF_SIZE)?, cfg.rx_ring)
            .map_err(|e| format!("RX ring: {e:?}"))?;
        let tx = TxRing::new(mk("TX descriptors", cfg.tx_ring, regs::DESC_SIZE)?, mk("TX buffers", cfg.tx_ring, regs::BUF_SIZE)?, cfg.tx_ring)
            .map_err(|e| format!("TX ring: {e:?}"))?;
        Ok(Card {
            name,
            model,
            hw,
            rx: SpinLock::named("igc.rx", rx),
            tx: SpinLock::named("igc.tx", tx),
            state: AtomicU8::new(State::Down as u8),
            mac: AtomicU64::new(0),
            link_up: AtomicBool::new(false),
            speed_mbps: AtomicU32::new(0),
            full_duplex: AtomicBool::new(false),
            next_link_look: AtomicU64::new(0),
            next_health: AtomicU64::new(0),
            next_restart: AtomicU64::new(0),
            retries: AtomicU32::new(0),
            restarting: AtomicBool::new(false),
            quiesce_note: AtomicU8::new(0),
            tx_late: AtomicU32::new(0),
            tdh_seen: AtomicU32::new(NO_TDH),
            irq_mode: AtomicU8::new(IRQ_POLLED),
            irq_vecs: AtomicU32::new(0),
            pending: Arc::new(AtomicU32::new(0)),
            irq_reported: AtomicU8::new(REPORTED_NONE),
            want_irqs: cfg.irqs,
            itr: regs::eitr_interval(cfg.itr_us.max(1)),
            worker: AtomicBool::new(false),
            due: SpinLock::named("igc.due", None),
            stats: Stats::default(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn model(&self) -> Model {
        self.model
    }
    pub fn hw(&self) -> &H {
        &self.hw
    }
    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
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
    /// (RX size, TX size, TX frames with the card, TX capacity).
    pub fn ring_info(&self) -> (usize, usize, usize, usize) {
        let rx = self.rx.lock().count();
        let tx = self.tx.lock();
        (rx, tx.count(), tx.in_flight(), tx.capacity())
    }
    /// (failed restarts in a row, next restart time).
    pub fn retry_info(&self) -> (u32, u64) {
        (self.retries.load(Ordering::Relaxed), self.next_restart.load(Ordering::Relaxed))
    }
    pub fn tx_late(&self) -> u32 {
        self.tx_late.load(Ordering::Relaxed)
    }
    pub fn hw_stats(&self) -> HwStats {
        HwStats {
            missed: self.stats.hw_missed.load(Ordering::Relaxed),
            no_buffer: self.stats.hw_no_buffer.load(Ordering::Relaxed),
            crc_errors: self.stats.hw_crc.load(Ordering::Relaxed),
        }
    }
    /// From now on the poller queues restarts (`restart_due`) for a worker thread that runs them with
    /// `run_due_restart`, instead of running them itself: a restart may take seconds.
    pub fn set_restart_worker(&self, on: bool) {
        self.worker.store(on, Ordering::Release);
    }
    /// A restart is queued for the worker.
    pub fn restart_due(&self) -> bool {
        self.due.lock().is_some()
    }
    /// The worker's part: runs the queued restart, if any, with sleeping waits (`Sleepy`).
    pub fn run_due_restart(&self) {
        let due = self.due.lock().take();
        if let Some((from, reason)) = due {
            self.auto_restart(from, reason, &Sleepy(&self.hw));
        }
    }

    /// First start at probe. A fatal error (no MAC) refuses the card; any other leaves it Failed with a retry
    /// scheduled, because the cable may be the only way in.
    pub fn start(&self) -> Result<(), BringUpError> {
        self.restarting.store(true, Ordering::Release);
        self.state.store(State::Down as u8, Ordering::Release);
        let r = self.bring_up(&self.hw);
        if let Err(e) = &r {
            self.after_failed_bring_up(e, true);
        }
        self.restarting.store(false, Ordering::Release);
        r
    }

    /// Reset, PHY, MAC, rings, bus master, in the spec's order (§2.2). Caller holds `restarting` and has set the
    /// state to Down. On any error the card is stopped again (reset, DRV_LOAD off, MSI off, bus master off), so
    /// a half-started card cannot DMA during the back-off and leaves nothing claimed. `hw` is the card's own
    /// (spinning waits) or `Sleepy` over it (the worker's restarts); the rings start with spinning waits either
    /// way, because they hold the ring locks.
    fn bring_up<W: Hw>(&self, hw: &W) -> Result<(), BringUpError> {
        let r = self.bring_up_steps(hw);
        if r.is_err() {
            self.stop_hw(false);
        }
        r
    }

    /// Down unless a Quiesce (or other state change) came in while this bring-up waited or slept.
    fn still_down(&self) -> Result<(), BringUpError> {
        match self.state() {
            State::Down => Ok(()),
            s => Err(BringUpError::Stopped(s)),
        }
    }

    fn bring_up_steps<W: Hw>(&self, hw: &W) -> Result<(), BringUpError> {
        let ifname = self.name.as_str();
        self.link_up.store(false, Ordering::Release);
        self.tx_late.store(0, Ordering::Relaxed);
        self.tdh_seen.store(NO_TDH, Ordering::Relaxed);
        // 1. MSI off, then bus master off before anything (PC-2 `disable_device`): a ring the firmware or a
        //    previous kernel left running writes nowhere, a Reload's MSI does not aim at a dead IDT. Memory
        //    decode stays as found until the BARs are known to be valid.
        hw.pci_off();
        // BARs a link retrain zeroed are written back first (spec §2.1/§2.3 "restore BARs first"): memory decode
        // on with BAR0 at 0 would make the card answer at address 0. PC-2 `restore_config`, from the copy the
        // last `enable_device` (or probe) saved.
        let restored = hw.restore_config();
        if restored > 0 {
            klog::event!(Warn, "net", "igc.config_restored", ifname = ifname, dwords = restored;
                "config space lost (link retrain or reset), written back");
        }
        // PC-2 `enable_device(false)`: D3hot -> D0 with its 10 ms (header written back if the wake reset it),
        // memory decode, INTx off; bus master stays off until the rings.
        hw.enable(false).map_err(BringUpError::Pci)?;
        if self.model == Model::I226 {
            hw.aspm_off();
        }
        if hw.rd(regs::STATUS) == !0 {
            return Err(BringUpError::Gone);
        }
        // 2.-6. PTM off, GIO master disable, mask, RCTL/TCTL, reset, auto-read.
        let kind = self.reset_kind();
        if kind == ResetKind::Device {
            klog::event!(Warn, "net", "igc.reset_fallback", ifname = ifname, attempt = self.retries.load(Ordering::Relaxed) + 1;
                "CTRL.DEV_RST instead of CTRL.RST after failed restarts");
        }
        let rr = reset::reset(hw, self.model, kind);
        if rr.gio_timeout || rr.auto_read_timeout {
            klog::event!(Debug, "net", "igc.reset_waits", ifname = ifname, gio_timeout = rr.gio_timeout,
                auto_read_timeout = rr.auto_read_timeout; "went on after the deadline, as Linux does");
        }
        if self.model == Model::I226 {
            reset::i226_defaults(hw);
        }
        reset::set_link_up(hw);
        let pr = phy::up(hw, self.model).map_err(BringUpError::Phy)?;
        if pr.acquired.smbi_cleared || pr.acquired.took_over_own_bit || pr.acquired.waited_for_firmware {
            klog::event!(Warn, "net", "igc.phy_semaphore", ifname = ifname, smbi_cleared = pr.acquired.smbi_cleared,
                took_over_own_bit = pr.acquired.took_over_own_bit, waited_for_firmware = pr.acquired.waited_for_firmware);
        }
        match pr.reset {
            Some(PhyReset::Blocked) => klog::event!(Info, "net", "igc.phy_reset_blocked", ifname = ifname; "MANC blocks PHY resets"),
            Some(PhyReset::NoCompletion) => klog::event!(Warn, "net", "igc.phy_reset_timeout", ifname = ifname),
            _ => {}
        }
        if let Some(e) = pr.advertise_error {
            klog::event!(Warn, "net", "igc.advertise_failed", ifname = ifname, error = e.as_str(); "the PHY keeps its own advertisement");
        }
        if let Some(e) = pr.release_error {
            klog::event!(Warn, "net", "igc.phy_release_failed", ifname = ifname, error = e.as_str();
                "PHY0 stays marked ours in SW_FW_SYNC; the next bring-up takes it over");
        }
        let mac = reset::read_mac(hw).ok_or(BringUpError::NoMac)?;
        self.mac.store(mac.iter().enumerate().fold(0u64, |m, (i, b)| m | (*b as u64) << (8 * i)), Ordering::Relaxed);
        // Nothing is switched on for a card a Quiesce stopped while this bring-up waited.
        self.still_down()?;
        // The card is ours now: its firmware leaves it alone. Bus master only now, with the bridges (PC-2).
        hw.set_bits(regs::CTRL_EXT, regs::CTRL_EXT_DRV_LOAD);
        hw.enable(true).map_err(BringUpError::Pci)?;
        for i in 0..regs::MTA_DWORDS {
            hw.wr(regs::MTA + 4 * i, 0);
        }
        self.rx.lock().start(&self.hw).map_err(|e| BringUpError::Ring("RX", e))?;
        self.tx.lock().start(&self.hw, hw.now_ns()).map_err(|e| BringUpError::Ring("TX", e))?;
        // NW-4: interrupts after bus master (an MSI is a DMA write; PC-3 wants `enable_device(true)` first) and
        // with the rings running. A set from the bring-up before was released by step 1's `pci_off`.
        let setup = if self.want_irqs { hw.setup_irqs(2, self.hook()) } else { IrqSetup::Polled };
        self.arm_irqs(hw, setup);
        // A Quiesce that ran meanwhile wins: the card stays stopped (the caller's error path stops it again).
        if let Err(s) = self.state.compare_exchange(State::Down as u8, State::Running as u8, Ordering::AcqRel, Ordering::Acquire) {
            return Err(BringUpError::Stopped(State::from(s)));
        }
        let now = hw.now_ns();
        self.next_link_look.store(now, Ordering::Relaxed);
        self.next_health.store(now.saturating_add(HEALTH_EVERY_NS), Ordering::Relaxed);
        self.retries.store(0, Ordering::Relaxed);
        let (rxs, txs) = (self.rx.lock().count(), self.tx.lock().count());
        klog::event!(Info, "net", "igc.up", ifname = ifname, model = self.model.name(), mac = mac_str(mac), rx_ring = rxs,
            tx_ring = txs, phy_reset = match pr.reset { Some(PhyReset::Done) => "done", Some(_) => "skipped", None => "n/a" },
            irq = setup.name(), itr_us = self.itr >> 2);
        if setup == IrqSetup::Polled && self.want_irqs {
            klog::event!(Warn, "net", "igc.irq_polled", ifname = ifname; "no MSI-X or MSI: the card is polled (fallback)");
        }
        self.report_irq();
        Ok(())
    }

    /// Stops the card's DMA and interrupts (`V1:igc.asm:129-142`, Quiesce in spec §2.2): receive off, optionally
    /// the TX ring drained, transmit off, the reset, DRV_LOAD off, MSI off **before** bus master off. Lock-free,
    /// spinning only, no klog: Quiesce runs it on the panic path.
    fn stop_hw(&self, drain: bool) {
        let hw = &self.hw;
        hw.wr(regs::RCTL, 0);
        if drain {
            let _ = until(hw, DRAIN_US, 100, || {
                let (h, t) = (hw.rd(regs::TDH), hw.rd(regs::TDT));
                h == t || h == !0
            });
        }
        hw.clear_bits(regs::TCTL, regs::TCTL_EN);
        let _ = reset::reset(hw, self.model, ResetKind::Global);
        hw.clear_bits(regs::CTRL_EXT, regs::CTRL_EXT_DRV_LOAD);
        // PC-2 `disable_device`: MSI off before bus master off; lock-free and silent, as Quiesce needs.
        hw.pci_off();
    }

    /// The reset bit for this attempt: RST, and on the I226 from `DEV_RST_AFTER` failed restarts on DEV_RST
    /// on every other attempt (RST stays in the rotation: it is the one proven on the Mini-PC).
    fn reset_kind(&self) -> ResetKind {
        let n = self.retries.load(Ordering::Relaxed);
        if self.model == Model::I226 && n >= DEV_RST_AFTER && (n - DEV_RST_AFTER).is_multiple_of(2) {
            ResetKind::Device
        } else {
            ResetKind::Global
        }
    }

    /// `first`: the probe's bring-up. Only there a fatal error (no MAC) refuses the card for good; on a restart
    /// it is retried with back-off like any other error (a half-read RAL/RAH after a reset may be transient, and
    /// the card is already known to the stack).
    fn after_failed_bring_up(&self, e: &BringUpError, first: bool) {
        let during = if first { "probe" } else { "restart" };
        if let BringUpError::Stopped(_) = e {
            klog::event!(Warn, "net", "igc.restart_aborted", ifname = self.name.as_str(), error = e.text());
            return;
        }
        if e.fatal() && first {
            self.state.store(State::Down as u8, Ordering::Release);
            klog::event!(Error, "net", "igc.refused", ifname = self.name.as_str(), during = during, error = e.text());
            return;
        }
        if self.state.compare_exchange(State::Down as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            klog::event!(Warn, "net", "igc.restart_aborted", ifname = self.name.as_str(), error = e.text());
            return;
        }
        let delay = self.schedule_retry();
        klog::event!(Error, "net", "igc.bring_up_failed", ifname = self.name.as_str(), during = during, error = e.text(),
            retry_in_ms = delay / 1_000_000);
    }

    /// Next retry: 10 s for the first six, then 60 s.
    fn schedule_retry(&self) -> u64 {
        let n = self.retries.fetch_add(1, Ordering::AcqRel);
        let delay = if n < RETRY_SOON_TIMES { RETRY_SOON_NS } else { RETRY_LATER_NS };
        self.next_restart.store(self.hw.now_ns().saturating_add(delay), Ordering::Relaxed);
        delay
    }

    fn log_quiesce_note(&self) {
        let n = self.quiesce_note.swap(0, Ordering::AcqRel);
        if n & 0x80 != 0 {
            klog::event!(Info, "net", "igc.quiesce", ifname = self.name.as_str(), was = State::from(n & 0x3f).as_str());
        }
    }

    /// Stops and starts the card again from any state (ktests, Reload's undo path), from a thread: its waits
    /// sleep. Waits a bounded time, yielding, for a restart the worker or the poller is running.
    pub fn restart(&self, reason: &str) -> Result<(), BringUpError> {
        let deadline = self.hw.now_ns().saturating_add(RESTART_WAIT_NS);
        while self.restarting.swap(true, Ordering::AcqRel) {
            if self.hw.now_ns() >= deadline {
                klog::event!(Warn, "net", "igc.restart_busy", ifname = self.name.as_str(), reason = reason);
                return Err(BringUpError::Stopped(self.state()));
            }
            self.hw.relax();
        }
        // This restart supersedes one the poller queued.
        *self.due.lock() = None;
        self.state.store(State::Down as u8, Ordering::Release);
        let r = self.restart_claimed(reason, &Sleepy(&self.hw));
        self.restarting.store(false, Ordering::Release);
        r
    }

    /// A restart rule fired (or a retry is due): queued for the worker thread if there is one, else run here.
    fn want_restart(&self, from: State, reason: &'static str) {
        if !self.worker.load(Ordering::Acquire) {
            self.auto_restart(from, reason, &self.hw);
            return;
        }
        let mut due = self.due.lock();
        if due.is_none() {
            klog::event!(Info, "net", "igc.restart_queued", ifname = self.name.as_str(), reason = reason);
        }
        *due = Some((from, reason));
    }

    /// A rule's restart: only if the card is still in `from` (a Quiesce in between cancels it).
    fn auto_restart<W: Hw>(&self, from: State, reason: &str, hw: &W) {
        if self.restarting.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.state.compare_exchange(from as u8, State::Down as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = self.restart_claimed(reason, hw);
        }
        self.restarting.store(false, Ordering::Release);
    }

    /// Caller holds `restarting` and has set the state to Down.
    fn restart_claimed<W: Hw>(&self, reason: &str, hw: &W) -> Result<(), BringUpError> {
        self.log_quiesce_note();
        RESTARTS.inc();
        let was_link = self.link_up.swap(false, Ordering::AcqRel);
        if was_link {
            klog::event!(Warn, "net", "link.down", ifname = self.name.as_str(), reason = "restart");
        }
        klog::event!(Warn, "net", "igc.restart", ifname = self.name.as_str(), reason = reason,
            attempt = self.retries.load(Ordering::Relaxed) + 1);
        let r = self.bring_up(hw);
        if let Err(e) = &r {
            self.after_failed_bring_up(e, false);
        }
        r
    }

    /// The hard-IRQ hook PC-3 runs for this card's vectors: note which fired and wake the stack (lock-free, no
    /// heap, no klog). The stack's next round calls `receive`, which looks (`irq_service`) and re-arms.
    fn hook(&self) -> IrqHook {
        let p = self.pending.clone();
        Arc::new(move |i: u16| {
            p.fetch_or(1u32 << (i & 31), Ordering::AcqRel);
            net::netdev::irq_wake();
        })
    }

    /// Programs the causes for `setup` (polled: every cause masked) and remembers the mode.
    fn arm_irqs<W: Hw>(&self, hw: &W, setup: IrqSetup) {
        self.pending.store(0, Ordering::Release);
        match (setup, setup.msix_vectors()) {
            (_, Some((other, queue))) => {
                self.irq_vecs.store(other as u32 | (queue as u32) << 8, Ordering::Release);
                irq::prepare_msix(hw, self.model, other, queue, self.itr);
                self.irq_mode.store(IRQ_MSIX, Ordering::Release);
            }
            (IrqSetup::Msi, None) => {
                irq::prepare_msi(hw, self.model, self.itr);
                self.irq_mode.store(IRQ_MSI, Ordering::Release);
            }
            _ => {
                reset::quiet(hw, self.model);
                self.irq_mode.store(IRQ_POLLED, Ordering::Release);
            }
        }
    }

    /// How the card's interrupts arrive now.
    pub fn irq_setup(&self) -> &'static str {
        match self.irq_mode.load(Ordering::Acquire) {
            IRQ_MSIX => "msix",
            IRQ_MSI => "msi",
            _ => "polled",
        }
    }

    /// Tells the stack whether this card wakes it by interrupt (only on change): running, MSI or MSI-X armed, and
    /// the PCI core still delivering (not released, no storm hold). Thread context.
    pub fn report_irq(&self) {
        let driven = self.state() == State::Running && self.irq_mode.load(Ordering::Acquire) != IRQ_POLLED && !self.hw.irqs_degraded();
        let now = if driven { REPORTED_IRQ } else { REPORTED_POLLED };
        if self.irq_reported.swap(now, Ordering::AcqRel) != now {
            net::netdev::set_irq_driven(&self.name, driven);
        }
    }

    /// The look an interrupt asked for, before the ring is read: MSI acknowledges (ICR read, EICR = ~0); MSI-X
    /// reads ICR when the other vector fired and re-arms it. A link change is looked at at once.
    fn irq_service(&self) {
        if self.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let p = self.pending.swap(0, Ordering::AcqRel);
        let hw = &self.hw;
        let icr = match self.irq_mode.load(Ordering::Acquire) {
            IRQ_MSI => irq::ack(hw, self.model),
            IRQ_MSIX => {
                let v = self.irq_vecs.load(Ordering::Acquire);
                let (other, queue) = (v & 0xFF, (v >> 8) & 0xFF);
                if p & (1 << other) == 0 {
                    0
                } else {
                    let icr = hw.rd(self.model.icr());
                    // One vector for both: re-armed with the queue when the ring is empty.
                    if other != queue {
                        hw.wr(regs::EIMS, 1 << other);
                    }
                    icr
                }
            }
            _ => 0,
        };
        if icr & irq::ICR_LSC != 0 && self.state() == State::Running {
            self.look_link();
        }
    }

    /// MSI-X: the queue vector masked itself when it fired (EIAM); the ring is empty now, so it may fire again
    /// (NAPI). Written every time (a posted write): a vector can never stay masked by a lost note.
    fn irq_rearm(&self) {
        if self.irq_mode.load(Ordering::Relaxed) == IRQ_MSIX {
            let queue = (self.irq_vecs.load(Ordering::Relaxed) >> 8) & 0xFF;
            self.hw.wr(regs::EIMS, 1 << queue);
        }
    }

    /// Link from STATUS; logs edges.
    fn look_link(&self) {
        let s = self.hw.rd(regs::STATUS);
        let (up, speed, fd) = reset::link_from_status(self.model, s);
        if up {
            self.speed_mbps.store(speed, Ordering::Relaxed);
            self.full_duplex.store(fd, Ordering::Relaxed);
        }
        let was = self.link_up.swap(up, Ordering::AcqRel);
        if was != up {
            if up {
                self.tdh_seen.store(NO_TDH, Ordering::Relaxed);
                klog::event!(Info, "net", "link.up", ifname = self.name.as_str(), speed_mbps = speed, full_duplex = fd,
                    status = klog::Hex(s as u64));
            } else {
                klog::event!(Warn, "net", "link.down", ifname = self.name.as_str(), status = klog::Hex(s as u64));
            }
        }
    }

    /// Once a second: hardware counters and the restart rules (`V1:netwatch.asm`, Linux `igc_watchdog_task`).
    fn health(&self) {
        let hw = &self.hw;
        if hw.rd(regs::STATUS) == !0 {
            self.want_restart(State::Running, "registers read all ones");
            return;
        }
        let (mpc, rnbc, crc) = (hw.rd(regs::MPC) as u64, hw.rd(regs::RNBC) as u64, hw.rd(regs::CRCERRS) as u64);
        if mpc + rnbc + crc > 0 {
            self.stats.hw_missed.fetch_add(mpc, Ordering::Relaxed);
            self.stats.hw_no_buffer.fetch_add(rnbc, Ordering::Relaxed);
            self.stats.hw_crc.fetch_add(crc, Ordering::Relaxed);
            HW_MISSED.add(mpc);
            HW_NO_BUFFER.add(rnbc);
            HW_CRC_ERRORS.add(crc);
        }
        let link = self.link_up.load(Ordering::Acquire);
        if let Some(mut tx) = self.tx.try_lock() {
            let _ = tx.reclaim(hw.now_ns());
            let held = tx.in_flight();
            drop(tx);
            if !link && held > 0 {
                // Frames held since the link went: the I226 never sends them until restarted (Linux resets it
                // with the cable out, so the restart costs no link of its own).
                let tdh = hw.rd(regs::TDH);
                if self.tdh_seen.swap(tdh, Ordering::AcqRel) == tdh {
                    self.want_restart(State::Running, "frames stuck since the link went");
                    return;
                }
            } else {
                self.tdh_seen.store(NO_TDH, Ordering::Relaxed);
            }
        }
        if self.irq_mode.load(Ordering::Acquire) == IRQ_MSIX {
            // Linux `igc_watchdog_task`: a software interrupt on every vector once a second, so a ring whose
            // interrupt got lost is still cleaned (and both vectors are re-armed through the normal path).
            let v = self.irq_vecs.load(Ordering::Acquire);
            hw.wr(regs::EICS, (1 << (v & 0xFF)) | (1 << ((v >> 8) & 0xFF)));
        }
        if link && self.tx_late.load(Ordering::Relaxed) >= TX_LATE_RESTART {
            // Link up but the ring does not move: a bridge without bus master (UR) shows in this check.
            hw.dma_check();
            self.want_restart(State::Running, "3 sends in a row found no free descriptor within 100 ms");
        }
    }

    fn frame_ok(frame: &[u8]) -> bool {
        (14..=regs::TX_MAX_FRAME).contains(&frame.len())
    }

    fn count_tx(&self, len: usize) {
        self.stats.tx_frames.fetch_add(1, Ordering::Relaxed);
        self.stats.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
        TX_FRAMES.inc();
        TX_BYTES.add(len as u64);
    }

    /// Running -> Failed with a retry scheduled, the card stopped at once (no DMA into rings the driver now
    /// considers broken while it waits). Never touches another state: a Quiesce that raced with the failing
    /// transmit/receive stays in force.
    fn fail(&self, during: &str, error: &str) {
        if self.state.compare_exchange(State::Running as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.link_up.store(false, Ordering::Release);
            // A restart running already does the stop itself.
            if !self.restarting.swap(true, Ordering::AcqRel) {
                self.stop_hw(false);
                self.restarting.store(false, Ordering::Release);
            }
            // Retry after a second: a ring error is the driver's or a glitch; the card itself may be fine. Not
            // counted as a failed restart (those follow the 10 s / 60 s rule).
            self.next_restart.store(self.hw.now_ns().saturating_add(FAIL_RETRY_NS), Ordering::Relaxed);
            klog::event!(Error, "net", "igc.failed", ifname = self.name.as_str(), during = during, error = error);
        }
    }

    // --- NetDevice, as inherent methods (the trait impl in lib.rs forwards; host tests call these) ---

    pub fn transmit(&self, frame: &[u8]) -> Result<(), DevError> {
        if !Self::frame_ok(frame) {
            return Err(DevError::InvalidArgument);
        }
        // No frame to a card that is stopped or has no link (Linux carrier-off, spec §2.3): the stack retries.
        if self.state() != State::Running || !self.link_up.load(Ordering::Acquire) {
            self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(DevError::Io);
        }
        let now = self.hw.now_ns();
        let mut tx = self.tx.lock();
        match tx.put(&self.hw, frame, now) {
            Ok(()) => {
                drop(tx);
                self.tx_late.store(0, Ordering::Relaxed);
                self.count_tx(frame.len());
                Ok(())
            }
            Err(TxError::Full) => {
                let stalled = now.saturating_sub(tx.progress_ns()) >= TX_LATE_NS;
                drop(tx);
                TX_RING_FULL.inc();
                if stalled {
                    self.tx_late.fetch_add(1, Ordering::Relaxed);
                }
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
        self.irq_service();
        let mut rx = self.rx.lock();
        // Bounded (B4): a peer sending only unusable frames cannot keep this loop going past one ring.
        for _ in 0..rx.count() {
            match rx.take(buf) {
                Ok(Rx::Empty) => {
                    rx.flush(&self.hw);
                    drop(rx);
                    self.irq_rearm();
                    return None;
                }
                Ok(Rx::Frame(n)) => {
                    if rx.pending() >= RX_WRITE_EVERY.min(rx.count() / 4).max(1) {
                        rx.flush(&self.hw);
                    }
                    drop(rx);
                    self.stats.rx_frames.fetch_add(1, Ordering::Relaxed);
                    self.stats.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                    RX_FRAMES.inc();
                    RX_BYTES.add(n as u64);
                    return Some(n);
                }
                Ok(Rx::Dropped(why)) => {
                    self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                    RX_DROPPED.inc();
                    if why != RxDrop::TooBig {
                        klog::event!(Debug, "net", "igc.rx_drop", ifname = self.name.as_str(), reason = match why {
                            RxDrop::NotEop => "no end of packet",
                            RxDrop::Error => "frame error",
                            _ => "bad length",
                        });
                    }
                }
                Err(e) => {
                    drop(rx);
                    self.fail("receive", if e == RingError::Memory { "RX descriptor memory refused" } else { "RX ring" });
                    return None;
                }
            }
        }
        rx.flush(&self.hw);
        drop(rx);
        self.irq_rearm();
        None
    }

    /// Emergency transmit (panic mirror): no blocking lock, no klog, no allocation; drops when the TX lock is held
    /// (the panic may have interrupted a transmit), the ring is full, or there is no link.
    pub fn poll_tx(&self, frame: &[u8]) -> bool {
        if !Self::frame_ok(frame) || self.state() != State::Running || !self.link_up.load(Ordering::Acquire) {
            POLL_TX_DROPPED.inc();
            return false;
        }
        let Some(mut tx) = self.tx.try_lock() else {
            POLL_TX_DROPPED.inc();
            return false;
        };
        let ok = tx.put(&self.hw, frame, self.hw.now_ns()).is_ok();
        drop(tx);
        if ok {
            self.count_tx(frame.len());
        } else {
            POLL_TX_DROPPED.inc();
        }
        ok
    }

    /// Housekeeping from the poller: RX doorbell flush, link look (100 ms), health and restart rules (1 s),
    /// retries of a failed card.
    pub fn poll(&self) {
        self.log_quiesce_note();
        let now = self.hw.now_ns();
        match self.state() {
            State::Running => {
                if let Some(mut rx) = self.rx.try_lock() {
                    rx.flush(&self.hw);
                }
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
                    self.want_restart(State::Failed, "retry after a failure");
                }
            }
            State::Down | State::Quiesced => {}
        }
    }

    pub fn stats(&self) -> NetStats {
        let s = &self.stats;
        NetStats {
            rx_frames: s.rx_frames.load(Ordering::Relaxed),
            tx_frames: s.tx_frames.load(Ordering::Relaxed),
            rx_bytes: s.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: s.tx_bytes.load(Ordering::Relaxed),
            rx_dropped: s.rx_dropped.load(Ordering::Relaxed) + s.hw_missed.load(Ordering::Relaxed),
            tx_dropped: s.tx_dropped.load(Ordering::Relaxed),
        }
    }

    pub fn mac(&self) -> MacAddr {
        MacAddr(self.mac_bytes())
    }

    /// Stop for Reload, reboot and panic: receive off, the TX ring drained (bounded), transmit off, the reset,
    /// DRV_LOAD off, MSI off before bus master off. Lock-free, no klog; the event is logged later from a note.
    pub fn quiesce(&self) {
        let was = State::from(self.state.swap(State::Quiesced as u8, Ordering::AcqRel));
        let link = self.link_up.swap(false, Ordering::AcqRel);
        self.stop_hw(was == State::Running && link);
        self.quiesce_note.store(0x80 | was as u8, Ordering::Release);
    }

    /// Test hook: fail the card as a broken ring would.
    pub fn fail_for_test(&self) {
        self.fail("test", "injected failure");
    }
    /// Test hook: runs `f` with the TX lock held, as a panic in the middle of a transmit leaves it.
    pub fn with_tx_locked<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.tx.lock();
        f()
    }
}
