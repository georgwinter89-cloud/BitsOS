//! CL-3b: what the conversation's model is made of: the ring's phase, the list's items, the assistant's status
//! and what the sidebar can do right now.

use crate::desk::Ring;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// Asked, nothing of the answer yet.
    Thinking,
    Answering,
    /// A tool runs.
    Working,
    /// A tool asks Georg.
    Waiting,
}

impl Phase {
    /// The ring's look: the design has no sixth state, an arriving answer turns slowly as work does.
    pub fn ring(self) -> Ring {
        match self {
            Phase::Idle => Ring::Idle,
            Phase::Thinking => Ring::Thinking,
            Phase::Answering | Phase::Working => Ring::Working,
            Phase::Waiting => Ring::Waiting,
        }
    }

    /// The state line beside the ring.
    pub fn word(self) -> &'static str {
        match self {
            Phase::Answering => "ANTWORTET",
            p => p.ring().word(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Thinking => "thinking",
            Phase::Answering => "answering",
            Phase::Working => "working",
            Phase::Waiting => "waiting",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Georg,
    Assistant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolState {
    Asks,
    Runs,
    Done,
    Refused,
    /// The answer ended while the tool was open.
    Stopped,
}

impl ToolState {
    pub fn word(self) -> &'static str {
        match self {
            ToolState::Asks => "fragt dich",
            ToolState::Runs => "läuft",
            ToolState::Done => "fertig",
            ToolState::Refused => "nicht ausgeführt",
            ToolState::Stopped => "abgebrochen",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Turn { who: Who, text: String },
    /// A tool's German line and how far it is.
    Tool { name: String, text: String, state: ToolState },
    /// A line of the sidebar's own: an error, "abgebrochen".
    Note { text: String, error: bool },
}

impl Item {
    pub fn text(&self) -> &str {
        match self {
            Item::Turn { text, .. } | Item::Tool { text, .. } | Item::Note { text, .. } => text,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub provider: Option<String>,
    pub place: Option<String>,
    pub local_only: bool,
    pub ready: bool,
    pub why: String,
    pub busy: bool,
    pub turns: u64,
}

/// What the sidebar can do right now.
#[derive(Clone, Debug, PartialEq)]
pub enum Gate {
    Ready,
    /// No status yet.
    Starting,
    /// The assistant said why (no provider, no key, "Nur lokal" with a cloud provider).
    NotReady(String),
    /// No socket, or no reply within the deadline.
    Silent,
    /// Somebody answers on the socket who is not the assistant's helper (`assist_peer`): nothing is said to it.
    Stranger,
}
