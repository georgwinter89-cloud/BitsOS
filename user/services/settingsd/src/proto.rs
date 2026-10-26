//! The socket protocol (`/run/bitsos/settings.sock`, AF_UNIX stream): one JSON request per line, one JSON answer
//! per line. CL-2's MCP tools map onto these ops one to one.
//!
//! | op | fields | answer |
//! |---|---|---|
//! | `list` | `prefix` | `keys: [{key,type,value,changed,text}]` |
//! | `describe` | `key` | `key: {schema,default,value,text{de,en},applier,restart,safety,owner}` |
//! | `get` | `key`, `owner` (reveals a secret to its owner) | `value`, `changed` |
//! | `set` | `key`, `value`, `source`, `confirmed` | `old`, `new`, `applied`, `restart`, `seq`, `stored` |
//! | `reset` | `key`, `source` | as `set` |
//! | `undo` | `n` (default 1), `source`, `expect_seq` (with n 1: the change it may undo) | `undone: [...]`, `stopped` |
//! | `history` | `n` (default 5), `prefix` | `entries` newest first |
//! | `register` | `owner`, `keys: [{key,schema,default,text{de,en},restart,safety}]` | `registered` |
//! | `unregister` | `owner`, `prefix`, `forget` | `removed` |
//! | `watch` | `prefix` | `watching`, then `{"event":"changed",...}` and `registered`/`unregistered` lines |
//! | `status` | – | `place` (stick/fallback), `reason`, `dir`, counts |
//!
//! Errors: `{"ok":false,"error":<code>,"message":...}` with code `bad_request`, `unknown_key`, `bad_value`,
//! `refused_r8`, `needs_confirm`, `not_owner`, `apply_failed`, `config_refused`, `nothing_to_undo`, `not_undoable`,
//! `changed_meanwhile`.

use crate::change::Who;
use crate::json::{self, Json};
use crate::key::KeyDef;
use crate::registry::{Error, Registry, err};
use crate::server::Shared;

pub enum Reply {
    Line(String),
    Watch(String),
}

pub fn error_line(e: &Error) -> String {
    Json::obj().with("ok", false).with("error", e.code).with("message", e.msg.as_str()).to_text()
}

fn count(j: &Json, key: &str, default: usize) -> Result<usize, Error> {
    match j.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(v) => v
            .as_i64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| err("bad_request", format!("{key}: want a count"))),
    }
}

fn need<'a>(j: &'a Json, key: &str) -> Result<&'a str, Error> {
    j.str(key).ok_or_else(|| err("bad_request", format!("{key} missing")))
}

/// Reads are answered from the last snapshot (they never wait behind a change that waits for the stick);
/// changes take the registry lock and publish a new snapshot afterwards, also when they failed half-way.
pub fn handle(shared: &Shared, line: &str) -> Reply {
    let r = (|| -> Result<Reply, Error> {
        let j = json::parse(line).map_err(|e| err("bad_request", format!("not JSON: {e}")))?;
        let op = need(&j, "op")?;
        if op == "watch" {
            return Ok(Reply::Watch(j.str("prefix").unwrap_or("").to_string()));
        }
        let read = || -> Result<Option<Json>, Error> {
            let v = shared.view();
            Ok(Some(match op {
                "list" => v.list(j.str("prefix").unwrap_or("")),
                "describe" => v.describe(need(&j, "key")?)?,
                "get" => v.get(need(&j, "key")?, j.str("owner"))?,
                "history" => v.history(count(&j, "n", 5)?, j.str("prefix")),
                "status" => v.status(),
                _ => return Ok(None),
            }))
        };
        if let Some(out) = read()? {
            return Ok(Reply::Line(out.to_text()));
        }
        let mut r = shared.lock();
        let out = change(&mut r, op, &j);
        shared.publish(&r);
        Ok(Reply::Line(out?.to_text()))
    })();
    r.unwrap_or_else(|e| Reply::Line(error_line(&e)))
}

fn change(r: &mut Registry, op: &str, j: &Json) -> Result<Json, Error> {
    let source = j.str("source").unwrap_or("socket");
    let confirmed = j.get("confirmed").and_then(Json::as_bool).unwrap_or(false);
    let who = Who { source, confirmed };
    Ok(match op {
        "set" => {
            let v = j.get("value").ok_or_else(|| err("bad_request", "value missing"))?;
            r.set(need(j, "key")?, v, &who)?
        }
        "reset" => r.reset(need(j, "key")?, &who)?,
        "undo" => {
            let expect = match j.get("expect_seq") {
                None | Some(Json::Null) => None,
                Some(v) => Some(v.as_i64().and_then(|n| u64::try_from(n).ok()).ok_or_else(|| err("bad_request", "expect_seq: want a journal number"))?),
            };
            r.undo_expect(count(j, "n", 1)?, expect, &who)?
        }
        "register" => {
            let owner = need(j, "owner")?;
            if owner == "settingsd" {
                return Err(err("not_owner", "only settingsd itself is settingsd"));
            }
            let Some(Json::Arr(keys)) = j.get("keys") else { return Err(err("bad_request", "keys: want a list")) };
            let defs = keys
                .iter()
                .map(|k| {
                    KeyDef::from_json(k, owner)
                        .map_err(|e| err(if e.contains("R8") { "refused_r8" } else { "bad_request" }, e))
                })
                .collect::<Result<Vec<_>, _>>()?;
            r.register(owner, defs)?
        }
        "unregister" => {
            let forget = j.get("forget").and_then(Json::as_bool).unwrap_or(false);
            r.unregister(need(j, "owner")?, need(j, "prefix")?, forget)?
        }
        other => return Err(err("bad_request", format!("unknown op {other:?}"))),
    })
}
