//! `bits programm start|finde|satz|befehle`: the library's programs through bitscomp (`program-start`,
//! `program-find`), and the scripting set "skripte" through the program service bitspkgd (`list`, `plan`, `do`).

use std::time::Duration;

use serde_json::{Value, json};

use crate::out::{Done, Fail, R};
use crate::{befehl, frage, sock};

pub fn run(args: &[String]) -> R {
    let name = || {
        let n = args[1..].join(" ");
        if n.trim().is_empty() { Err(Fail::usage("Welches Programm?")) } else { Ok(n) }
    };
    match args.first().map(String::as_str) {
        Some("start") => {
            let answer = frage::changing(&format!("program-start {}", name()?))?;
            Ok(Done::new(answer.clone()).with("answer", answer))
        }
        Some("finde") => {
            let raw = sock::ctl(&format!("program-find {}", name()?))?;
            let (id, n) = raw.split_once('\t').unwrap_or((raw.as_str(), ""));
            Ok(Done::new(format!("{id}  {n}")).with("id", id).with("name", n))
        }
        Some("satz") => satz(&args[1..]),
        Some("befehle") => befehl::list(),
        Some(v) => Err(Fail::usage(format!("„programm {v}“ gibt es nicht; möglich: start, finde, satz, befehle."))),
        None => Err(Fail::usage("programm start|finde|satz|befehle")),
    }
}

const WHO: &str = "Der Programmdienst";
pub const SET_NAME: &str = "skripte";

fn pkg(req: Value, wait: Duration, progress: impl FnMut(&Value)) -> R<Value> {
    sock::ok_or_fail(sock::json_line(&sock::pkg_path(), WHO, &req, wait, progress)?)
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

/// `programm satz [skripte] [--installieren]`: each program of the set, installed or what installing it brings;
/// with `--installieren` the missing ones are installed one after the other (bitspkgd lets root and the desktop).
fn satz(a: &[String]) -> R {
    let install = a.iter().any(|x| x == "--installieren");
    if let Some(n) = a.iter().find(|x| !x.starts_with("--"))
        && n != SET_NAME
    {
        return Err(Fail::usage(format!("Den Satz „{n}“ gibt es nicht; es gibt „{SET_NAME}“.")));
    }
    if install && bitstree::of_self().inside() {
        return Err(Fail::refused("Programme installiert der Assistent nur über sein Werkzeug program_install, mit Georgs Ja. Es wurde nichts installiert."));
    }
    let listed = pkg(json!({"op": "list"}), Duration::from_secs(30), |_| {})?;
    let have = |p: &str| listed["programs"].as_array().and_then(|l| l.iter().find(|x| x["name"] == p)).and_then(|x| x["current"].as_str().map(str::to_string));
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    // Shared packages (glibc, openssl, ...) come once: the sum of the plans counts them again for each program.
    let mut download = 0u64;
    let mut parts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    for (prog, cmds) in befehl::members() {
        if let Some(v) = have(&prog) {
            lines.push(format!("{prog}: installiert ({v})"));
            rows.push(json!({"program": prog, "commands": cmds, "installed": v}));
            continue;
        }
        let plan = pkg(json!({"op": "plan", "action": "install", "name": prog}), Duration::from_secs(60), |_| {})?;
        let bytes = plan["download"].as_u64().unwrap_or(0);
        download += bytes;
        for p in plan["packages"].as_array().into_iter().flatten() {
            parts.insert(p["name"].as_str().unwrap_or("").to_string(), p["csize"].as_u64().unwrap_or(0));
        }
        let mut row = json!({"program": prog, "commands": cmds, "installed": null, "installable": plan["installable"], "download": bytes, "summary": plan["summary"]});
        lines.push(format!("{prog}: {}", plan["summary"].as_str().unwrap_or("?")));
        if install {
            let id = plan["plan_id"].as_str().ok_or_else(|| Fail::refused(format!("{}: {}", prog, plan["note"].as_str().unwrap_or("Installieren dürfen root an der Konsole und die Oberfläche."))))?;
            let done = pkg(json!({"op": "do", "plan_id": id}), Duration::from_secs(3600), |p| {
                if !crate::out::json_mode() {
                    eprintln!("{prog}: {}", p["progress"].as_str().or(p["step"].as_str()).unwrap_or("…"));
                }
            })?;
            lines.push(format!("{prog}: installiert"));
            row["installed"] = done["program"].clone();
            row["report"] = done["report"].clone();
        }
        rows.push(row);
    }
    if !parts.is_empty() {
        download = download.min(parts.values().sum());
    }
    if download > 0 && !install {
        lines.push(format!("Zu laden: etwa {}. Installieren: als root „bits programm satz {SET_NAME} --installieren“.", mb(download)));
    }
    Ok(Done::new(lines.join("\n")).with("set", SET_NAME).with("programs", rows).with("download", download))
}
