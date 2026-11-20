//! What the comparison of a stage with Linux's trace found: the kind of a difference, and the difference with its
//! place in the fixture (`compare`).

use alloc::string::String;
use core::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Linux does something here that the stage leaves out.
    Missing,
    /// The stage does something here that Linux does not.
    Extra,
    /// Two accesses the other way round.
    Order,
    /// Another access altogether.
    Other,
    /// The same write with another value (and no rule for it, or a rule whose check fails), a sleep of another
    /// length.
    Value,
    /// A wait on the same register for another condition.
    Condition,
    /// The same wait that gives up at another point than Linux's.
    Bound,
    /// The card (in a test the fake one) answered a read or a wait differently from how the real card answered
    /// Linux.
    Answer,
    /// A write of the stage did not reach the card.
    Refused,
    /// A rule that is wrong in itself: unknown register, unused, half a pair.
    Rule,
    /// The trail is no proof: it lost accesses or holds something that is no register.
    Lost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub kind: Kind,
    pub section: String,
    /// The fixture's line and the line of init.txt it stands for.
    pub at: Option<(u32, u32)>,
    pub text: String,
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some((line, init)) => write!(f, "{}.txt line {line} (init.txt line {init}): {}", self.section, self.text),
            None => write!(f, "{}: {}", self.section, self.text),
        }
    }
}
