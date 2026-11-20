//! `bits zwischenablage lesen|setzen`: the clipboard through bitscomp (`bits-clip-get`, `bits-clip-set`). The text
//! to set goes through a file of this user's runtime folder (a control line is short), which bitscomp reads once.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

use serde_json::Value;

use crate::frage;
use crate::out::{Done, Fail, R};
use crate::sock;

/// The most text set at once (bitscomp reads no more).
pub const MAX: usize = 1 << 20;

pub fn run(args: &[String]) -> R {
    match args.first().map(String::as_str) {
        Some("lesen") | None => lesen(),
        Some("setzen") => setzen(&args[1..]),
        Some(v) => Err(Fail::usage(format!("„zwischenablage {v}“ gibt es nicht; möglich: lesen, setzen."))),
    }
}

fn lesen() -> R {
    let raw = sock::ctl("bits-clip-get")?;
    let v: Value = serde_json::from_str(&raw).map_err(|e| Fail::failed(format!("unverständliche Antwort: {e}")))?;
    let text = v["text"].as_str().unwrap_or("").to_string();
    Ok(Done::new(text.clone()).with("text", text))
}

fn setzen(a: &[String]) -> R {
    let text = if a.is_empty() || a == ["-"] {
        let mut s = String::new();
        std::io::stdin().take(MAX as u64 + 1).read_to_string(&mut s).map_err(|e| Fail::usage(format!("Die Eingabe ist kein Text: {e}")))?;
        s
    } else {
        a.join(" ")
    };
    if text.len() > MAX {
        return Err(Fail::usage(format!("Mehr als {} KiB gehen nicht in die Zwischenablage.", MAX >> 10)));
    }
    let path = sock::runtime_dir().join(format!(".bits-clip-{}-{}", std::process::id(), nanos()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(|e| Fail::failed(format!("{}: {e}", path.display())))?;
    let r = f.write_all(text.as_bytes()).map_err(|e| Fail::failed(e.to_string())).and_then(|()| frage::changing(&format!("bits-clip-set {}", path.display())));
    let _ = std::fs::remove_file(&path);
    r?;
    Ok(Done::new(format!("In der Zwischenablage: {} Zeichen.", text.chars().count())).with("chars", text.chars().count()))
}

pub fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}
