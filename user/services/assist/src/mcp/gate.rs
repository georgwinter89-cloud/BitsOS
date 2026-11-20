//! What a program at `assist --mcp` gets of the chat's tools, under the one level (`ai.level`, plan/spec/
//! assistent-eins K2). When this server runs under a helper program of a running assistant (`--bell`,
//! [`super::bell`]), what would change something is asked through that assistant's bell, whose judge applies the
//! chat's level (Automodus runs it, Fragt vorher asks Georg on the chat's view). Otherwise nobody sits at this
//! bell to be asked, the program is a remote caller (K3: never more than `ask`), and the question becomes a rule:
//! - at `answer` nothing that changes is listed;
//! - what reads runs;
//! - what changes runs with `ai.mcp.ask_tools` (on by default, DECISIONS 18.11.: the client then asks its own
//!   user); while Georg has it off, it answers how to allow it again;
//! - a change of `ai.*`, the safety switches, guarded keys and kernel secrets: never, whatever is set and
//!   whoever could be asked; `loeschen`, a settings key that asks first and an undo without the journal's
//!   number are asked at every level, also through the bell (F3, F4: `Verdict::must`);
//! - `--read-only`: only what reads. No setting and no bell widens that.
//!
//! The settings are read anew for every list and every call, as the chat does for every question.

use crate::config::{self, Config};
use crate::json::Json;
use crate::provider::program::ask::Reply;
use crate::settings::Settings;
use crate::tools::permit::{self, By, Decision, Level};
use crate::tools::{Class, Grant, ToolSource, ToolSpec, schema};

/// A call that did not run, in the two ways MCP knows.
pub enum Deny {
    /// No such tool for this client (JSON-RPC error -32602).
    Unknown,
    /// The tool's own answer, marked as an error (words for the model and its user).
    Told(String),
}

/// Who can ask Georg for this server: the assistant it runs under.
pub trait Owner {
    /// Puts the question of the tool `tool` to the assistant's bell and waits for the answer. `must_ask`: asked
    /// whatever the level (F3, F4); otherwise the bell's judge decides by the chat's level.
    fn ask(&self, tool: &str, question: &str, must_ask: bool) -> Reply;
}

pub struct Gate<'a> {
    pub settings: &'a dyn Settings,
    pub tools: &'a dyn ToolSource,
    pub read_only: bool,
    pub owner: Option<&'a dyn Owner>,
}

fn unreachable(e: &crate::settings::Refused) -> String {
    format!("Die Einstellungen sind nicht erreichbar ({}). Läuft settingsd?", e.code)
}

fn how_to_allow(tool: &str) -> String {
    format!("Das Werkzeug „{tool}“ ändert etwas und läuft über „assist --mcp“ erst wieder, wenn du es erlaubst: settings --confirm set ai.mcp.ask_tools true als root an der Konsole")
}

const OWN_LEASH: &str = "Die KI-Einstellungen (alles unter „ai.“: Anbieter, Schlüssel, Erlaubnisse) lassen sich über „assist --mcp“ nie ändern. \
Das geht nur im Gespräch mit dem Assistenten oder mit dem Befehl „settings“.";

const OWN_LEASH_UNDO: &str = "Die letzte Änderung lässt sich über „assist --mcp“ nicht zurücknehmen: sie betrifft die KI-Einstellungen (unter „ai.“), die sich über diesen Weg nie ändern lassen, oder sie war gerade nicht zu lesen. Das geht nur im Gespräch mit dem Assistenten oder mit dem Befehl „settings“.";

impl Gate<'_> {
    /// The level this server works under: the chat's beside an assistant's bell, else a remote caller's.
    fn level(&self, cfg: &Config) -> Level {
        if self.owner.is_some() { cfg.level } else { cfg.level.for_origin(By::Remote) }
    }

    fn offered(&self, cfg: &Config, t: &ToolSpec) -> bool {
        permit::offered(self.level(cfg), t) && (!self.read_only || t.class == Class::Read)
    }

    /// The `tools` of a `tools/list` answer.
    pub fn list(&self) -> Result<Vec<Json>, String> {
        let all = self.tools.list();
        let cfg = config::load(self.settings).map_err(|e| unreachable(&e))?;
        Ok(all
            .iter()
            .filter(|t| self.offered(&cfg, t))
            .map(|t| {
                // "Reads only" is said of a tool that runs unasked in the chat, nothing else: a client
                // that asks its user before a changing tool then asks for every tool the chat would ask for.
                let reads = t.class == Class::Read;
                let notes = Json::obj().with("readOnlyHint", reads).with("destructiveHint", t.class == Class::Other).with("openWorldHint", false);
                Json::obj().with("name", t.name.as_str()).with("description", t.description.as_str()).with("inputSchema", t.schema.clone()).with("annotations", notes)
            })
            .collect())
    }

    /// One call through schema and rules. `stop` is asked while a tool waits for someone else.
    pub fn call(&self, name: &str, args: &Json, stop: &dyn Fn() -> bool) -> Result<String, Deny> {
        let all = self.tools.list();
        let cfg = config::load(self.settings).map_err(|e| Deny::Told(unreachable(&e)))?;
        let Some(spec) = all.iter().find(|t| t.name == name).filter(|t| self.offered(&cfg, t)) else { return Err(Deny::Unknown) };
        schema::check(&spec.schema, args).map_err(|e| Deny::Told(format!("invalid arguments: {e}")))?;
        let settled = self.tools.settle(name, args).map_err(Deny::Told)?;
        let args = &settled;
        // Beside a bell the bell's judge applies the level: here every change is a question for it.
        let level = if self.owner.is_some() { Level::Ask } else { self.level(&cfg) };
        let verdict = permit::judge(level, spec, args, self.settings);
        // What the chat would ask Georg: asked through the assistant above when there is one, else answered
        // by rule.
        let asked = |question: &str, must: bool| -> Result<(), Deny> {
            if verdict.leash {
                return Err(Deny::Told(if name == "settings_undo" { OWN_LEASH_UNDO } else { OWN_LEASH }.into()));
            }
            if self.read_only {
                return Err(Deny::Told(format!("Das Werkzeug „{name}“ ändert etwas; hier wird nur gelesen (assist --mcp --read-only).")));
            }
            if let Some(owner) = self.owner {
                return match owner.ask(name, question, must) {
                    Reply::Yes => Ok(()),
                    Reply::No(why) => Err(Deny::Told(why.sentence().into())),
                };
            }
            if !cfg.mcp_ask_tools {
                return Err(Deny::Told(how_to_allow(name)));
            }
            Ok(())
        };
        let run = |confirmed: bool| self.tools.call_with(name, args, Grant { confirmed, pin: verdict.pin, level: self.level(&cfg) }, stop);
        let answer = match &verdict.decision {
            Decision::Refuse(why) => return Err(Deny::Told(why.clone())),
            Decision::Ask(question) => {
                asked(question, verdict.must)?;
                run(true)
            }
            Decision::Run => match run(false) {
                // A key of settingsd's class "confirm" behind a tool that runs unasked: asked at every level (F3).
                Err(e) if e.starts_with("needs_confirm") => {
                    asked(&verdict.what, true)?;
                    run(true)
                }
                other => other,
            },
        };
        answer.map_err(Deny::Told)
    }
}
