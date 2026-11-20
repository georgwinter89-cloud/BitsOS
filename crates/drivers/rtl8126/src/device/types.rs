//! What a card is configured with, the states it can be in, how a bring-up fails, and what the self-check asks the
//! stack.

use crate::bringup::ProbeError;
use crate::chip::ChipError;
use crate::machine::PciFail;
use crate::phy::PhyError;
use crate::ring::RingError;
use crate::seq::SeqError;
use crate::stage::{text, Stage};
use alloc::format;
use alloc::string::String;

/// CONFIG: `rtl8126_max_mbps=` caps what is advertised (default: all the PHY has, as Linux); the firmware wait is how long the first bring-up waits for the stick to be mounted.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub max_mbps: u32,
    pub fw_wait_ns: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config { max_mbps: 5000, fw_wait_ns: 30_000_000_000 }
    }
}

/// Linux lets this card DMA anywhere (64 bits, M:5700-5701). The first rounds stay below 4 GiB: the high address
/// words are then always 0, so a mistake in them cannot be what breaks a round.
pub const DMA_LIMIT: u64 = 1 << 32;

pub const LINK_EVERY_NS: u64 = 100_000_000;
pub const HEALTH_EVERY_NS: u64 = 1_000_000_000;
/// The self-check's deadlines.
pub const LINK_WAIT_NS: u64 = 20_000_000_000;
pub const RX_WAIT_NS: u64 = 30_000_000_000;
pub const LEASE_WAIT_NS: u64 = 30_000_000_000;
pub const PING_NS: u64 = 1_000_000_000;
pub const PING_TRIES: u32 = 3;
pub const IRQ_PROOF_NS: u64 = 5_000_000_000;
/// With no frame at all the proof cannot be had: after this long the stage says so and ends.
pub const IRQ_PROOF_LIMIT_NS: u64 = 30_000_000_000;
/// Seconds in a row with frames and without an interrupt before a proven interrupt counts as lost.
pub const IRQ_SILENT_TICKS: u32 = 5;
/// A frame the card holds this long with link is a transmit timeout: Linux's watchdog default of 5 s (the driver
/// sets no `watchdog_timeo`). The first one restarts the MAC as `rtl_task` does (M:4883-4913, `Depth::Mac`), the
/// next ones the whole card; after this many restarts without one frame sent it is stopped.
pub const TX_TIMEOUT_NS: u64 = 5_000_000_000;
pub const TX_TIMEOUT_STOP: u32 = 3;

/// How much of the card a restart rule takes down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// `rtl_reset_work` (M:4306-4319): the chip stopped and reset, the rings handed back, the start sequence. Bus
    /// master, PHY and link stay as they are, so the recovery takes milliseconds and not a new negotiation.
    Mac,
    /// Everything from the probe sequence on, firmware script and negotiation included.
    Full,
}
/// A PHY window that fails this many link looks in a row (one second) restarts the card.
pub const PHY_ERRORS_RESTART: u32 = 10;
/// Retry a card that did not come up: every 10 s six times, then every 60 s (as igc: the cable may be the only way in).
pub const RETRY_SOON_NS: u64 = 10_000_000_000;
pub const RETRY_SOON_TIMES: u32 = 6;
pub const RETRY_LATER_NS: u64 = 60_000_000_000;
pub const FAIL_RETRY_NS: u64 = 1_000_000_000;
/// Quiesce lets the TX ring drain this long (the log mirror's last lines).
pub const DRAIN_US: u32 = 100_000;
pub(super) const FW_STEP_US: u32 = 1_000_000;
pub(super) const OCP_WAIT_NS: u64 = 1_000_000_000;
pub(super) const RESTART_WAIT_NS: u64 = 60_000_000_000;

klog::counter!(pub(super) RX_FRAMES, "net.rtl8126.rx_frames");
klog::counter!(pub(super) TX_FRAMES, "net.rtl8126.tx_frames");
klog::counter!(pub(super) RX_BYTES, "net.rtl8126.rx_bytes");
klog::counter!(pub(super) TX_BYTES, "net.rtl8126.tx_bytes");
klog::counter!(pub(super) RX_DROPPED, "net.rtl8126.rx_dropped");
klog::counter!(pub(super) RX_ERRORS, "net.rtl8126.rx_errors");
klog::counter!(pub(super) TX_RING_FULL, "net.rtl8126.tx_ring_full");
klog::counter!(pub(super) POLL_TX_DROPPED, "net.rtl8126.poll_tx_dropped");
klog::counter!(pub(super) RESTARTS, "net.rtl8126.restarts");
klog::counter!(pub QUIESCE_STEP_FAILURES, "net.rtl8126.quiesce_step_failures");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Down = 0,
    Running = 1,
    /// Stopped by `Quiesce` (Reload, reboot, panic): stays stopped until `restart` is called explicitly.
    Quiesced = 2,
    /// Bring-up failed or a restart rule fired and the restart failed: the poller retries with back-off.
    Failed = 3,
    /// Stopped by the self-check or refused for good: bus master off, interrupts masked, no retry. Only an explicit
    /// `restart` (a ktest, the desk) tries again.
    Stopped = 4,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Running => "running",
            State::Quiesced => "quiesced",
            State::Failed => "failed",
            State::Stopped => "stopped",
        }
    }
    pub(super) fn from(v: u8) -> State {
        match v {
            1 => State::Running,
            2 => State::Quiesced,
            3 => State::Failed,
            4 => State::Stopped,
            _ => State::Down,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BringUpError {
    Probe(ProbeError),
    /// MAC OCP 0xd006 says fibre module (M:5688-5692): Linux then drives the chip another way, this driver not at all.
    Sfp,
    Phy(PhyError),
    Start(SeqError),
    Ring(&'static str, RingError),
    Pci(PciFail),
    /// Quiesce (or another state change) came in between.
    Stopped(State),
    /// The OCP windows stayed in use by another thread.
    Busy,
}

impl BringUpError {
    /// Errors a retry cannot fix: the card is stopped for good instead of retried.
    pub fn fatal(&self) -> bool {
        matches!(
            self,
            BringUpError::Probe(ProbeError::Chip(ChipError::Unknown(_)) | ProbeError::NoMac)
                | BringUpError::Sfp
                | BringUpError::Phy(PhyError::WrongId(_))
                | BringUpError::Pci(PciFail { refused: true, .. })
        )
    }
    pub fn stage(&self) -> Stage {
        match self {
            BringUpError::Probe(_) | BringUpError::Sfp | BringUpError::Pci(_) | BringUpError::Busy => Stage::Probe,
            BringUpError::Phy(_) => Stage::Phy,
            BringUpError::Start(_) | BringUpError::Ring(..) | BringUpError::Stopped(_) => Stage::Rings,
        }
    }
    pub fn text(&self) -> String {
        match self {
            BringUpError::Probe(e) => format!("probe: {e:?}"),
            BringUpError::Sfp => "the chip reports a fibre module (MAC OCP 0xd006)".into(),
            BringUpError::Phy(e) => format!("PHY: {e:?}"),
            BringUpError::Start(e) => format!("start sequence: {e:?}"),
            BringUpError::Ring(q, e) => format!("{q} ring: {e:?}"),
            BringUpError::Pci(e) => e.text.clone(),
            BringUpError::Stopped(s) => format!("stopped during bring-up (state {})", s.as_str()),
            BringUpError::Busy => "the OCP windows stayed in use".into(),
        }
    }
    /// The report page's line for this failure.
    pub(super) fn line(&self) -> String {
        match self {
            BringUpError::Probe(ProbeError::Chip(ChipError::Gone)) => text::probe_gone(),
            BringUpError::Probe(ProbeError::Chip(ChipError::Unknown(xid))) => text::probe_unknown(*xid),
            BringUpError::Probe(ProbeError::NoMac) => text::probe_other("sie hat keine gültige eigene Adresse"),
            BringUpError::Probe(ProbeError::Seq(_)) => text::probe_other("sie wird beim Zurücksetzen nicht fertig"),
            BringUpError::Sfp => text::probe_other("sie meldet ein Glasfaser-Modul, das hier nicht unterstützt wird"),
            BringUpError::Pci(_) => text::probe_other("der Steckplatz lässt sich nicht einschalten"),
            BringUpError::Busy => text::probe_other("sie ist noch von einem anderen Vorgang belegt"),
            BringUpError::Phy(PhyError::WrongId(id)) => text::phy_wrong(*id),
            BringUpError::Phy(PhyError::ResetStuck) => text::phy_failed("er wird beim Zurücksetzen nicht fertig"),
            BringUpError::Phy(PhyError::Firmware(_)) => text::phy_failed("die Firmware ließ sich nicht einspielen"),
            BringUpError::Phy(PhyError::Ocp(_)) => text::phy_failed("keine Antwort in der vorgesehenen Zeit"),
            BringUpError::Start(_) => text::start_failed("ein Schritt der Startfolge wird nicht fertig"),
            BringUpError::Ring(..) => text::start_failed("der Speicher für die Datenpakete passt nicht"),
            BringUpError::Stopped(_) => text::start_failed("sie wurde währenddessen angehalten"),
        }
    }
}

/// What the self-check asks the network stack. The kernel answers from `net::stack`; host tests from a table.
pub trait Uplink {
    /// The card's address and the router its lease named.
    fn lease(&self, ifname: &str) -> Option<([u8; 4], Option<[u8; 4]>)>;
    /// One echo request; the round trip in nanoseconds. May block up to `timeout_ns`.
    fn ping(&self, dst: [u8; 4], timeout_ns: u64) -> Option<u64>;
}
