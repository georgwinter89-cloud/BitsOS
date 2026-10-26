//! R8: the safety switches are never settings keys, and settingsd never adds or changes their CONFIG.TXT lines
//! (V2-PLAN R8a, §8a.3). Kernel secrets (`kernel/src/config.rs` `is_secret`) never enter the registry.
//!
//! `edit_config` rewrites one key's line and then runs the same check FS-7's guard runs on every CONFIG.TXT write
//! (crates/fs-fat/src/protect.rs `config_guard`), so settingsd refuses before the file system would.

/// Only Georg's own hand turns these on (R8a; FS-7 GUARDED_KEYS). `wlan` is free since 21.10. (R8d).
pub const R8_SWITCHES: &[&str] = &["nvme_write", "stick_write", "usb_msd_write"];
/// CONFIG.TXT keys the kernel never shows; they stay out of journal, history and answers.
pub const KERNEL_SECRETS: &[&str] = &["token", "wlan_psk", "claude_api_key"];
/// FS-7's guarded CONFIG keys.
const GUARDED: &[&str] = &["nvme_write", "stick_write", "usb_msd_write"];

fn is_switch(s: &str) -> bool {
    let s = s.trim().trim_start_matches('\u{feff}').to_ascii_lowercase();
    R8_SWITCHES.contains(&s.as_str())
}

/// A key named like a switch (`nvme_write`, `disk.nvme_write`, `kernel.stick_write`) is refused: no key may look like a way to
/// flip one. Longer names (`disk.nvme_write.note`) are fine.
pub fn refuse_key(name: &str) -> Result<(), String> {
    let last = name.rsplit('.').next().unwrap_or(name);
    if is_switch(name) || is_switch(last) {
        return Err(format!("{name}: R8 safety switches are not settings; only Georg's own edit changes them"));
    }
    // The kernel's secrets (`token`, `wlan_psk`, `claude_api_key`) are never keys, under any namespace: no key
    // may look like a way to read or replace one (R8i). A service names its own secret otherwise (`api_key`).
    if KERNEL_SECRETS.contains(&last.trim().to_ascii_lowercase().as_str()) {
        return Err(format!("{name}: kernel secret (R8), never a settings key"));
    }
    Ok(())
}

pub fn refuse_config_name(config: &str) -> Result<(), String> {
    if is_switch(config) {
        return Err(format!("{config}: R8 safety switch, never written by settingsd"));
    }
    if KERNEL_SECRETS.contains(&config.trim().to_ascii_lowercase().as_str()) {
        return Err(format!("{config}: kernel secret, never handled by settingsd"));
    }
    Ok(())
}

/// The key of a CONFIG.TXT line as the kernel reads it (`#` comment, spaces trimmed), or None.
fn line_key(line: &str) -> Option<&str> {
    let l = line.split('#').next().unwrap_or("").trim();
    let (k, _) = l.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()).then_some(k)
}

/// The value the kernel reads for `key` (last line wins), as `kernel/src/config.rs` `parse` does.
pub fn config_value(text: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let l = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = l.split_once('=')
            && k.trim() == key
        {
            found = Some(v.trim().to_string());
        }
    }
    found
}

/// Sets (`Some`) or removes (`None`) `key` in a CONFIG.TXT text. Set replaces the line that counts (the last
/// one) and keeps earlier ones; remove drops every line of the key so no earlier one becomes effective. Every
/// other line stays byte-identical, line ends (LF or CRLF) included.
pub fn edit_config(old: &str, key: &str, value: Option<&str>) -> Result<String, String> {
    refuse_config_name(key)?;
    if let Some(v) = value
        && (v.contains(['\n', '\r', '#']) || v.trim() != v)
    {
        return Err(format!("{key}: a CONFIG.TXT value cannot hold a line break, '#' or outer spaces"));
    }
    let lines: Vec<&str> = old.split_inclusive('\n').collect();
    let crlf = old.contains("\r\n");
    let last = lines.iter().rposition(|l| line_key(l) == Some(key));
    let mut out = String::with_capacity(old.len() + key.len() + 32);
    for (i, l) in lines.iter().enumerate() {
        if line_key(l) != Some(key) {
            out.push_str(l);
            continue;
        }
        match value {
            Some(v) if Some(i) == last => {
                let end = if l.ends_with("\r\n") {
                    "\r\n"
                } else if l.ends_with('\n') {
                    "\n"
                } else {
                    ""
                };
                out.push_str(&format!("{key}={v}{end}"));
            }
            Some(_) => out.push_str(l),
            None => {}
        }
    }
    if let (Some(v), None) = (value, last) {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(if crlf { "\r\n" } else { "\n" });
        }
        out.push_str(&format!("{key}={v}{}", if crlf { "\r\n" } else { "\n" }));
    }
    config_guard(old.as_bytes(), out.as_bytes())
        .map_err(|k| format!("refused: the edit would change the {k} line (R8)"))?;
    Ok(out)
}

/// Port of FS-7's `config_guard`: every guarded line of `new` must already be in `old`, and each guarded key's
/// effective value must stay or go. Err names the key.
pub fn config_guard(old: &[u8], new: &[u8]) -> Result<(), String> {
    let before = guarded_lines(old);
    for line in guarded_lines(new) {
        if !before.contains(&line) {
            return Err(line.0);
        }
    }
    for &k in GUARDED {
        if let Some(v) = effective(new, k)
            && effective(old, k) != Some(v)
        {
            return Err(k.to_string());
        }
    }
    Ok(())
}

fn guarded_lines(text: &[u8]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = text
        .split(|&b| b == b'\n')
        .filter_map(|l| {
            let l = String::from_utf8_lossy(l);
            let l = l.trim().trim_start_matches('\u{feff}');
            let (k, val) = l.split_once('=')?;
            let k = k.trim().to_ascii_lowercase();
            GUARDED.contains(&k.as_str()).then(|| (k, val.trim().to_string()))
        })
        .collect();
    v.sort();
    v
}

fn effective<'a>(text: &'a [u8], key: &str) -> Option<&'a str> {
    let mut found = None;
    for raw in text.split(|&b| b == b'\n') {
        let Ok(line) = std::str::from_utf8(raw) else { continue };
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=')
            && k.trim() == key
        {
            found = Some(v.trim());
        }
    }
    found
}
