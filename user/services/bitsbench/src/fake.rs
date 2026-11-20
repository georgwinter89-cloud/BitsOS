//! Stand-ins for the programs that run only on the Mini-PC (Georg 30.10.: Brave, Steam and games never in QEMU)
//! and for the profiler of lane SO, so the host tests and the QEMU scenario run the whole set:
//! - `fake brave`: answers the remote debugging protocol on fds 3 and 4 as Brave does for the calls the runner
//!   makes (one tab, attach, navigate with its load event, wheel events, close);
//! - `fake game`: prints a timedemo line as vkQuake does and waits to be ended;
//! - `fake bitsprof run --out DIR ...` / `report DIR ...`: writes the contract's two files, prints a report.
//! Steam's stand-in needs no code: any program that maps a window (the QEMU scenario uses `bitscomp page`).

use mcpreg::Json;
use std::time::Duration;

fn send(j: Json) -> bool {
    let mut b = j.to_string().into_bytes();
    b.push(0);
    let mut off = 0;
    while off < b.len() {
        match usys::write(4, &b[off..]) {
            Ok(n) if n > 0 => off += n,
            _ => return false,
        }
    }
    true
}

/// The answer of the fake Brave to one command (and an event to send after it).
pub fn answer(cmd: &Json) -> (Json, Option<Json>) {
    let id = cmd.get("id").cloned().unwrap_or(Json::Null);
    let session = cmd.get("sessionId").cloned();
    let method = cmd.get("method").and_then(Json::as_str).unwrap_or("");
    let ok = |r: Json| Json::obj(vec![("id", id.clone()), ("result", r)]);
    match method {
        "Target.getTargets" => {
            let t = Json::obj(vec![("targetId", Json::str("T1")), ("type", Json::str("page")), ("url", Json::str("about:blank"))]);
            (ok(Json::obj(vec![("targetInfos", Json::Arr(vec![t]))])), None)
        }
        "Target.attachToTarget" => (ok(Json::obj(vec![("sessionId", Json::str("S1"))])), None),
        "Page.navigate" => {
            let mut ev = vec![("method", Json::str("Page.loadEventFired")), ("params", Json::obj(vec![("timestamp", Json::Num(1.0))]))];
            if let Some(s) = session {
                ev.push(("sessionId", s));
            }
            (ok(Json::obj(vec![("frameId", Json::str("F1"))])), Some(Json::obj(ev)))
        }
        "Page.enable" | "Input.dispatchMouseEvent" | "Browser.close" => (ok(Json::obj(vec![])), None),
        other => (Json::obj(vec![("id", id), ("error", Json::obj(vec![("code", Json::Num(-32601.0)), ("message", Json::str(&format!("{other} wasn't found")))]))]), None),
    }
}

pub fn brave() -> i32 {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = match usys::read(3, &mut chunk) {
            Ok(n) if n > 0 => n,
            _ => return 0,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(i) = buf.iter().position(|&b| b == 0) {
            let msg: Vec<u8> = buf.drain(..=i).collect();
            let Ok(cmd) = Json::parse(&String::from_utf8_lossy(&msg[..msg.len() - 1])) else { continue };
            let (a, ev) = answer(&cmd);
            // A real tab takes a moment to load its page.
            if ev.is_some() {
                std::thread::sleep(Duration::from_millis(30));
            }
            if !send(a) || ev.is_some_and(|e| !send(e)) {
                return 0;
            }
            if cmd.get("method").and_then(Json::as_str) == Some("Browser.close") {
                return 0;
            }
        }
    }
}

pub fn game() -> i32 {
    std::thread::sleep(Duration::from_millis(300));
    println!("969 frames  4.2 seconds 230.7 fps");
    std::thread::sleep(Duration::from_secs(60));
    0
}

pub fn bitsprof(args: &[String]) -> i32 {
    let at = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1));
    match args.first().map(String::as_str) {
        Some("run") => {
            let Some(dir) = at("--out") else { return 2 };
            let dir = std::path::Path::new(dir);
            let secs = at("--seconds").and_then(|s| s.parse::<u64>().ok());
            let meta = Json::obj(vec![
                ("fake", Json::Bool(true)),
                ("hz", Json::str(at("--hz").map_or("1000", String::as_str))),
                ("seconds", secs.map_or(Json::Null, |s| Json::Num(s as f64))),
            ]);
            if std::fs::write(dir.join("meta.json"), format!("{meta}\n")).and_then(|_| std::fs::write(dir.join("profile.bin"), b"fake")).is_err() {
                return 1;
            }
            std::thread::sleep(Duration::from_secs(secs.unwrap_or(3600).min(3600)));
            0
        }
        Some("report") => {
            let Some(dir) = args.get(1) else { return 2 };
            if !std::path::Path::new(dir).join("meta.json").is_file() {
                return 1;
            }
            let top = Json::obj(vec![("name", Json::str("fake_hot_function")), ("share", Json::Num(0.5))]);
            println!("{}", Json::obj(vec![("fake", Json::Bool(true)), ("kernel", Json::Arr(vec![top]))]));
            0
        }
        _ => 2,
    }
}
