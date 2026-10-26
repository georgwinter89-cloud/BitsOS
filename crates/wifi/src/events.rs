//! What the station layer tells the log (rule B6): state changes, errors, retries, timeouts and
//! decisions as typed events. The crate never logs itself; the driver (WL-4) drains the events
//! after each call into klog (subsystem "wlan"). No key byte, PSK or passphrase is ever part of
//! an event (R8i): the types below have no field that could carry one.
//!
//! A caller that does not drain must not grow memory without end: past `cap` entries the oldest
//! go and are counted (as rtw89-core's EventLog).

use alloc::vec::Vec;
use core::fmt;

use crate::eapol::{Bad, Got};
use crate::mlme::JoinFailure;

/// How many events a log keeps by default before the oldest go. Not a resource count (R1): a
/// caller sets its own with `set_cap`.
pub const DEFAULT_EVENT_CAP: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLog<T> {
    buf: Vec<T>,
    cap: usize,
    dropped: u64,
}

impl<T> Default for EventLog<T> {
    fn default() -> EventLog<T> {
        EventLog {
            buf: Vec::new(),
            cap: DEFAULT_EVENT_CAP,
            dropped: 0,
        }
    }
}

impl<T> EventLog<T> {
    pub fn new() -> EventLog<T> {
        EventLog::default()
    }
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        self.trim();
    }
    pub fn push(&mut self, e: T) {
        self.buf.push(e);
        self.trim();
    }
    fn trim(&mut self) {
        if self.buf.len() > self.cap {
            let n = self.buf.len() - self.cap;
            self.buf.drain(..n);
            self.dropped = self.dropped.saturating_add(n as u64);
        }
    }
    pub fn take(&mut self) -> Vec<T> {
        core::mem::take(&mut self.buf)
    }
    pub fn as_slice(&self) -> &[T] {
        &self.buf
    }
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Why a link ended or a join is due again (supervisor)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkLoss {
    /// the AP deauthenticated or disassociated us
    EndedByAp { reason: u16 },
    /// the firmware's beacon filter reported no beacons
    BeaconLoss,
    /// the chip failed (health check, firmware halt): off and on again
    ChipFailed,
    /// a join failed
    JoinFailed,
}

impl fmt::Display for LinkLoss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LinkLoss::EndedByAp { reason } => write!(f, "ended by the AP, reason {reason}"),
            LinkLoss::BeaconLoss => f.write_str("no beacons from the AP"),
            LinkLoss::ChipFailed => f.write_str("the chip failed"),
            LinkLoss::JoinFailed => f.write_str("the join failed"),
        }
    }
}

/// One event of the station layer
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiEvent {
    AuthSent {
        attempt: u32,
    },
    AuthOk,
    AssocSent {
        attempt: u32,
    },
    Associated {
        aid: u16,
    },
    EapolTaken {
        got: Got,
    },
    /// an EAPOL frame the supplicant dropped (its state stays)
    EapolDropped {
        why: Bad,
    },
    PtkInstalled,
    /// a retransmitted message 3: answered, the PTK in force kept (KRACK guard)
    PtkKept,
    GtkInstalled {
        id: u8,
    },
    /// the GTK in force came again: answered, not installed again (KRACK guard)
    GtkKept {
        id: u8,
    },
    Connected {
        aid: u16,
        group_key: bool,
    },
    JoinFailed {
        why: JoinFailure,
    },
    EndedByAp {
        reason: u16,
        disassoc: bool,
    },
    /// the supervisor's next join, `in_ms` from now (`attempt` since the link was lost)
    RejoinScheduled {
        attempt: u32,
        in_ms: u64,
        why: LinkLoss,
    },
    RejoinStart {
        attempt: u32,
    },
    LinkUp,
    /// the supervisor was told to stop (WLAN off)
    Off,
}

impl fmt::Display for WifiEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WifiEvent::AuthSent { attempt } => write!(f, "authentication sent (try {attempt})"),
            WifiEvent::AuthOk => f.write_str("authenticated"),
            WifiEvent::AssocSent { attempt } => {
                write!(f, "association request sent (try {attempt})")
            }
            WifiEvent::Associated { aid } => write!(f, "associated, AID {aid}"),
            WifiEvent::EapolTaken { got } => write!(f, "EAPOL {} taken", got.as_str()),
            WifiEvent::EapolDropped { why } => write!(f, "EAPOL frame dropped: {why}"),
            WifiEvent::PtkInstalled => f.write_str("pairwise key in"),
            WifiEvent::PtkKept => f.write_str("message 3 again: answered, pairwise key kept"),
            WifiEvent::GtkInstalled { id } => write!(f, "group key {id} in"),
            WifiEvent::GtkKept { id } => write!(f, "group key {id} in force came again: kept"),
            WifiEvent::Connected { aid, group_key } => write!(
                f,
                "connected, AID {aid}, group key {}",
                if *group_key { "in" } else { "missing" }
            ),
            WifiEvent::JoinFailed { why } => write!(f, "join failed: {why}"),
            WifiEvent::EndedByAp { reason, disassoc } => write!(
                f,
                "link ended by the AP ({}), reason {reason}",
                if *disassoc {
                    "disassociation"
                } else {
                    "deauthentication"
                }
            ),
            WifiEvent::RejoinScheduled {
                attempt,
                in_ms,
                why,
            } => {
                write!(f, "joining again in {in_ms} ms (try {attempt}): {why}")
            }
            WifiEvent::RejoinStart { attempt } => write!(f, "joining again (try {attempt})"),
            WifiEvent::LinkUp => f.write_str("link up"),
            WifiEvent::Off => f.write_str("WLAN off"),
        }
    }
}

impl WifiEvent {
    /// The B6 event name (`thing.what`) for klog
    pub fn name(&self) -> &'static str {
        match self {
            WifiEvent::AuthSent { .. } => "auth.sent",
            WifiEvent::AuthOk => "auth.ok",
            WifiEvent::AssocSent { .. } => "assoc.sent",
            WifiEvent::Associated { .. } => "assoc.ok",
            WifiEvent::EapolTaken { .. } => "eapol.taken",
            WifiEvent::EapolDropped { .. } => "eapol.dropped",
            WifiEvent::PtkInstalled => "key.ptk",
            WifiEvent::PtkKept => "key.ptk_kept",
            WifiEvent::GtkInstalled { .. } => "key.gtk",
            WifiEvent::GtkKept { .. } => "key.gtk_kept",
            WifiEvent::Connected { .. } => "link.up",
            WifiEvent::JoinFailed { .. } => "join.failed",
            WifiEvent::EndedByAp { .. } => "link.ended",
            WifiEvent::RejoinScheduled { .. } => "rejoin.scheduled",
            WifiEvent::RejoinStart { .. } => "rejoin.start",
            WifiEvent::LinkUp => "supervisor.up",
            WifiEvent::Off => "supervisor.off",
        }
    }
}
