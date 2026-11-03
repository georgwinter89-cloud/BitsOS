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
    module_at(out, Level::Warn, name, evs);
}

/// The events of chan, btc and the RF calibrations (`btc.event`, `chan.event`, `rfk.event`,
/// `rfk2.event`): states Linux logs at its debug masks (rtw89_debug), handled, not faults (WL-9)
pub fn part<E: Display>(out: &mut Vec<DevEvent>, name: &'static str, evs: Vec<E>) {
    module_at(out, Level::Info, name, evs);
}

/// `chip.found_on`: a Reload or a warm boot left the chip on; a handled state, not a fault (WL-9)
pub fn found_on(text: String) -> DevEvent {
    DevEvent::new(Level::Info, "chip.found_on", text)
}

/// A module's events as `name` events at `level`
pub fn module_at<E: Display>(
    out: &mut Vec<DevEvent>,
    level: Level,
    name: &'static str,
    evs: Vec<E>,
) {
    out.extend(
        evs.into_iter()
            .map(|e| DevEvent::new(level, name, format!("{e}"))),
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

/// The first failed join of a rhythm, held back (WL-9: on 24.10. the first join after boot failed
/// and the second went through): at Info when the next join goes through, at Warn when the next
/// fails too or the driver stops or restarts before it. `rejoin.scheduled` (Info) says at once
/// that a join failed and when the next comes.
#[derive(Debug, Default)]
pub struct QuietRetry {
    held: Option<DevEvent>,
}

impl QuietRetry {
    /// After a turn whose events start at `from` in `log`: `failed` whether its join failed,
    /// `first` whether that was the first join of the rhythm, `joined` whether one went through
    pub fn after_turn(
        &mut self,
        log: &mut Vec<DevEvent>,
        from: usize,
        failed: bool,
        first: bool,
        joined: bool,
    ) {
        let from = from.min(log.len());
        if joined && let Some(mut e) = self.held.take() {
            e.text = format!("{} (the next join went through)", e.text);
            log.insert(from, e);
        } else if failed {
            if let Some(mut e) = self.held.take() {
                e.level = Level::Warn;
                log.insert(from, e);
            } else if first
                && let Some(i) = log[from..].iter().position(|e| e.name == "join.failed")
            {
                let mut e = log.remove(from + i);
                e.level = Level::Info;
                self.held = Some(e);
            }
        }
    }

    /// The driver stops or restarts: a held failure is a warning after all
    pub fn flush(&mut self, log: &mut Vec<DevEvent>) {
        if let Some(mut e) = self.held.take() {
            e.level = Level::Warn;
            log.push(e);
        }
    }

    pub fn holds(&self) -> bool {
        self.held.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn ev(level: Level, name: &'static str) -> DevEvent {
        DevEvent::new(level, name, "t")
    }

    #[test]
    fn levels_by_meaning() {
        let mut out = Vec::new();
        for n in ["btc.event", "chan.event", "rfk.event", "rfk2.event"] {
            part(&mut out, n, vec!["x"]);
        }
        out.push(found_on(String::from("on")));
        assert!(out.iter().all(|e| e.level == Level::Info), "{out:?}");
        let mut warn = Vec::new();
        for n in ["fw.event", "mac.event", "phy.event", "tx.event"] {
            module(&mut warn, n, vec!["x"]);
        }
        assert!(warn.iter().all(|e| e.level == Level::Warn) && warn.len() == 4);
    }

    #[test]
    fn first_failure_held_until_the_next_join() {
        let mut q = QuietRetry::default();
        // the first join fails: held, the rest of the turn stays
        let mut log = vec![
            ev(Level::Info, "a"),
            ev(Level::Warn, "join.failed"),
            ev(Level::Info, "rejoin.scheduled"),
        ];
        q.after_turn(&mut log, 1, true, true, false);
        assert!(q.holds());
        assert_eq!(
            log.iter().map(|e| e.name).collect::<Vec<_>>(),
            ["a", "rejoin.scheduled"]
        );
        // the next goes through: the failure at Info, before this turn's lines
        let mut log = vec![ev(Level::Info, "link.up")];
        q.after_turn(&mut log, 0, false, false, true);
        assert!(!q.holds());
        assert_eq!((log[0].name, log[0].level), ("join.failed", Level::Info));
        assert!(log[0].text.ends_with("(the next join went through)"));
        // a quiet turn in between changes nothing
        let mut log = vec![ev(Level::Warn, "join.failed")];
        q.after_turn(&mut log, 0, true, true, false);
        let mut quiet = vec![ev(Level::Debug, "x")];
        q.after_turn(&mut quiet, 0, false, false, false);
        assert!(q.holds() && quiet.len() == 1);
        // the next fails too: the first at Warn, the second as it came
        let mut log = vec![ev(Level::Warn, "join.failed")];
        q.after_turn(&mut log, 0, true, false, false);
        assert!(!q.holds());
        assert_eq!(log.len(), 2);
        assert!(
            log.iter()
                .all(|e| e.name == "join.failed" && e.level == Level::Warn)
        );
        // a later failure that is not the first of a rhythm is never held
        let mut log = vec![ev(Level::Warn, "join.failed")];
        q.after_turn(&mut log, 0, true, false, false);
        assert!(!q.holds() && log.len() == 1);
        // stop or restart while held: Warn
        let mut log = vec![ev(Level::Warn, "join.failed")];
        q.after_turn(&mut log, 0, true, true, false);
        let mut end = Vec::new();
        q.flush(&mut end);
        assert_eq!((end.len(), end[0].level), (1, Level::Warn));
        q.flush(&mut end);
        assert_eq!(end.len(), 1);
    }
}
