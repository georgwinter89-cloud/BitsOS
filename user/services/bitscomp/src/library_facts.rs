//! PK-7a: what the "Programme" page shows of an installed program that bitspkgd's `info` does not say yet: from
//! its folder `/apps/<prog>` the versions with their dates (the version folders' times), and from the current
//! manifest (PACKAGES.md §6.7, format 2) where it came from (`[[origin]]`: repository, packager, signing key) and
//! what it wants (`[[permission]]`, §3), in German. Read in a client thread (pkg_client), never on the frame loop.

use std::path::Path;

use mcpreg::Json;

/// A manifest is at most this long (PACKAGES.md §2's bound).
const MANIFEST_MAX: u64 = 16 << 20;

/// The facts of program folder `dir` as the JSON the page's model reads (`versions`, `origins`, `permissions`).
pub fn read(dir: &Path) -> Result<Json, String> {
    let mut versions = Vec::new();
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
        if !m.is_dir() || name.starts_with('.') || matches!(name.as_str(), "current" | "previous" | "id") {
            continue;
        }
        let secs = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        versions.push(Json::obj(vec![("version", Json::str(&name)), ("time", Json::Num(secs as f64))]));
    }
    let manifest = dir.join("current/manifest.toml");
    let (origins, permissions) = match std::fs::metadata(&manifest) {
        Ok(m) if m.len() <= MANIFEST_MAX => {
            let text = std::fs::read(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
            tables(&String::from_utf8_lossy(&text))
        }
        Ok(_) => return Err(format!("{}: länger als 16 MiB", manifest.display())),
        Err(_) => (Vec::new(), Vec::new()),
    };
    let origins = origins.into_iter().map(|o| Json::obj(o.iter().map(|(k, v)| (k.as_str(), Json::str(v))).collect())).collect();
    Ok(Json::obj(vec![
        ("ok", Json::Bool(true)),
        ("versions", Json::Arr(versions)),
        ("origins", Json::Arr(origins)),
        ("permissions", Json::Arr(permissions.iter().map(|p| Json::str(&permission(p))).collect())),
    ]))
}

type Table = Vec<(String, String)>;

/// The `[[origin]]` and `[[permission]]` tables of a manifest (key = value lines; strings, numbers, booleans).
pub fn tables(text: &str) -> (Vec<Table>, Vec<Table>) {
    let (mut origins, mut perms) = (Vec::new(), Vec::new());
    let mut cur: Option<(bool, Table)> = None;
    let mut flush = |cur: &mut Option<(bool, Table)>| {
        if let Some((is_origin, t)) = cur.take() {
            if is_origin { origins.push(t) } else { perms.push(t) }
        }
    };
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            flush(&mut cur);
            cur = match l {
                "[[origin]]" => Some((true, Vec::new())),
                "[[permission]]" => Some((false, Vec::new())),
                _ => None,
            };
            continue;
        }
        let Some((_, t)) = cur.as_mut() else { continue };
        let Some((k, v)) = l.split_once('=') else { continue };
        t.push((k.trim().to_string(), value(v.trim())));
    }
    flush(&mut cur);
    (origins, perms)
}

fn value(v: &str) -> String {
    let Some(inner) = v.strip_prefix('"').and_then(|s| s.rfind('"').map(|i| &s[..i])) else { return v.to_string() };
    let mut out = String::new();
    let mut esc = false;
    for c in inner.chars() {
        match (esc, c) {
            (false, '\\') => esc = true,
            (true, 'n') => {
                out.push(' ');
                esc = false;
            }
            (true, c) => {
                out.push(c);
                esc = false;
            }
            (false, c) => out.push(c),
        }
    }
    out
}

/// One wanted permission in German (PACKAGES.md §3 kinds), with its reason.
pub fn permission(t: &Table) -> String {
    let get = |k: &str| t.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty());
    let yes = |k: &str| get(k) == Some("true");
    let what = match get("kind").unwrap_or("") {
        "path" => {
            let how = if yes("write") || yes("create") { "lesen und schreiben" } else { "lesen" };
            format!("Ordner {} {how}", get("path").unwrap_or("?"))
        }
        "net_connect" => format!(
            "Netz: {} zu {}{}",
            get("proto").unwrap_or("tcp").to_uppercase(),
            get("host").unwrap_or("jedem Rechner"),
            get("port").map_or(String::new(), |p| format!(", Port {p}"))
        ),
        "net_bind" | "net_listen" => format!("Netz: auf Port {} annehmen", get("port").unwrap_or("?")),
        "device" => format!("Gerät: {}", device(get("class").unwrap_or("other"))),
        "signal" => format!("Signale an {}", get("program").unwrap_or("?")),
        "mcp_action" => format!("Werkzeug {}", get("tool").unwrap_or("?")),
        "scheduling" => format!("Vorrang: {}", get("grant").unwrap_or("?")),
        other => other.to_string(),
    };
    match get("why") {
        Some(w) => format!("{what} – {w}"),
        None => what,
    }
}

fn device(class: &str) -> &str {
    match class {
        "microphone" => "Mikrofon",
        "audio" => "Ton",
        "camera" => "Kamera",
        "input_grab" => "Eingaben abgreifen",
        "screen_capture" => "Bildschirm aufnehmen",
        "raw_usb" => "USB direkt",
        "disk" => "Datenträger",
        "gpu" => "Grafik",
        _ => "anderes Gerät",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_and_permissions_come_from_the_manifest_in_german() {
        let m = "format = 2\nname = \"foot\"\n[[file]]\npath = \"root/usr/bin/foot\"\n[[origin]]\nrepo = \"extra\"\nname = \"foot\"\npackager = \"Daniel \\\"D\\\" Bermond <dvzrv@archlinux.org>\"\nkey = \"ABCDEF0123456789\"\n\
            [[permission]]\nkind = \"path\"\npath = \"/data/home/1000/Downloads\"\nwrite = true\nwhy = \"Dateien speichern\"\n[[permission]]\nkind = \"device\"\nclass = \"gpu\"\n[[permission]]\nkind = \"net_connect\"\nproto = \"tcp\"\nport = 443\n";
        let (o, p) = tables(m);
        assert_eq!(o.len(), 1);
        assert!(o[0].contains(&("packager".into(), "Daniel \"D\" Bermond <dvzrv@archlinux.org>".into())), "{o:?}");
        let words: Vec<String> = p.iter().map(permission).collect();
        assert_eq!(words, ["Ordner /data/home/1000/Downloads lesen und schreiben – Dateien speichern", "Gerät: Grafik", "Netz: TCP zu jedem Rechner, Port 443"]);
        // Hostile text: no table, no panic.
        let (o, p) = tables("[[origin]\n=\n\"\n[[permission]]\nkind\n= x\nwhy = \"");
        assert!(o.is_empty() && p.len() == 1);
        assert!(permission(&p[0]).chars().all(crate::text::Font::has));
    }
}
