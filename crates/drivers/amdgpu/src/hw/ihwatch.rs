//! GX-7n: the job thread's trust in interrupts. A fence that moved with no EOP or trap behind it is a suspect, not yet a
//! lost interrupt: the entry follows the fence write by little, so a look `SETTLE_NS` later decides, however many rounds
//! a kick brings before that. `LOST_LIMIT` lost ones in a row and the thread goes back to the poll it always had (one
//! event says so, a counter counts); `BACK_AFTER` fences in a row with their interrupt and it trusts them again. A job
//! never waits on an interrupt alone: the look at `irq_check_ns` is always there. Pure, the clock comes as an argument.
//!
//! The count of events is read before the round looks at the fences. The chip writes a fence before it raises the entry,
//! so an event counted before that read has its fence in this round's look or an earlier one; an event counted later
//! belongs to the next round, where its fence shows at the latest. Read after the round, an event of a fence the round's
//! look just missed would be used up here and that fence would read as lost in the next one.

/// Lost ones in a row before polling.
pub const LOST_LIMIT: u32 = 3;
/// Fences with their interrupt in a row before trusting again.
pub const BACK_AFTER: u32 = 8;
/// How long an entry may follow its fence before it counts as lost: the IH thread's wake on a busy machine.
pub const SETTLE_NS: u64 = 2_000_000;

/// How the thread waits after this round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// For an interrupt, with the long look behind it.
    Interrupts,
    /// Soon: a suspect is open.
    Settle,
    /// GX-8b's poll.
    Poll,
}

/// What the watch saw that the log and the counters must hear of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Told {
    pub lost: bool,
    pub fell_back: bool,
    pub came_back: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IrqWatch {
    polling: bool,
    /// Since when a fence is waiting for its entry.
    suspect: Option<u64>,
    lost_row: u32,
    good_row: u32,
    fences: u64,
    events: u64,
}

impl IrqWatch {
    pub fn new(fences: u64, events: u64) -> IrqWatch {
        IrqWatch { fences, events, ..IrqWatch::default() }
    }

    pub fn polling(&self) -> bool {
        self.polling
    }

    /// After a round: `fences`: how often a fence moved so far; `events`: fence interrupts so far, read before the round
    /// looked at the fences; `now`: the clock.
    pub fn after_round(&mut self, fences: u64, events: u64, now: u64) -> (Wait, Told) {
        let mut told = Told::default();
        let fence_moved = fences != self.fences;
        let event_moved = events != self.events;
        (self.fences, self.events) = (fences, events);
        if let Some(since) = self.suspect {
            if event_moved {
                // Late, not lost.
                self.suspect = None;
                self.lost_row = 0;
            } else if now.saturating_sub(since) >= SETTLE_NS {
                self.suspect = None;
                told.lost = true;
                self.good_row = 0;
                self.lost_row = self.lost_row.saturating_add(1);
                if !self.polling && self.lost_row >= LOST_LIMIT {
                    self.polling = true;
                    told.fell_back = true;
                }
            }
        }
        if fence_moved {
            if event_moved {
                self.lost_row = 0;
                self.good_row = self.good_row.saturating_add(1);
                if self.polling && self.good_row >= BACK_AFTER {
                    self.polling = false;
                    told.came_back = true;
                }
            } else {
                self.suspect = self.suspect.or(Some(now));
                self.good_row = 0;
            }
        }
        let wait = if self.suspect.is_some() {
            Wait::Settle
        } else if self.polling {
            Wait::Poll
        } else {
            Wait::Interrupts
        };
        (wait, told)
    }
}
