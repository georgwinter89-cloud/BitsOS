//! What the driver tells the log (B6): every module keeps its own events (crate::events), the
//! driver drains them after each call into one list of [`DevEvent`]s, which the kernel thread
//! writes to klog (subsystem "wlan"). No event carries a key, the PMK or the passphrase: the
//! modules' Display texts name keys only by slot and id (checked by the fake tests' R8i scans).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Display;

use crate::sta::StaEvent;
use wifi::events::WifiEvent;

/// klog's levels the driver uses
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

/// One log line of the driver: `name` is the klog event name (`thing.what`), `text` its message
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevEvent {
    pub level: Level,
    pub name: &'static str,
    pub text: String,
}

impl DevEvent {
    pub fn new(level: Level, name: &'static str, text: impl Into<String>) -> DevEvent {
        DevEvent {
            level,
            name,
            text: text.into(),
        }
    }
}

/// A module's warnings as `name` events at Warn (the modules keep only what Linux warns about)
pub fn module<E: Display>(out: &mut Vec<DevEvent>, name: &'static str, evs: Vec<E>) {
    out.extend(
        evs.into_iter()
            .map(|e| DevEvent::new(Level::Warn, name, format!("{e}"))),
    );
}

fn wifi_level(e: &WifiEvent) -> Level {
    match e {
        WifiEvent::EapolTaken { .. } | WifiEvent::AuthSent { .. } | WifiEvent::AssocSent { .. } => {
            Level::Debug
        }
        WifiEvent::JoinFailed { .. }
        | WifiEvent::EndedByAp { .. }
        | WifiEvent::EapolDropped { .. }
        | WifiEvent::PtkKept
        | WifiEvent::GtkKept { .. } => Level::Warn,
        _ => Level::Info,
    }
}

/// The station's events: its own names, levels by what they mean
pub fn sta(out: &mut Vec<DevEvent>, evs: Vec<StaEvent>) {
    for e in evs {
        let level = match &e {
            StaEvent::Wifi(w) => wifi_level(w),
            // per frame: Debug (klog's rate limit keeps a flood short); counted for the status
            StaEvent::DataDropped { .. } => Level::Debug,
            StaEvent::ScanDone { .. } | StaEvent::Rfkill { blocked: false } => Level::Info,
            StaEvent::ChipError { .. } => Level::Error,
            _ => Level::Warn,
        };
        out.push(DevEvent::new(level, e.name(), format!("{e}")));
    }
}
