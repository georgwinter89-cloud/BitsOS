//! The command line: the chat, `--once`, `serve` (the service of both views), the three small commands that set a
//! provider up at the console, and `--test`, which the settings page "KI" (CL-5) runs too.

use crate::config::{self, parse_ids, valid_id};
use crate::json::Json;
use crate::settings::{self, Settings, Socket};
use crate::term::sys;
use crate::tools::ToolSource;
use crate::tools::both::Tools;
use crate::tools::local::{self, Place};
use crate::tools::settings::{SOURCE, SettingsTools};
use crate::mcp::{self, End, Named, Opts, gate::Gate};
use crate::term::wrap;
use crate::{chat, de, limits, once, probe, serve};

fn providers(s: &dyn Settings) -> i32 {
    let cfg = match config::load(s) {
        Ok(c) => c,
        Err(e) => return unreachable(&e),
    };
    if cfg.ids.is_empty() {
        print!("{}", de::howto());
        return 0;
    }
    println!("Eingerichtete Anbieter (ai.providers): {}", cfg.ids.join(", "));
    for id in &cfg.ids {
        let e = cfg.entries.get(id).cloned().unwrap_or_default();
        // Only whether a key is set; its value is not even fetched here.
        let has_key = s.request(&Json::obj().with("op", "get").with("key", format!("ai.provider.{id}.key"))).ok().and_then(|a| a.get("changed").and_then(Json::as_bool)) == Some(true);
        let place = match e.place.as_str() {
            "cloud" => "Cloud",
            "local" => "lokal",
            _ => "Ort fehlt",
        };
        let or = |v: &str, missing: &str| if v.is_empty() || v == "unset" { missing.to_string() } else { v.to_string() };
        println!(
            "  {id}: {} · {place} · {} · Modell {} · Schlüssel {}",
            or(&e.kind, "Art fehlt"),
            or(&e.address, "Adresse fehlt"),
            or(&e.model, if e.kind == "program" { "nicht genannt" } else { "fehlt" }),
            if has_key { "gesetzt" } else { "nicht gesetzt" }
        );
        if let Err(missing) = cfg.provider(id) {
            println!("      {missing}");
        }
    }
    let or_none = |v: &str| if v.is_empty() { "keiner".to_string() } else { v.to_string() };
    println!("Gespräch (ai.role.chat): {} · Ersatz (ai.role.chat_fallback): {} · Nur lokal (ai.local_only): {}", or_none(&cfg.chat), or_none(&cfg.fallback.join(", ")), if cfg.local_only { "an" } else { "aus" });
    0
}

fn unreachable(e: &settings::Refused) -> i32 {
    eprintln!("assist: Die Einstellungen sind nicht erreichbar ({e}). Läuft settingsd?");
    1
}

fn add(s: &dyn Settings, id: &str) -> i32 {
    if !valid_id(id) {
        eprintln!("assist: Der Name „{id}“ geht nicht: nur Kleinbuchstaben, Ziffern, - und _ (zum Beispiel claude oder strata).");
        return 2;
    }
    let listed = match settings::get(s, "ai.providers") {
        Ok(v) => v.as_str().unwrap_or("").to_string(),
        Err(e) => return unreachable(&e),
    };
    let mut ids = parse_ids(&listed);
    if !ids.iter().any(|i| i == id) {
        ids.push(id.to_string());
    }
    // settingsd makes the provider's five keys when its name joins the list (CL-5s).
    if let Err(e) = settings::set(s, "ai.providers", ids.join(", "), SOURCE) {
        eprintln!("assist: Der Anbieter „{id}“ ließ sich nicht anlegen ({e}).");
        return 1;
    }
    println!(
        "Der Anbieter „{id}“ ist angelegt. Jetzt fehlen noch:\n\
  settings set ai.provider.{id}.kind ART          anthropic | openai | gemini | openai_compatible | program\n\
  settings set ai.provider.{id}.address ADRESSE\n\
  settings set ai.provider.{id}.model MODELL\n\
  settings set ai.provider.{id}.place ORT         cloud | local\n\
  assist --key {id}                               (nur wenn der Anbieter einen Schlüssel verlangt)\n\
  settings set ai.role.chat {id}                  damit er im Gespräch antwortet"
    );
    0
}

fn key(s: &dyn Settings, id: &str) -> i32 {
    let cfg = match config::load(s) {
        Ok(c) => c,
        Err(e) => return unreachable(&e),
    };
    if !cfg.ids.iter().any(|i| i == id) {
        eprintln!("assist: Der Anbieter „{id}“ ist nicht eingerichtet. Anlegen: assist --add {id}");
        return 2;
    }
    // The key is entered for the provider as it is now: settingsd lets nobody change where a stored key goes
    // (its redirect rule), so Georg reads here where it will go, and the key is stored only if that is
    // still so when he has typed it.
    let seen: std::collections::BTreeMap<String, Json> = match settings::list(s, &format!("ai.provider.{id}")) {
        Ok(all) => ["address", "kind", "place"].iter().filter_map(|f| all.get_key_value(&format!("ai.provider.{id}.{f}")).map(|(k, v)| (k.clone(), v.clone()))).collect(),
        Err(e) => return unreachable(&e),
    };
    let now = |f: &str| seen.get(&format!("ai.provider.{id}.{f}")).and_then(Json::as_str).unwrap_or("").trim().to_string();
    if now("kind") == "program" {
        eprintln!("assist: „{id}“ ist ein Hilfsprogramm (Art program). Dafür gibt es keinen Schlüssel; BitsOS würde ihn dem Programm nicht geben.");
        return 2;
    }
    let missing: Vec<&str> = [("Art (kind)", "kind"), ("Adresse (address)", "address"), ("Ort (place)", "place")].iter().filter(|(_, f)| matches!(now(f).as_str(), "" | "unset")).map(|(de, _)| *de).collect();
    if !missing.is_empty() {
        eprintln!(
            "assist: Beim Anbieter „{id}“ fehlt noch: {}. Trag das zuerst ein (settings set ai.provider.{id}.…), danach den Schlüssel: er gilt nur für genau diese Angaben.",
            missing.join(", ")
        );
        return 2;
    }
    eprintln!(
        "Der Schlüssel für „{id}“ wird nur hierhin geschickt: {} (Art {}, Ort {}).\nSpäter lassen sich Adresse, Art und Ort nur ändern, wenn der Schlüssel vorher gelöscht und danach neu eingetragen wird.",
        now("address"),
        now("kind"),
        now("place")
    );
    let mut line = String::new();
    {
        let _hidden = sys::is_tty(0).then(|| {
            eprint!("Schlüssel für „{id}“ (wird nicht angezeigt, Eingabe beendet): ");
            sys::no_echo()
        });
        if std::io::stdin().read_line(&mut line).is_err() {
            line.clear();
        }
    }
    if sys::is_tty(0) {
        eprintln!();
    }
    let value = line.trim();
    if value.is_empty() {
        eprintln!("assist: Nichts eingegeben, der Schlüssel bleibt wie er war.");
        return 1;
    }
    match settings::set_while(s, &format!("ai.provider.{id}.key"), value, SOURCE, &seen) {
        Ok(_) => {
            println!("Der Schlüssel für „{id}“ ist gespeichert.");
            0
        }
        Err(e) if e.code == "changed_meanwhile" => {
            eprintln!("assist: Adresse, Art oder Ort von „{id}“ wurden während der Eingabe geändert. Der Schlüssel ist nicht gespeichert. Sieh nach (assist --providers) und trag ihn dann neu ein.");
            1
        }
        Err(e) => {
            eprintln!("assist: Der Schlüssel ließ sich nicht speichern ({}).", e.code);
            1
        }
    }
}

/// `assist --mcp`: stdout carries the protocol and nothing else; what a person should know goes to stderr,
/// which a client keeps as this server's log.
fn serve_mcp(settings: &dyn Settings, tools: &dyn ToolSource, read_only: bool, bell: Option<&str>) -> i32 {
    let bell = bell.map(mcp::bell::Bell::at);
    let owner = bell.as_ref().map(|d| d as &dyn mcp::gate::Owner);
    let (note, log) = tools.greeting();
    for line in [note, log].into_iter().flatten() {
        eprintln!("assist --mcp: {}", wrap::clean(&line));
    }
    match mcp::serve(std::io::stdin(), std::io::stdout(), &Gate { settings, tools, read_only, owner }, &Opts::new(read_only)) {
        Ok(End::Closed) => 0,
        Ok(End::Stuck) => {
            eprintln!("assist --mcp: Das andere Programm nimmt die Antworten nicht ab. Beendet.");
            1
        }
        Err(e) => {
            eprintln!("assist --mcp: {e}");
            1
        }
    }
}

pub fn main(args: &[String]) -> i32 {
    let mut socket = None;
    let mut log = None;
    let mut bell = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut it = args.iter().map(String::as_str);
    while let Some(a) = it.next() {
        match a {
            "--socket" => socket = it.next(),
            "--log" => log = it.next(),
            "--bell" => bell = it.next(),
            other => rest.push(other),
        }
    }
    let budget = limits::budget();
    let sock = Socket::new(socket, budget);
    let env = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    let with_machine = |settings| {
        let place = local::place(socket.is_some() || env("SETTINGS_SOCKET").is_some(), env("SSL_CERT_FILE").is_some(), env("TOOLS_SOCKET").map(Into::into));
        // Before the first word to the door: it admits this program only with its memory closed.
        if place == Place::Door {
            sys::close_memory();
        }
        Tools::start(settings, place, budget)
    };
    match rest.as_slice() {
        [] => {
            let log = match log.map(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).map_err(|e| (p, e))) {
                None => None,
                Some(Ok(f)) => Some(f),
                Some(Err((p, e))) => {
                    eprintln!("assist: {p}: {e}");
                    return 2;
                }
            };
            chat::run(&sock, &with_machine(&sock).with_native("console"), budget, log)
        }
        ["--once", text] => once::run(&sock, &with_machine(&sock).with_native("console"), budget, text),
        ["--test", id] => {
            // The settings come out of the test's one wait as well.
            let until = std::time::Instant::now() + probe::PATIENCE;
            let sock = Socket::new(socket, budget).until(until);
            probe::run(&sock, &SettingsTools { settings: &sock }, budget, id, until)
        }
        ["--mcp", more @ ..] | [more @ .., "--mcp"] if matches!(more, [] | ["--read-only"]) => {
            let named = Named(&sock);
            serve_mcp(&named, &with_machine(&named), !more.is_empty(), bell)
        }
        ["--ask"] if bell.is_some() => mcp::bell::relay(bell.unwrap_or_default()),
        ["--call", "--tool-socket", path] => mcp::bell::call_relay(path),
        ["serve", more @ ..] if matches!(more, [] | ["--listen", _]) => {
            let place = local::place(socket.is_some() || env("SETTINGS_SOCKET").is_some(), env("SSL_CERT_FILE").is_some(), env("TOOLS_SOCKET").map(Into::into));
            serve::main(std::sync::Arc::new(Socket::new(socket, budget)), place, more.get(1).copied())
        }
        ["--providers"] => providers(&sock),
        ["--add", id] => add(&sock, id),
        ["--key", id] => key(&sock, id),
        ["--help" | "-h"] => {
            print!("{}", de::USAGE);
            0
        }
        _ => {
            eprint!("{}", de::USAGE);
            2
        }
    }
}
