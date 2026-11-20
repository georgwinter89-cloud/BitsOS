//! atspid: the bridge from foreign programs' accessibility trees to Durchblick (lib.rs says what it is).
#![forbid(unsafe_code)]

use atspid::{client, serve};
use serde_json::{Value, json};
use std::path::PathBuf;

const USAGE: &str = "usage:
  atspid serve [--bus PATH] [--socket PATH] [--settings PATH|--no-settings] [--look SECONDS] [--peer EXE]...
                                                               the accessibility bus and the C3 socket (asked by
                                                               root and the desktop; --peer: one more program)
  atspid tree [--socket PATH] [--origin X,Y] [--size W,H] PID [TITLE]
                                                               the program's window as an element list (JSON);
                                                               origin and size of its surface on the desktop
  atspid act [--socket PATH] PID ELEMENT ACTION [TEXT]         press|toggle|type|choose|focus|scroll
  atspid ping [--socket PATH]
  atspid env                                                   what a program needs to show its tree";

fn take(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)?;
    args.remove(i);
    (i < args.len()).then(|| args.remove(i))
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    let cmd = if a.is_empty() { String::new() } else { a.remove(0) };
    let socket = PathBuf::from(take(&mut a, "--socket").unwrap_or_else(|| serve::SOCKET.into()));
    let r = match cmd.as_str() {
        "serve" => {
            let bus = PathBuf::from(take(&mut a, "--bus").unwrap_or_else(|| serve::BUS.into()));
            let no = a.iter().any(|x| x == "--no-settings");
            let settings = take(&mut a, "--settings").unwrap_or_else(|| atspid::settings::SOCKET.into());
            let settings = (!no).then(|| PathBuf::from(settings));
            let look = take(&mut a, "--look").and_then(|s| s.parse().ok()).unwrap_or(serve::LOOK_S);
            let look = std::time::Duration::from_secs(look.max(1));
            let mut peers = Vec::new();
            while let Some(p) = take(&mut a, "--peer") {
                peers.push(PathBuf::from(p));
            }
            serve::run(serve::Opts { bus, socket, settings, uid: serve::SESSION_UID, look, peers }).map(|()| Value::Null)
        }
        "tree" => {
            let pair = |v: Option<String>| -> Option<Vec<i64>> { v.map(|s| s.split(',').filter_map(|n| n.trim().parse().ok()).collect()) };
            let (origin, size) = (pair(take(&mut a, "--origin")), pair(take(&mut a, "--size")));
            let Some(pid) = a.first().and_then(|p| p.parse::<u64>().ok()) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let mut args = json!({"pid": pid});
            for (k, v) in [("origin", origin), ("size", size)] {
                if let Some(v) = v.filter(|v| v.len() == 2) {
                    args[k] = json!(v);
                }
            }
            if let Some(t) = a.get(1) {
                args["title"] = json!(t);
            }
            client::ask(&socket, "tree", args)
        }
        "act" if a.len() >= 3 => {
            let pid: u64 = a[0].parse().unwrap_or(0);
            let mut args = json!({"pid": pid, "element": a[1], "action": a[2]});
            if let Some(t) = a.get(3) {
                args["text"] = json!(t);
            }
            client::ask(&socket, "act", args)
        }
        "ping" => client::ask(&socket, "ping", Value::Null),
        "env" => {
            println!("AT_SPI_BUS_ADDRESS=unix:path={}", serve::BUS);
            Ok(Value::Null)
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };
    match r {
        Ok(Value::Null) => {}
        Ok(v) => {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            if v["ok"] != json!(true) {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("atspid: {e}");
            std::process::exit(1);
        }
    }
}
