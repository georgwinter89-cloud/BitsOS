//! The `settings` command (also `settingsd <command>`): talks to settingsd over its socket, for Georg's shell
//! and for tests. Exit status: 0 done, 1 settingsd said no, 2 usage or no connection.

use crate::json::{self, Json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

pub const COMMANDS: &[&str] = &["list", "get", "describe", "set", "reset", "undo", "history", "status", "watch", "raw"];

const USAGE: &str = "usage: settings [--socket PATH] [--json] [--source NAME] <command>
  list [PREFIX]            keys with their values (* = changed)
  get KEY | describe KEY
  set KEY VALUE [--confirm]
  reset KEY                back to the default
  undo [N]                 revert the last N changes (default 1)
  history [N] [PREFIX]     the last N changes (default 5)
  status | watch [PREFIX] | raw JSON";

pub fn main(args: &[String]) -> i32 {
    let mut socket = std::env::var("SETTINGS_SOCKET").unwrap_or_else(|_| "/run/bitsos/settings.sock".into());
    let mut raw_out = false;
    let mut source = "cli".to_string();
    let mut confirm = false;
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = it.next().cloned().unwrap_or_default(),
            "--json" => raw_out = true,
            "--source" => source = it.next().cloned().unwrap_or_default(),
            "--confirm" => confirm = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            _ => rest.push(a),
        }
    }
    let req = match build(&rest, &source, confirm) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("settings: {e}\n{USAGE}");
            return 2;
        }
    };
    let s = match UnixStream::connect(&socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("settings: settingsd not reachable at {socket}: {e}");
            return 2;
        }
    };
    let watch = rest.first() == Some(&"watch");
    match talk(s, &req, watch, |line| {
        if raw_out || watch {
            println!("{line}");
        } else {
            show(rest[0], line);
        }
    }) {
        Ok(ok) => i32::from(!ok),
        Err(e) => {
            eprintln!("settings: {e}");
            2
        }
    }
}

fn build(a: &[&str], source: &str, confirm: bool) -> Result<String, String> {
    let arg = |i: usize| a.get(i).copied();
    let need = |i: usize, what: &str| arg(i).ok_or_else(|| format!("{} needs {what}", a[0]));
    let n = |i: usize| arg(i).map(|s| s.parse::<i64>().map_err(|_| format!("{s:?} is not a number"))).transpose();
    let base = |op: &str| Json::obj().with("op", op).with("source", source);
    let j = match a.first().copied() {
        None => return Err("no command".into()),
        Some("list") => base("list").with("prefix", arg(1).unwrap_or("")),
        Some(op @ ("get" | "describe" | "reset")) => base(op).with("key", need(1, "a key")?),
        Some("set") => {
            base("set").with("key", need(1, "a key")?).with("value", need(2, "a value")?).with("confirmed", confirm)
        }
        Some("undo") => base("undo").with("n", n(1)?.unwrap_or(1)),
        Some("history") => {
            let (cnt, prefix) = match n(1) {
                Ok(c) => (c.unwrap_or(5), arg(2)),
                Err(_) => (5, arg(1)),
            };
            base("history").with("n", cnt).with("prefix", prefix)
        }
        Some("status") => base("status"),
        Some("watch") => base("watch").with("prefix", arg(1).unwrap_or("")),
        Some("raw") => return Ok(need(1, "a JSON request")?.to_string()),
        Some(c) => return Err(format!("unknown command {c:?}")),
    };
    Ok(j.to_text())
}

/// Sends one request; calls `out` for the answer (and every event line when watching). Ok(answer was ok).
pub fn talk(s: UnixStream, req: &str, watch: bool, mut out: impl FnMut(&str)) -> Result<bool, String> {
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut rd = BufReader::new(s);
    let mut line = String::new();
    rd.read_line(&mut line).map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("settingsd closed the connection".into());
    }
    let ok = json::parse(line.trim()).ok().and_then(|j| j.get("ok").and_then(Json::as_bool)).unwrap_or(false);
    out(line.trim_end());
    if watch && ok {
        loop {
            line.clear();
            match rd.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => out(line.trim_end()),
            }
        }
    }
    Ok(ok)
}

fn val(j: Option<&Json>) -> String {
    match j {
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Null) | None => "-".into(),
        Some(v) => v.to_text(),
    }
}

/// Human answers: one line per key or change.
fn show(cmd: &str, line: &str) {
    let Ok(j) = json::parse(line) else {
        println!("{line}");
        return;
    };
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        eprintln!("settings: {}: {}", val(j.get("error")), val(j.get("message")));
        return;
    }
    let change = |c: &Json| {
        format!(
            "{}: {} -> {} (applied: {}; restart: {}; stored: {})",
            val(c.get("key")),
            val(c.get("old")),
            val(c.get("new")),
            val(c.get("applied")),
            val(c.get("restart")),
            val(c.get("stored"))
        )
    };
    match cmd {
        "list" => {
            if let Some(Json::Arr(keys)) = j.get("keys") {
                for k in keys {
                    let star = if k.get("changed").and_then(Json::as_bool) == Some(true) { "*" } else { " " };
                    println!("{star} {} = {}    # {}", val(k.get("key")), val(k.get("value")), val(k.get("text")));
                }
            }
        }
        "get" => println!("{}", val(j.get("value"))),
        "set" | "reset" => println!("{}", change(&j)),
        "undo" => {
            if let Some(Json::Arr(u)) = j.get("undone") {
                for c in u {
                    println!("undone {}", change(c));
                }
            }
            if let Some(Json::Str(s)) = j.get("stopped") {
                println!("stopped: {s}");
            }
        }
        "history" => {
            if let Some(Json::Arr(es)) = j.get("entries") {
                for e in es {
                    let undone = if e.get("undone").and_then(Json::as_bool) == Some(true) { " (undone)" } else { "" };
                    println!(
                        "#{} {} {}: {} -> {} by {}{undone}",
                        val(e.get("seq")),
                        val(e.get("op")),
                        val(e.get("key")),
                        val(e.get("old")),
                        val(e.get("new")),
                        val(e.get("source"))
                    );
                }
            }
        }
        _ => println!("{line}"),
    }
}
