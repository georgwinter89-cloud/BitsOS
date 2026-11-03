//! The permission gate for actions that change something (`Safety::Change`, `Safety::DeleteOverwrite`): each one
//! has a settings key `claude.actions.<tool>` (choice `deny`/`allow`, default `deny`, safety `confirm`) that the
//! service registers with settingsd (`/run/bitsos/settings.sock`). A call runs only while its key is `allow`;
//! settingsd unreachable, the key missing or any other value refuse it (fail closed). Georg allows it on the
//! settings page or in the shell (`settings set claude.actions.brave_download allow --confirm`). An `allow` that
//! came over MCP does not count: settingsd's own MCP tools write with the source `mcp` (a source the caller
//! cannot choose), and the gate reads the key's newest journal entry (`history`) at every call: an MCP client
//! cannot grant itself the action with `settings_set {confirmed: true}`. An `allow` without a journal entry
//! (trimmed away) is refused too: Georg sets it again. Limits: a client that may run programs (the kernel's `run`
//! tool) can run the `settings` command itself, so against such a client the gate is advice, not a wall; the wall
//! is the kernel's per-client policy (a change request to MC/PM). The value is read at every call, so a change
//! applies at once. PM-3 (permd) replaces this by its own rules (`perm.<program>.*`, with `ask`).

use crate::events::{Events, Level, q};
use mcpreg::Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

pub const SETTINGS_SOCKET: &str = "/run/bitsos/settings.sock";
const PREFIX: &str = "claude.actions.";
/// settingsd answers within its own deadline (5 s by default); a hung one must not hold a call longer.
const TALK: Duration = Duration::from_secs(5);
const MAX_ANSWER: u64 = 1 << 20;
/// Journal sources that are not Georg: settingsd's MCP tools (fixed `mcp`) and names Claude-side tools use.
const NOT_GEORG: &[&str] = &["mcp", "claude"];

/// The settings key of a tool: settings names are lower case `[a-z0-9_-]`, a tool name may also have upper case
/// and dots.
pub fn key_for(tool: &str) -> String {
    let t: String = tool.chars().map(|c| if c == '.' { '_' } else { c.to_ascii_lowercase() }).collect();
    format!("{PREFIX}{t}")
}

pub struct Gate {
    pub tool: String,
    pub de: String,
    pub en: String,
}

pub struct Permissions {
    owner: String,
    socket: String,
    gates: Vec<Gate>,
    ev: Events,
}

impl Permissions {
    pub fn new(owner: &str, socket: &str, gates: Vec<Gate>, ev: Events) -> Permissions {
        Permissions { owner: owner.to_string(), socket: socket.to_string(), gates, ev }
    }

    /// Registers every gate's key (settingsd keeps a value set earlier). Err: settingsd unreachable or refusing.
    pub fn register(&self) -> Result<(), String> {
        if self.gates.is_empty() {
            return Ok(());
        }
        let keys = self.gates.iter().map(|g| {
            Json::obj(vec![
                ("key", Json::Str(key_for(&g.tool))),
                ("schema", Json::obj(vec![("type", Json::str("choice")), ("choices", Json::Arr(vec![Json::str("deny"), Json::str("allow")]))])),
                ("default", Json::str("deny")),
                ("text", Json::obj(vec![("de", Json::str(&g.de)), ("en", Json::str(&g.en))])),
                ("restart", Json::str("none")),
                ("safety", Json::str("confirm")),
            ])
        });
        let req = Json::obj(vec![("op", Json::str("register")), ("owner", Json::str(&self.owner)), ("keys", Json::Arr(keys.collect()))]);
        let a = self.talk(&req)?;
        if a.get("ok").and_then(Json::as_bool) == Some(true) {
            self.ev.emit(Level::Info, "perm.registered", &format!("keys={} socket={}", self.gates.len(), q(&self.socket)));
            Ok(())
        } else {
            Err(format!("settingsd refused the keys: {}", answer_error(&a)))
        }
    }

    /// Ok when the tool's key is `allow`; Err is the refusal the caller sees.
    pub fn check(&self, tool: &str) -> Result<(), String> {
        let key = key_for(tool);
        let v = match self.value(&key) {
            Ok(Some(v)) => Ok(v),
            Ok(None) => self.register().and_then(|_| self.value(&key)).and_then(|v| v.ok_or_else(|| "the key is unknown to settingsd".into())),
            Err(e) => Err(e),
        };
        let v = match v {
            Ok(v) if v == "allow" => self.source(&key).map(|src| (v, src)),
            Ok(v) => Ok((v, String::new())),
            Err(e) => Err(e),
        };
        match v {
            Ok((v, src)) if v == "allow" && NOT_GEORG.contains(&src.as_str()) => {
                self.ev.emit(Level::Warn, "perm.self_granted", &format!("tool={} key={key} source={}", q(tool), q(&src)));
                Err(format!(
                    "{tool} is refused: {key} was set to allow over MCP (source {src:?}), not by Georg. A client cannot \
allow itself a change action. Georg allows it on the settings page or in the shell; as the value is allow already, \
he sets it to deny and then to allow again (settingsd journals only changes). / {tool} abgelehnt: {key} wurde über MCP \
erlaubt, nicht von Georg: einmal auf deny und wieder auf allow stellen."
                ))
            }
            Ok((v, src)) if v == "allow" && src.is_empty() => {
                self.ev.emit(Level::Warn, "perm.no_journal", &format!("tool={} key={key}", q(tool)));
                Err(format!(
                    "{tool} is refused: {key} is allow, but settingsd's journal no longer says who set it. Georg sets it \
again (settings page, or `settings set {key} allow --confirm` in the shell)."
                ))
            }
            Ok((v, src)) if v == "allow" => {
                self.ev.emit(Level::Info, "perm.allowed", &format!("tool={} key={key} source={}", q(tool), q(&src)));
                Ok(())
            }
            Ok((v, _)) => {
                self.ev.emit(Level::Info, "perm.refused", &format!("tool={} key={key} value={}", q(tool), q(&v)));
                Err(format!(
                    "{tool} changes something and is not permitted: the setting {key} is {v:?}. Ask Georg; when he \
agrees, he sets it himself on the settings page or in the shell (`settings set {key} allow --confirm`); an allow \
set over MCP (settings_set) does not count. / {tool} ist nicht erlaubt ({key} = {v}); Georg kann es erlauben."
                ))
            }
            Err(e) => {
                self.ev.emit(Level::Warn, "perm.unknown", &format!("tool={} key={key} why={}", q(tool), q(&e)));
                Err(format!("{tool} changes something and needs the setting {key} = allow, which cannot be read now ({e}); it is refused."))
            }
        }
    }

    /// Who made the key's newest change (the journal's `source`); empty when the journal has none for it.
    fn source(&self, key: &str) -> Result<String, String> {
        let a = self.talk(&Json::obj(vec![("op", Json::str("history")), ("n", Json::Num(1.0)), ("prefix", Json::str(key))]))?;
        if a.get("ok").and_then(Json::as_bool) != Some(true) {
            return Err(format!("settingsd's journal: {}", answer_error(&a)));
        }
        let newest = a.get("entries").and_then(Json::as_array).and_then(|e| e.iter().find(|e| e.get("key").and_then(Json::as_str) == Some(key)));
        Ok(newest.and_then(|e| e.get("source")).and_then(Json::as_str).unwrap_or("").to_string())
    }

    /// Ok(None): settingsd does not know the key.
    fn value(&self, key: &str) -> Result<Option<String>, String> {
        let a = self.talk(&Json::obj(vec![("op", Json::str("get")), ("key", Json::str(key))]))?;
        if a.get("ok").and_then(Json::as_bool) == Some(true) {
            return Ok(Some(match a.get("value") {
                Some(Json::Str(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            }));
        }
        if a.get("error").and_then(Json::as_str) == Some("unknown_key") {
            return Ok(None);
        }
        Err(format!("settingsd: {}", answer_error(&a)))
    }

    fn talk(&self, req: &Json) -> Result<Json, String> {
        let s = UnixStream::connect(&self.socket).map_err(|e| format!("settingsd at {}: {e}", self.socket))?;
        s.set_read_timeout(Some(TALK)).and_then(|_| s.set_write_timeout(Some(TALK))).map_err(|e| e.to_string())?;
        let mut w = s.try_clone().map_err(|e| e.to_string())?;
        w.write_all(format!("{req}\n").as_bytes()).map_err(|e| format!("settingsd: {e}"))?;
        let mut line = Vec::new();
        BufReader::new(s).take(MAX_ANSWER).read_until(b'\n', &mut line).map_err(|e| format!("settingsd: {e}"))?;
        Json::parse(String::from_utf8_lossy(&line).trim()).map_err(|e| format!("settingsd answered no JSON: {e}"))
    }
}

fn answer_error(a: &Json) -> String {
    let code = a.get("error").and_then(Json::as_str).unwrap_or("?");
    let msg = a.get("message").and_then(Json::as_str).unwrap_or("");
    format!("{code} {msg}").trim().to_string()
}
