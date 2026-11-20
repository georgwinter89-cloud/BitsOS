//! `bits`: one command for BitsOS (PLAN K7). `bits [--json] <thema> <verb> [...]`; BITS.md is its contract and its
//! help text. A link named after a command of the scripting set (`python3`, `git`, ...) starts that command.
#![forbid(unsafe_code)]

mod befehl;
mod clip;
mod desk;
mod einst;
mod fenster;
mod foto;
mod frage;
mod hilfe;
mod messwerte;
mod out;
mod papierkorb;
mod png;
mod programm;
mod py;
mod raum;
mod sock;
mod trash;
mod ui;

use std::ffi::OsString;

use out::{Fail, R};

/// The command topics (BITS.md has a few more help-only sections).
const TOPICS: &[&str] = &["fenster", "ui", "einstellungen", "arbeitsraum", "dateien", "zwischenablage", "foto", "meldung", "messwerte", "programm", "papierkorb", "hilfe"];

fn dispatch(args: &[String]) -> R {
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match args.first().map(String::as_str) {
        None | Some("hilfe") => hilfe::run(rest),
        Some("fenster") => fenster::run(rest),
        Some("ui") => ui::run(rest),
        Some("einstellungen") => einst::run(rest),
        Some("arbeitsraum") => raum::arbeitsraum(rest),
        Some("dateien") => raum::dateien(rest),
        Some("zwischenablage") => clip::run(rest),
        Some("foto") => foto::run(rest),
        Some("meldung") => desk::meldung(rest),
        Some("messwerte") => messwerte::run(rest),
        Some("programm") => programm::run(rest),
        Some("papierkorb") => papierkorb::run(rest),
        Some(t) => Err(Fail::usage(format!("Ein Thema „{t}“ gibt es nicht; es gibt: {}.", TOPICS.join(", ")))),
    }
}

fn main() {
    let mut argv = std::env::args_os();
    let me = argv.next().unwrap_or_default();
    let name = std::path::Path::new(&me).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let raw: Vec<OsString> = argv.collect();
    // A link of the set: everything after the name belongs to the command, not to bits.
    if name != "bits" && befehl::program_of(&name).is_some() {
        let r = befehl::run(&name, raw);
        std::process::exit(out::show(&r, false));
    }
    if raw.first().is_some_and(|a| a == "befehl") {
        let r = match raw.get(1).and_then(|c| c.to_str()) {
            Some(c) => befehl::run(c, raw[2..].to_vec()),
            None => Err(Fail::usage("befehl <name> [argumente]")),
        };
        std::process::exit(out::show(&r, false));
    }
    let mut args = Vec::new();
    for a in raw {
        match a.into_string() {
            Ok(s) => args.push(s),
            Err(a) => {
                let r = Err(Fail::usage(format!("{:?} ist kein UTF-8", a.to_string_lossy())));
                std::process::exit(out::show(&r, false));
            }
        }
    }
    let json = args.iter().any(|a| a == "--json");
    args.retain(|a| a != "--json");
    if args.first().is_some_and(|a| a == "-h" || a == "--help") {
        args = vec!["hilfe".into()];
    }
    out::set_json(json);
    let r = dispatch(&args);
    std::process::exit(out::show(&r, json));
}
