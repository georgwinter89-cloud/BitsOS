//! SYS-A: what the task manager may do to a row, and when it must ask (Georg's decision 3 of 14.11.): ending his own
//! programs and their Vorrang Niedrig or Normal are free; Vorrang Hoch, and anything on BitsOS's own services, only
//! after a question answered with his real mouse or keyboard (Origin::Physical, as Durchblick's consent: `ctl`, MCP
//! and ui-act cannot answer it). Processes of root or another user cannot be touched from the desktop at all (the
//! kernel says EPERM), and the desktop itself or init are never ended from here.

use crate::sys_group::Row;

/// The Vorrang choices and their nice values. Hoch is -5: RLIMIT_NICE of the session allows no more (sys_limit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vorrang {
    Niedrig,
    Normal,
    Hoch,
}

impl Vorrang {
    pub const ALL: [Vorrang; 3] = [Vorrang::Niedrig, Vorrang::Normal, Vorrang::Hoch];
    pub fn nice(self) -> i32 {
        match self {
            Vorrang::Niedrig => 10,
            Vorrang::Normal => 0,
            Vorrang::Hoch => -5,
        }
    }
    pub fn of_nice(n: i32) -> Vorrang {
        match n {
            n if n > 0 => Vorrang::Niedrig,
            n if n < 0 => Vorrang::Hoch,
            _ => Vorrang::Normal,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Vorrang::Niedrig => "Niedrig",
            Vorrang::Normal => "Normal",
            Vorrang::Hoch => "Hoch",
        }
    }
    /// The word of the habit event (K2) and of `ctl system vorrang`.
    pub fn word(self) -> &'static str {
        match self {
            Vorrang::Niedrig => "niedrig",
            Vorrang::Normal => "normal",
            Vorrang::Hoch => "hoch",
        }
    }
    pub fn of_word(w: &str) -> Option<Vorrang> {
        Vorrang::ALL.into_iter().find(|v| v.word() == w)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// The polite end (close the windows, else SIGTERM).
    End,
    /// "Sofort beenden".
    Kill,
    Prio(Vorrang),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    Free,
    /// Only after a question answered with real input.
    Ask,
    /// Not from here, with the reason the page shows.
    No(String),
}

pub const NOT_OURS: &str = "Gehört dem System: das kann nur BitsOS selbst ändern.";
pub const CORE: &str = "Ohne ihn läuft der Desktop nicht: er lässt sich hier nicht beenden.";
pub const TEST_DATA: &str = "Das sind Testdaten: an echten Prozessen ändert diese Ansicht nichts.";
pub const REAL_INPUT: &str = "Nur mit deiner echten Maus oder Tastatur: das kann niemand für dich bestätigen.";

/// May the desktop (uid `own_uid`) do `a` to row `r`, and must it ask?
pub fn gate(r: &Row, a: Act, own_uid: u32, real_root: bool) -> Gate {
    if !real_root {
        return Gate::No(TEST_DATA.into());
    }
    if r.uid != Some(own_uid) {
        return Gate::No(NOT_OURS.into());
    }
    match a {
        Act::End | Act::Kill if r.core => Gate::No(CORE.into()),
        _ if r.service => Gate::Ask,
        Act::Prio(Vorrang::Hoch) => Gate::Ask,
        _ => Gate::Free,
    }
}

/// Whether an answer may carry out `g`: an asked step only by Georg's own input.
pub fn may(g: &Gate, real: bool) -> bool {
    match g {
        Gate::Free => true,
        Gate::Ask => real,
        Gate::No(_) => false,
    }
}

/// The habit event goes out only for Georg's own change of his own program (K2: never for services).
pub fn learns(r: &Row, real: bool) -> bool {
    real && !r.service && !r.core
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys_group::Part;

    fn row(uid: u32, service: bool, core: bool) -> Row {
        Row { part: Some(if service { Part::System } else { Part::Window }), uid: Some(uid), service, core, pids: vec![42], ..Row::default() }
    }

    #[test]
    fn own_programs_are_free_hoch_and_services_ask_root_is_refused() {
        let own = row(1000, false, false);
        assert_eq!(gate(&own, Act::End, 1000, true), Gate::Free);
        assert_eq!(gate(&own, Act::Kill, 1000, true), Gate::Free);
        assert_eq!(gate(&own, Act::Prio(Vorrang::Niedrig), 1000, true), Gate::Free);
        assert_eq!(gate(&own, Act::Prio(Vorrang::Normal), 1000, true), Gate::Free);
        assert_eq!(gate(&own, Act::Prio(Vorrang::Hoch), 1000, true), Gate::Ask);
        let svc = row(1000, true, false);
        for a in [Act::End, Act::Kill, Act::Prio(Vorrang::Niedrig), Act::Prio(Vorrang::Normal), Act::Prio(Vorrang::Hoch)] {
            assert_eq!(gate(&svc, a, 1000, true), Gate::Ask, "{a:?}");
        }
        let desk = row(1000, true, true);
        assert_eq!(gate(&desk, Act::End, 1000, true), Gate::No(CORE.into()));
        assert_eq!(gate(&desk, Act::Prio(Vorrang::Niedrig), 1000, true), Gate::Ask);
        let root = row(0, true, false);
        for a in [Act::End, Act::Prio(Vorrang::Niedrig)] {
            assert_eq!(gate(&root, a, 1000, true), Gate::No(NOT_OURS.into()));
        }
        assert_eq!(gate(&own, Act::End, 1000, false), Gate::No(TEST_DATA.into()), "a fixture's pids are not this machine's");
    }

    #[test]
    fn ctl_and_mcp_cannot_confirm_an_asked_step() {
        assert!(may(&Gate::Free, false), "free steps work for injected input too");
        assert!(!may(&Gate::Ask, false), "an injected yes (ctl, MCP, ui-act) does not count");
        assert!(may(&Gate::Ask, true));
        assert!(!may(&Gate::No("x".into()), true));
        assert!(learns(&row(1000, false, false), true));
        assert!(!learns(&row(1000, false, false), false), "only Georg's own change teaches the habit");
        assert!(!learns(&row(1000, true, false), true), "never for BitsOS's services");
    }

    #[test]
    fn vorrang_nice_values() {
        assert_eq!(Vorrang::ALL.map(Vorrang::nice), [10, 0, -5]);
        assert_eq!([7, 0, -3].map(Vorrang::of_nice), [Vorrang::Niedrig, Vorrang::Normal, Vorrang::Hoch]);
        assert_eq!(Vorrang::of_word("hoch"), Some(Vorrang::Hoch));
        assert_eq!(Vorrang::of_word("x"), None);
    }
}
