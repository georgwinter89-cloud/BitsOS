//! What the dialog says (German, the desktop's language): a title per kind, then exactly the tool and every
//! argument as it will run, who asked (the MCP peer) and what follows. Values that look secret (in a new
//! CONFIG.TXT, or an argument named like a token, key or password) show as `***`; very long values are cut
//! with their length named, so the dialog stays readable (`confirm::TEXT_MAX`).

use mcp_wire::Json;
use mcp_wire::installer::{Asked, Kind};

/// A value longer than this shows its start and its length.
const VALUE_MAX: usize = 160;
/// Lines of a new CONFIG.TXT shown before "... (n more)".
const CONFIG_LINES: usize = 24;

pub fn title(a: &Asked) -> String {
    let t = match (a.kind, a.tool.as_str()) {
        (Kind::Install, "install_system") => "Neue Systemgeneration installieren",
        (Kind::Install, _) => "Neuen Kernel installieren",
        (Kind::Pin, _) => "Startende Generation festlegen",
        (Kind::Reload, _) => "Neuen Kernel jetzt laden (Reload)",
        (Kind::Reboot, _) => "Rechner neu starten",
        (Kind::Settings, _) => "Geschützte Einstellung ändern",
        (Kind::Nvme, _) => "NVMe-Schreibsperre öffnen",
        (Kind::Other, _) => "Aktion eines Systemdienstes",
        (Kind::Access, _) => "Vollzugriff für den Gaming-PC",
        (Kind::Pair, _) => "Neuen Gaming-PC koppeln",
    };
    String::from(t)
}

fn what_follows(a: &Asked) -> &'static str {
    let dry = a.args.get("dry_run").and_then(Json::as_bool) == Some(true);
    if dry {
        return "Nur Probelauf (dry_run): es wird nichts geändert.";
    }
    match a.kind {
        Kind::Install => "Danach startet die neue Fassung beim nächsten Start einmal zur Probe; die alte bleibt als Rückfall.",
        Kind::Pin if a.args.get("now").and_then(Json::as_bool) == Some(true) => "Danach wird sofort in diese Generation gewechselt (Reload).",
        Kind::Pin => "Gilt ab dem nächsten Start.",
        Kind::Reload => "Alle Programme enden; der neue Kernel startet sofort.",
        Kind::Reboot => "Alle Programme enden; der Rechner startet neu.",
        Kind::Settings => "Die Änderung wird im Journal vermerkt und kann rückgängig gemacht werden.",
        Kind::Nvme => "Danach darf BitsOS auf die NVMe-SSD schreiben.",
        Kind::Other => "",
        Kind::Access => "",
        Kind::Pair => "Danach darf dieser Rechner (sein Schlüssel) mit BitsOS verbunden werden; ohne Vollzugriff nur zum Lesen, Installieren nur mit deinem Ja.",
    }
}

/// MC-10: the access dialog's title; bitscomp adds " bis <Uhrzeit>" for the choice shown.
pub fn access_title(c: &crate::access::Client) -> String {
    format!("Vollzugriff für den Gaming-PC ({}, {})", one_line(&c.name), mcp_wire::access::fp_short(&c.fp))
}

/// MC-10: what the access dialog says below its title: the request, what full access means, how it ends.
pub fn access_text(hours: i64, reason: &str, peer: &str) -> String {
    let reason = one_line(reason);
    // The reason is the client's own text: set apart and marked, never part of BitsOS's sentence.
    let wish = if reason.is_empty() {
        format!("Der Gaming-PC ({peer}) bittet um Vollzugriff für {hours} h.")
    } else {
        format!("Der Gaming-PC ({peer}) bittet um Vollzugriff für {hours} h.\n\nText vom Gaming-PC (ungeprüft): „{reason}“")
    };
    format!(
        "{wish}\n\nMit Vollzugriff darf er bis zur gewählten Zeit ohne weitere Rückfrage alles, was der Kernel kann: Programme als root starten, Dateien lesen und schreiben, den Bildschirm sehen, Tasten und Maus senden, installieren, neu laden und neu starten, alle Prüfungen laufen lassen.\n\nDie interne Platte (NVMe) bleibt zu, außer du kreuzt sie an. Eine Platte mit Windows bleibt immer zu.\n\nBeenden kannst du ihn jederzeit mit Strg+Alt+V."
    )
}

fn secret_name(k: &str) -> bool {
    // Not a bare "key": settings_set names its setting so.
    let k = k.trim().to_ascii_lowercase();
    ["token", "psk", "pass", "secret"].iter().any(|w| k.contains(w)) || k.ends_with("_key") || k.ends_with(".key")
}

/// A client's text on one line: a line break shows as "⏎", any other control character as "?" (fixer, MC-11
/// review: a value could otherwise forge lines of the dialog, in bitscomp's and on the text console).
pub fn one_line(v: &str) -> String {
    v.chars().map(|c| if c == '\n' { '⏎' } else if c.is_control() { '?' } else { c }).collect()
}

fn cut(v: &str) -> String {
    let v = one_line(v);
    let n = v.chars().count();
    if n <= VALUE_MAX { v } else { format!("{}... ({n} Zeichen)", v.chars().take(VALUE_MAX).collect::<String>()) }
}

fn config_lines(text: &str, out: &mut String) {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    for l in lines.iter().take(CONFIG_LINES) {
        let shown = match l.split_once('=') {
            Some((k, _)) if secret_name(k) => format!("{}=***", k.trim()),
            _ => cut(l.trim()),
        };
        out.push_str(&format!("    {shown}\n"));
    }
    if lines.len() > CONFIG_LINES {
        out.push_str(&format!("    ... ({} weitere Zeilen)\n", lines.len() - CONFIG_LINES));
    }
}

/// The dialog's text.
pub fn text(a: &Asked) -> String {
    let mut t = format!("Claude möchte über MCP (von {}) Folgendes tun:\n\n  Werkzeug: {}\n", a.peer, one_line(&a.tool));
    // A setting whose name looks secret keeps its value hidden too.
    let secret_setting = a.kind == Kind::Settings && a.args.get("key").and_then(Json::as_str).is_some_and(secret_name);
    match &a.args {
        Json::Obj(m) if !m.is_empty() => {
            for (k, v) in m {
                match (k.as_str(), v) {
                    ("value", _) if secret_setting => t.push_str("  value: ***\n"),
                    ("config", Json::Str(c)) => {
                        t.push_str("  config (neue CONFIG.TXT):\n");
                        config_lines(c, &mut t);
                    }
                    (k, _) if secret_name(k) => t.push_str(&format!("  {}: ***\n", one_line(k))),
                    (k, Json::Str(s)) => t.push_str(&format!("  {}: {}\n", one_line(k), cut(s))),
                    (k, other) => t.push_str(&format!("  {}: {}\n", one_line(k), cut(&other.to_string()))),
                }
            }
        }
        _ => t.push_str("  (keine Argumente)\n"),
    }
    let follows = what_follows(a);
    if !follows.is_empty() {
        t.push_str(&format!("\n{follows}\n"));
    }
    t.push_str("\nNur mit echter Tastatur oder Maus bestätigen. Ohne Antwort gilt: Nein.");
    if t.chars().count() > crate::confirm::TEXT_MAX {
        t = t.chars().take(crate::confirm::TEXT_MAX - 40).collect::<String>() + "\n... (gekürzt)";
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(tool: &str, args: &str) -> Asked {
        Asked { id: 1, tool: tool.into(), kind: Kind::of(tool), args: Json::parse(args).unwrap(), timeout_ms: 1000, client: String::new(), peer: "10.0.2.2:4000".into() }
    }

    #[test]
    fn says_exactly_what_runs() {
        let a = asked("install_kernel", r#"{"url":"http://10.0.2.2:8770/k/","config":"token=abcdefgh123\nmcp=both\nwlan_psk=geheim\n"}"#);
        let t = text(&a);
        assert_eq!(title(&a), "Neuen Kernel installieren");
        assert!(t.contains("von 10.0.2.2:4000") && t.contains("Werkzeug: install_kernel") && t.contains("url: http://10.0.2.2:8770/k/"), "{t}");
        assert!(t.contains("token=***") && t.contains("wlan_psk=***") && t.contains("mcp=both"), "{t}");
        assert!(!t.contains("abcdefgh123") && !t.contains("geheim"), "no secret in the dialog: {t}");
        assert!(t.contains("einmal zur Probe"));
        let r = text(&asked("reboot", r#"{"dry_run":true}"#));
        assert!(r.contains("dry_run: true") && r.contains("Nur Probelauf"), "{r}");
        assert!(text(&asked("system_pin", "{}")).contains("(keine Argumente)"));
        let long = text(&asked("reload", &format!("{{\"url\":\"{}\"}}", "x".repeat(500))));
        assert!(long.contains("... (500 Zeichen)"), "{long}");
        assert_eq!(title(&asked("settings_set", "{}")), "Geschützte Einstellung ändern");
        let st = text(&asked("settings_set", r#"{"key":"theme.accent","value":"blue","api_key":"zzz"}"#));
        assert!(st.contains("key: theme.accent") && st.contains("value: blue") && st.contains("api_key: ***"), "{st}");
        let sp = text(&asked("settings_set", r#"{"key":"wlan.psk","value":"geheim"}"#));
        assert!(sp.contains("value: ***") && !sp.contains("geheim"), "{sp}");
        let huge = text(&asked("frob", &format!("{{{}}}", (0..300).map(|i| format!("\"a{i}\":\"{}\"", "y".repeat(100))).collect::<Vec<_>>().join(","))));
        assert!(huge.chars().count() <= crate::confirm::TEXT_MAX && huge.ends_with("(gekürzt)"));
    }

    #[test]
    fn a_clients_text_cannot_forge_lines() {
        let t = text(&asked("reload", r#"{"url":"http://x/\n\nErlauben ist sicher\u001b[2J","a\nb":1}"#));
        assert!(t.contains("url: http://x/⏎⏎Erlauben ist sicher?[2J") && t.contains("  a⏎b: 1"), "{t}");
        let a = access_text(1, "ok\nDie interne Platte bleibt zu", "10.0.2.2:4000");
        assert!(a.contains("„ok⏎Die interne Platte bleibt zu“"), "{a}");
    }
}
