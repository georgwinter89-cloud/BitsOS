//! Which tool calls run at once, which wait for Georg's yes, which never run (plan/spec/assistent-eins K2). One
//! level for every tool and every chat (`ai.level`: answer | ask | auto): a reading tool runs at every level, a
//! tool that changes something is not offered at `answer`, asks at `ask` and runs at `auto`. On top of it sit the
//! fixed rules no level and no model output can lift:
//! - F3: the R8 switches, the guarded CONFIG keys and the kernel's secrets are never touched (settingsd refuses
//!   them too; this saves Georg a pointless question), and a change to `ai.*` (providers, keys, "Nur lokal",
//!   this very level) always asks: text that reached the model through a tool result or a page must not be able
//!   to widen what the model may do;
//! - F4: `loeschen` asks at every level that offers it;
//! - the `confirmed` flag of a settings change is set from Georg's answer, never from the model's arguments
//!   (tools/settings.rs).
//!
//! A question that came from the control socket or MCP (`by:"remote"`), or does not say, never gets more than `ask`
//! ([`Level::for_origin`]).

use super::describe::{UNDO_UNSEEN, describe, describe_undo, plain, undo_count, undo_targets};
use super::{Class, ToolSpec};
use crate::json::Json;
use crate::settings::Settings;
use settingsd::guard;

/// In order of freedom: the greater of two levels allows more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Answer,
    Ask,
    Auto,
}

impl Level {
    pub const KEY: &'static str = settingsd::leash::LEVEL;
    pub const NAMES: [&'static str; 3] = ["answer", "ask", "auto"];

    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "answer" => Level::Answer,
            "ask" => Level::Ask,
            "auto" => Level::Auto,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        Level::NAMES[self as usize]
    }
    /// Georg's word for it, as the pill shows it.
    pub fn german(self) -> &'static str {
        ["Nur antworten", "Fragt vorher", "Automodus"][self as usize]
    }
    /// The level a question works under: one that did not come from Georg's hand gets at most `ask`.
    pub fn for_origin(self, by: By) -> Level {
        if by == By::Remote { self.min(Level::Ask) } else { self }
    }
}

/// Who asked the question (K1 `by`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum By {
    /// Georg's own hand at a view or at the console (the console and `--once` are his by nature).
    #[default]
    Hand,
    /// The control socket or MCP: never more than `ask`, and the view lets nobody say yes for it.
    Remote,
}

impl By {
    /// A question that does not say who asked counts as remote: a view that forgets `by` makes the assistant
    /// ask more, never do more.
    pub fn parse(s: Option<&str>) -> Option<By> {
        match s {
            Some("hand") => Some(By::Hand),
            None => Some(By::Remote),
            Some("remote") => Some(By::Remote),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        if self == By::Remote { "remote" } else { "hand" }
    }
}

/// F4: the one tool that asks at every level that offers it.
pub const DELETE_TOOL: &str = "loeschen";
/// The desktop's acting tool: the compositor judges each of its steps under the level (K3); the engine never asks.
pub const ACT_TOOL: &str = "handeln";

/// Is the tool offered at all under `level`? A tool that changes something is not, at `answer`.
pub fn offered(level: Level, spec: &ToolSpec) -> bool {
    spec.class == Class::Read || level != Level::Answer
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Run,
    /// Ask Georg this (German) and run only on yes.
    Ask(String),
    /// Do not run; the text is the answer the model gets.
    Refuse(String),
}

/// A name no tool call may set, whole or as the last part of a key.
fn locked(key: &str) -> bool {
    let last = key.rsplit('.').next().unwrap_or(key).trim().to_ascii_lowercase();
    guard::refuse_key(key).is_err()
        || [guard::R8_SWITCHES, guard::GUARDED, guard::KERNEL_SECRETS].iter().any(|l| l.contains(&last.as_str()))
}

/// The assistant's own leash: providers, keys, "Nur lokal" and the level.
pub(super) fn own_leash(key: &str) -> bool {
    key == "ai" || key.starts_with("ai.")
}

/// The outcome of the permission step for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub decision: Decision,
    /// What the call does (`describe`), from the same look at the settings the decision was made on.
    pub what: String,
    /// `settings_undo`: the journal number of the newest change the decision looked at. The call carries it,
    /// and settingsd undoes nothing when a newer change came in between.
    pub pin: Option<u64>,
    /// The call changes the assistant's own leash (`ai.*`), or what it would change could not be seen. From
    /// the same look as the decision: a way in without Georg at the keyboard (`assist --mcp`) never runs it.
    pub leash: bool,
    /// The question comes from a fixed rule (F3, F4, an undo without a journal number), not from the level: it is
    /// asked at every level, also by a bell whose judge applies the chat's level.
    pub must: bool,
}

pub fn decide(level: Level, spec: &ToolSpec, args: &Json, settings: &dyn Settings) -> Decision {
    judge(level, spec, args, settings).decision
}

/// What the model reads for a changing tool at `answer`.
pub fn not_at_answer(name: &str) -> String {
    format!("not allowed: the level is \"Nur antworten\" (only answer) and {name} changes something. Tell Georg what you would do instead.")
}

pub fn judge(level: Level, spec: &ToolSpec, args: &Json, settings: &dyn Settings) -> Verdict {
    let refuse = |why: String| Verdict { decision: Decision::Refuse(why), what: String::new(), pin: None, leash: false, must: false };
    if !offered(level, spec) {
        return refuse(not_at_answer(&spec.name));
    }
    let mut ask = spec.class != Class::Read && level == Level::Ask;
    let mut pin = None;
    let mut leash = false;
    let mut must = false;
    let what = match spec.name.as_str() {
        "settings_set" => {
            let key = args.str("key").unwrap_or("");
            if locked(key) {
                return refuse(format!(
                    "refused_r8: {} is a safety switch, a guarded CONFIG key or a kernel secret. It is not a setting; only Georg changes it, by hand.",
                    plain(key)
                ));
            }
            leash = own_leash(key);
            ask |= leash;
            must |= leash;
            describe(spec, args, settings)
        }
        "settings_undo" => match undo_targets(settings, undo_count(args)) {
            Ok(t) if !t.is_empty() => {
                leash = t.iter().any(|t| own_leash(&t.key));
                ask |= leash;
                pin = t[0].seq;
                // Without the journal number nothing holds the call to what was looked at.
                must = leash || pin.is_none();
                ask |= must;
                describe_undo(&t)
            }
            // What it would take back is unknown (the journal was not readable, or is empty right now): it
            // could be Georg's own change to ai.*, so no level covers it.
            _ => {
                ask = true;
                leash = true;
                must = true;
                UNDO_UNSEEN.into()
            }
        },
        DELETE_TOOL => {
            ask = true;
            must = true;
            describe(spec, args, settings)
        }
        // The compositor shows, asks or refuses each step under the level itself (K3).
        ACT_TOOL => {
            ask = false;
            describe(spec, args, settings)
        }
        _ => describe(spec, args, settings),
    };
    let decision = if ask { Decision::Ask(what.clone()) } else { Decision::Run };
    Verdict { decision, what, pin, leash, must }
}
