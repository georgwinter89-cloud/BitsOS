//! CL-5: where a provider's key will go, said in full before it is typed. The page's answer line and the
//! address field cut a long text with "…"; a key must never be typed for an address Georg could not read to its
//! end. So the destination comes in a dialog whose lines are broken at its width, the machine's name on a line
//! of its own, and when it cannot be shown whole the field does not open.

use crate::ki::{PROGRAM, Provider, UNSET, kind_name};

/// More lines than this the page's dialog does not draw (`page_confirm::boxes`).
pub const LINES_MAX: usize = 16;

/// The machine an address names, read as assist reads it (its net/url.rs: `http(s)://host[:port][/base]`, no
/// user part, query or fragment, so nothing before an `@` can pass as the machine). Err: the German reason.
pub fn host_of(address: &str) -> Result<String, String> {
    let s = address.trim();
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).ok_or("sie muss mit http:// oder https:// beginnen")?;
    if rest.contains(['?', '#', '@']) || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("sie darf kein ?, #, @ und keine Leerzeichen enthalten".into());
    }
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(r) => r.split_once(']').ok_or("die eckige Klammer ist nicht geschlossen")?.0,
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    if host.is_empty() {
        return Err("in ihr fehlt der Name des Rechners".into());
    }
    Ok(host.to_string())
}

/// Why no key can be typed for `p` yet (Ok: address, kind and place are set and it takes a key).
pub fn key_ready(p: &Provider) -> Result<(), String> {
    if p.kind == PROGRAM {
        return Err(format!("„{}“ ist ein Hilfsprogramm: es braucht keinen Schlüssel.", p.id));
    }
    let missing: Vec<&str> =
        [("die Art", p.kind == UNSET), ("der Ort", p.place == UNSET), ("die Adresse", p.address.is_empty())].iter().filter(|m| m.1).map(|m| m.0).collect();
    if !missing.is_empty() {
        return Err(format!("Erst eintragen: {}. Der Schlüssel gilt nur für Adresse, Art und Ort, wie sie dann sind.", missing.join(", ")));
    }
    Ok(())
}

/// The dialog's lines before the key of `p` is typed: the machine, the whole address, kind and place. No line
/// is wider than `max` by `width` (a long address is broken, never cut). Err: the German reason the field
/// stays closed (nothing to send a key to yet, no address assist would use, or more lines than the dialog draws).
pub fn key_lines(p: &Provider, max: i32, width: impl Fn(&str) -> i32) -> Result<Vec<String>, String> {
    key_ready(p)?;
    let host = host_of(&p.address).map_err(|why| {
        let next = crate::ki_kinds::address_advice(&p.kind, &p.address, false).unwrap_or_else(|| format!("Die Adresse passt nicht: {why}."));
        format!("Erst die Adresse von „{}“ richtig eintragen, dann den Schlüssel. {next}", p.id)
    })?;
    let parts = [
        "Der Schlüssel geht an diesen Rechner:".to_string(),
        host,
        "Die ganze Adresse:".into(),
        p.address.trim().to_string(),
        format!("Art {}, Ort {}.", kind_name(&p.kind), kind_name(&p.place)),
        "Stimmt der Rechner nicht: Abbrechen.".into(),
    ];
    let lines: Vec<String> = parts.iter().flat_map(|t| crate::install_dialog::wrap(t, max, &width)).collect();
    if lines.len() > LINES_MAX || lines.iter().any(|l| width(l) > max) {
        return Err(format!(
            "Die Adresse von „{0}“ ist zu lang, um sie hier ganz zu zeigen (ein breiteres Fenster hilft vielleicht). Sonst den Schlüssel in der Konsole eingeben: assist --key {0}",
            p.id
        ));
    }
    Ok(lines)
}
