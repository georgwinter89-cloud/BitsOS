//! The six MCP tools of V2-PLAN §8a.3 and the socket op (`proto`) each one stands for. A tool answer is
//! settingsd's own JSON answer line, so Claude sees what the `settings` command sees; an error answer becomes
//! "code: message" (`isError`), a malformed request a bad argument (-32602).

use crate::json::{self as sj, Json as SJson};
use crate::proto::{self, Reply as PReply};
use crate::server::Shared;
use mcpreg::{Json, Reply, Safety, Tool};

/// The largest integer an f64 holds exactly together with all smaller ones (JavaScript's MAX_SAFE_INTEGER).
const MAX_EXACT: f64 = 9_007_199_254_740_991.0;

pub const NAMES: [&str; 6] =
    ["settings_list", "settings_describe", "settings_get", "settings_set", "settings_undo", "settings_history"];

const CONFIRM: &str = "Keys of safety class confirm need \"confirmed\": true; set it only after Georg agreed to \
this change in the conversation.";

pub fn tools() -> Vec<Tool> {
    let t = |name: &str, desc: String, schema: &str, safety| {
        Tool::new(name, &desc, schema, safety).expect("built-in tool schemas are JSON")
    };
    vec![
        t(
            "settings_list",
            "List BitsOS settings (key, type, value, changed, short text in the language of settings.language). \
`prefix` limits to one group (\"theme\", \"kernel\"); `limit` cuts the list (the answer then says how many \
more). Secret values show as ***."
                .into(),
            r#"{"type":"object","properties":{"prefix":{"type":"string"},"limit":{"type":"integer","minimum":1}}}"#,
            Safety::Read,
        ),
        t(
            "settings_describe",
            "Everything about one setting: schema (type, range, choices), default, current value, German and \
English text, who applies it (kernel/service/settingsd), whether a change needs a restart, safety class, owner."
                .into(),
            r#"{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}"#,
            Safety::Read,
        ),
        t(
            "settings_get",
            "Current value of one setting and whether it differs from the default. Secret values show as ***.".into(),
            r#"{"type":"object","properties":{"key":{"type":"string"}},"required":["key"]}"#,
            Safety::Read,
        ),
        t(
            "settings_set",
            format!(
                "Change one setting. `value` has the key's type (boolean, integer or text; see settings_describe). \
The answer says old and new value, `applied` (now / next_start / notified), `restart` (none / service / reboot), \
`seq` (journal number) and where it was stored. Every change is journaled and can be undone with \
settings_undo. The safety switches (WLAN, NVMe/stick/USB writes, display writes) are not settings and are \
refused (refused_r8). {CONFIRM}"
            ),
            r#"{"type":"object","properties":{"key":{"type":"string"},"value":{"type":["boolean","integer","string"]},"confirmed":{"type":"boolean"}},"required":["key","value"]}"#,
            Safety::Change,
        ),
        t(
            "settings_undo",
            format!(
                "Undo the newest `n` changes (default 1) that are not undone yet, newest first; an undo is itself \
journaled. The answer lists what was undone and, if it stopped early, why (`stopped`). Secret keys cannot be \
undone. {CONFIRM}"
            ),
            r#"{"type":"object","properties":{"n":{"type":"integer","minimum":1},"confirmed":{"type":"boolean"}}}"#,
            Safety::Change,
        ),
        t(
            "settings_history",
            "The last `n` changes (default 5), newest first: key, old, new, source, time, seq, undone. `prefix` \
limits to one group. Secret values are never journaled."
                .into(),
            r#"{"type":"object","properties":{"n":{"type":"integer","minimum":1},"prefix":{"type":"string"}}}"#,
            Safety::Read,
        ),
    ]
}

/// Changes go to the change worker (one at a time, they may wait for the stick); reads never wait for them.
pub fn is_change(name: &str) -> bool {
    matches!(name, "settings_set" | "settings_undo")
}

fn text_arg<'a>(args: &'a Json, name: &str, required: bool) -> Result<Option<&'a str>, String> {
    match args.get(name) {
        None | Some(Json::Null) if !required => Ok(None),
        None | Some(Json::Null) => Err(format!("{name} missing")),
        Some(v) => v.as_str().map(Some).ok_or_else(|| format!("{name}: want text")),
    }
}

fn count_arg(args: &Json, name: &str) -> Result<Option<u64>, String> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| *n >= 1).map(Some).ok_or_else(|| format!("{name}: want a count of 1 or more")),
    }
}

fn bool_arg(args: &Json, name: &str) -> Result<bool, String> {
    match args.get(name) {
        None | Some(Json::Null) => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| format!("{name}: want true or false")),
    }
}

/// The socket request a call stands for. Only named fields are taken over: `owner` (reveals secrets to their
/// service) and `source` are never the caller's to choose.
pub fn request(name: &str, args: &Json) -> Result<SJson, String> {
    let with_opt = |j: SJson, k: &str, v: Option<&str>| if let Some(v) = v { j.with(k, v) } else { j };
    let with_n = |j: SJson, v: Option<u64>| if let Some(v) = v { j.with("n", v) } else { j };
    Ok(match name {
        "settings_list" => {
            count_arg(args, "limit")?;
            with_opt(SJson::obj().with("op", "list"), "prefix", text_arg(args, "prefix", false)?)
        }
        "settings_describe" | "settings_get" => {
            let op = if name == "settings_get" { "get" } else { "describe" };
            SJson::obj().with("op", op).with("key", text_arg(args, "key", true)?.unwrap_or_default())
        }
        "settings_set" => {
            let key = text_arg(args, "key", true)?.unwrap_or_default();
            let value = match args.get("value") {
                None | Some(Json::Null) => return Err("value missing".into()),
                // mcpreg numbers are f64: beyond 2^53 a different integer than the one sent would be stored.
                Some(Json::Num(n)) if n.abs() > MAX_EXACT => {
                    return Err("value: integer too large to arrive exactly (at most 2^53-1)".into());
                }
                Some(v @ (Json::Bool(_) | Json::Num(_) | Json::Str(_))) => {
                    sj::parse(&v.to_string()).map_err(|e| format!("value: {e}"))?
                }
                Some(_) => return Err("value: want a boolean, an integer or text".into()),
            };
            SJson::obj()
                .with("op", "set")
                .with("key", key)
                .with("value", value)
                .with("source", "mcp")
                .with("confirmed", bool_arg(args, "confirmed")?)
        }
        "settings_undo" => with_n(SJson::obj().with("op", "undo"), count_arg(args, "n")?)
            .with("source", "mcp")
            .with("confirmed", bool_arg(args, "confirmed")?),
        "settings_history" => with_opt(
            with_n(SJson::obj().with("op", "history"), count_arg(args, "n")?),
            "prefix",
            text_arg(args, "prefix", false)?,
        ),
        other => return Err(format!("{other}: not a settingsd tool")),
    })
}

/// Runs one call against the registry and turns settingsd's answer into the tool's answer.
pub fn call(shared: &Shared, name: &str, args: &Json) -> Reply {
    let req = match request(name, args) {
        Ok(r) => r,
        Err(e) => return Reply::bad_argument(e),
    };
    let out = match proto::handle(shared, &req.to_text()) {
        PReply::Line(l) => l,
        PReply::Watch(_) => return Reply::error("internal: a watch is not a tool"),
    };
    let Ok(mut a) = sj::parse(&out) else { return Reply::error(format!("internal: answer is not JSON: {out}")) };
    if a.get("ok").and_then(SJson::as_bool) != Some(true) {
        let code = a.str("error").unwrap_or("error").to_string();
        let msg = a.str("message").unwrap_or("").to_string();
        return match code.as_str() {
            "bad_request" => Reply::bad_argument(msg),
            "needs_confirm" => Reply::error(format!("needs_confirm: {msg} {CONFIRM}")),
            _ => Reply::error(format!("{code}: {msg}")),
        };
    }
    if let (Ok(Some(limit)), SJson::Obj(fields)) = (count_arg(args, "limit"), &mut a)
        && name == "settings_list"
    {
        let mut more = 0;
        for (k, v) in fields.iter_mut() {
            if let (true, SJson::Arr(keys)) = (k == "keys", v) {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                more = keys.len().saturating_sub(limit);
                keys.truncate(limit);
            }
        }
        fields.push(("more".into(), SJson::from(more)));
    }
    Reply::text(a.to_text())
}
