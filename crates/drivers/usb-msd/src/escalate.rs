//! US-R: what a stick that stopped answering gets, in order: Bulk-Only reset, port reset with the device set up again
//! in place, the same after a pause, the same with the port's power off and on, and only then giving up (the device
//! detached and its port enumerated afresh, a new disk). Each rung is taken once until a command passes again.
//!
//! Linux has the same pieces in another order (linux-6.6/drivers/usb/storage/transport.c:904-925: a transport error
//! costs a port reset at once, a class reset only if that fails; the SCSI error handler above asks TEST UNIT READY):
//! here the cheap reset comes first, and `Bot::answers` asks after each rung. A failed command with a good CSW (a
//! medium error) never comes here: the transport works. The first command that reached the medium and passed starts
//! over (a TEST UNIT READY or a READ CAPACITY a firmware answers from memory proves little: a stick answering those but
//! hanging on every read climbs on, one rung per failed attempt).
//!
//! The bound (`GATE_TIMEOUT_NS` and the block layer's timeout are 120 s, others wait behind this device's gate): a
//! rung waits at most `wait_ns`, and past `LADDER_MAIN_NS` only the power rung is left. In the worst case that is the
//! stage that ran out (30 s data + 2 s abort), Bulk-Only reset (5 s), two port resets with a look (13 + 7 s, 1 + 13 + 7 s),
//! the power rung with a look (4 + 15 + 12 s): 109 s. `tests::a_wedged_stick_holds_its_gate_under_the_timeout`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    BotReset,
    PortReset { pause_ns: u64, power: bool, wait_ns: u64 },
    GiveUp,
}

const PAUSE_SHORT_NS: u64 = 1_000_000_000;
const PAUSE_LONG_NS: u64 = 4_000_000_000;
/// Past this since the first rung the rungs without power are left out.
pub const LADDER_MAIN_NS: u64 = 30_000_000_000;
/// A port reset's wait: `usb_core::reset::RESET_WAIT_NS`, and with the power's off and on on top.
const WAIT_NS: u64 = usb_core::reset::RESET_WAIT_NS;
const WAIT_POWER_NS: u64 = WAIT_NS + 2_000_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Escalation {
    rung: u8,
    gave_up: bool,
    /// The clock at the first rung since a command passed or the device answered.
    started: Option<u64>,
    /// The device answered after the last rung (`Bot::answers`).
    answering: bool,
}

impl Escalation {
    /// The next thing to try; it counts as tried.
    pub fn next(&mut self, now_ns: u64) -> Step {
        // The time bound is for a device that does not answer; one that did just spent its command's own time.
        let started = if self.answering { now_ns } else { self.started.unwrap_or(now_ns) };
        self.started = Some(started);
        self.answering = false;
        // The power rung clears what nothing else clears: it comes before giving up, however late it has become.
        if matches!(self.rung, 1 | 2) && now_ns.saturating_sub(started) >= LADDER_MAIN_NS {
            self.rung = 3;
        }
        let s = match self.rung {
            0 => Step::BotReset,
            1 => Step::PortReset { pause_ns: 0, power: false, wait_ns: WAIT_NS },
            2 => Step::PortReset { pause_ns: PAUSE_SHORT_NS, power: false, wait_ns: WAIT_NS },
            3 => Step::PortReset { pause_ns: PAUSE_LONG_NS, power: true, wait_ns: WAIT_POWER_NS },
            _ => Step::GiveUp,
        };
        self.rung = self.rung.saturating_add(1);
        s
    }
    /// A command that reached the medium passed: the device is well, the next trouble starts at the bottom.
    pub fn clean(&mut self) {
        *self = Escalation::default();
    }
    /// The device answered after the last rung: it stays on that rung, and its command's own attempts decide.
    pub fn answered(&mut self) {
        self.answering = true;
    }
    pub fn answering(&self) -> bool {
        self.answering
    }
    /// Port resets cannot help on this device: next comes `GiveUp`.
    pub fn no_port_reset(&mut self) {
        self.rung = self.rung.max(4);
    }
    /// The device will go; every command until then ends at once.
    pub fn gave_up(&mut self) {
        self.gave_up = true;
    }
    pub fn has_given_up(&self) -> bool {
        self.gave_up
    }
    /// Something was tried since the last command that passed.
    pub fn dirty(&self) -> bool {
        self.rung > 0 || self.gave_up
    }
    /// Something was tried since the last command that passed, nothing has answered, and there is more to try: a
    /// command that failed now is worth another attempt (the bound is the ladder).
    pub fn climbing(&self) -> bool {
        self.rung > 0 && self.rung <= 4 && !self.gave_up && !self.answering
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 1_000_000_000;

    #[test]
    fn the_ladder_goes_up_in_order_and_starts_over_after_a_passed_command() {
        let mut e = Escalation::default();
        assert!(!e.climbing() && !e.dirty());
        assert_eq!(e.next(T), Step::BotReset);
        assert!(e.climbing());
        assert_eq!(e.next(T), Step::PortReset { pause_ns: 0, power: false, wait_ns: WAIT_NS });
        assert_eq!(e.next(T), Step::PortReset { pause_ns: PAUSE_SHORT_NS, power: false, wait_ns: WAIT_NS });
        assert_eq!(e.next(T), Step::PortReset { pause_ns: PAUSE_LONG_NS, power: true, wait_ns: WAIT_POWER_NS });
        assert!(e.climbing(), "one more thing to do: giving up");
        assert_eq!(e.next(T), Step::GiveUp);
        assert!(!e.climbing());
        assert_eq!(e.next(T), Step::GiveUp, "and it stays there");
        e.clean();
        assert!(!e.dirty());
        assert_eq!(e.next(T), Step::BotReset);
    }

    #[test]
    fn a_device_without_a_port_reset_gives_up_after_its_bulk_only_reset() {
        let mut e = Escalation::default();
        assert_eq!(e.next(T), Step::BotReset);
        e.no_port_reset();
        assert_eq!(e.next(T), Step::GiveUp);
        e.gave_up();
        assert!(e.has_given_up() && !e.climbing());
    }

    #[test]
    fn past_its_time_the_ladder_goes_straight_to_the_power_rung_and_not_past_it() {
        let mut e = Escalation::default();
        assert_eq!(e.next(T), Step::BotReset);
        let late = T + LADDER_MAIN_NS;
        assert_eq!(e.next(late), Step::PortReset { pause_ns: PAUSE_LONG_NS, power: true, wait_ns: WAIT_POWER_NS });
        assert_eq!(e.next(late), Step::GiveUp);
    }

    #[test]
    fn a_device_that_answers_stops_the_climb_but_keeps_its_rung() {
        let mut e = Escalation::default();
        assert_eq!(e.next(T), Step::BotReset);
        e.answered();
        assert!(e.answering() && !e.climbing() && e.dirty(), "this command's attempts decide now");
        assert_eq!(e.next(T), Step::PortReset { pause_ns: 0, power: false, wait_ns: WAIT_NS });
        assert!(!e.answering() && e.climbing(), "it failed again: the next rung has to prove it again");
        // Long after the first rung: the time since it answered counts, not its command's time.
        e.answered();
        assert_eq!(e.next(T + 2 * LADDER_MAIN_NS), Step::PortReset { pause_ns: PAUSE_SHORT_NS, power: false, wait_ns: WAIT_NS });
    }
}
