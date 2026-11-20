//! `status`: what the sidebar shows before anything is asked. Who would answer, where that is, and when
//! nobody can, the one sentence that says why. Read anew every time, as the engine reads the settings anew
//! for every question; a key is never fetched for it, only whether one is set.

use super::Ctx;
use super::proto;
use crate::config::{self, Config};
use crate::json::Json;
use crate::provider::{Kind, Place, Provider};
use crate::settings::Settings;
use crate::tools::permit::Level;

const UNREACHABLE: &str = "Die Einstellungen sind gerade nicht erreichbar.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub name: String,
    /// The provider that answers; when none can, the one the chat names.
    pub provider: Option<String>,
    pub place: Option<Place>,
    pub local_only: bool,
    /// `ai.level`.
    pub level: Level,
    /// None: a question can be asked. Some: why not (German, one sentence or two).
    pub why: Option<String>,
}

impl Look {
    pub fn ready(&self) -> bool {
        self.why.is_none()
    }
    pub fn place_name(&self) -> Option<&'static str> {
        self.place.map(|p| if p == Place::Cloud { "cloud" } else { "local" })
    }
}

/// Is the key of `p` there, as far as that can be said without reading it?
fn key_set(s: &dyn Settings, p: &Provider) -> Result<(), String> {
    if p.kind == Kind::Program || p.place == Place::Local {
        return Ok(());
    }
    let answer = s.request(&Json::obj().with("op", "get").with("key", format!("ai.provider.{}.key", p.id))).map_err(|_| UNREACHABLE.to_string())?;
    if answer.get("changed").and_then(Json::as_bool) == Some(true) { Ok(()) } else { Err(config::key_missing(&p.id)) }
}

/// The first provider the engine would get an answer from, by the same rules in the same order
/// (`engine::Session::send`), without contacting anybody.
fn pick(s: &dyn Settings, cfg: &Config) -> (Option<String>, Option<Place>, Option<String>) {
    if cfg.ids.is_empty() {
        return (None, None, Some("Es ist noch kein KI-Anbieter eingerichtet.".into()));
    }
    let Ok(ids) = cfg.candidates() else {
        return (None, None, Some("Es ist noch kein Anbieter für das Gespräch gewählt.".into()));
    };
    let noted = |id: &str| match cfg.entries.get(id).map(|e| e.place.as_str()) {
        Some("cloud") => Some(Place::Cloud),
        Some("local") => Some(Place::Local),
        _ => None,
    };
    let mut first: Option<String> = None;
    for id in &ids {
        match cfg.provider(id) {
            // The engine gives up here as well: a fallback that is half set up is not skipped silently.
            Err(why) => {
                first.get_or_insert(why);
                break;
            }
            Ok(p) => match cfg.admit(&p).and_then(|()| key_set(s, &p)) {
                Ok(()) => return (Some(p.id), Some(p.place), None),
                Err(why) => {
                    first.get_or_insert(why);
                }
            },
        }
    }
    (Some(ids[0].clone()), noted(&ids[0]), first)
}

pub fn look(s: &dyn Settings) -> Look {
    match config::load(s) {
        Err(_) => Look { name: "Assistent".into(), provider: None, place: None, local_only: false, level: Level::Ask, why: Some(UNREACHABLE.into()) },
        Ok(cfg) => {
            let (provider, place, why) = pick(s, &cfg);
            Look { name: cfg.name, provider, place, local_only: cfg.local_only, level: cfg.level, why }
        }
    }
}

pub fn reply(ctx: &Ctx) -> Json {
    let l = look(&*ctx.parts.settings);
    let (running, chats) = ctx.state.view();
    let turns = ctx.state.default_chat().and_then(|c| ctx.store.load(&c).ok()).map_or(0, |l| super::history::plain(&l).len());
    proto::ok("status")
        .with("name", l.name.as_str())
        .with("provider", l.provider.clone())
        .with("place", l.place_name())
        .with("local_only", l.local_only)
        .with("ready", l.ready())
        .with("why", l.why.clone())
        // The sidebar asks again by itself while this holds (assist may start before settingsd).
        .with("settings_down", l.why.as_deref() == Some(UNREACHABLE))
        .with("level", l.level.name())
        .with("running", running)
        .with("chats", chats)
        // Of before K1: whether anything works, and the turns kept in the sidebar's chat.
        .with("busy", running > 0)
        .with("turns", turns)
}
