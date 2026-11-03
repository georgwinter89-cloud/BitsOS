//! RB-18: CONFIG.TXT's secret values in the BootInfo CONFIG record. bitsboot hands the whole file over and the
//! record is never freed, so `boot::set_boot_info` blanks the values right after `config::init` took them (spaces
//! in place: the record keeps its size and every other line). A Reload gets them back from the stick
//! (`refresh`); only what the running kernel holds in clear for its whole life (the MCP token) can also be put
//! back from there when the stick does not answer ([`refill`]). No function here logs a value.

use crate::refresh::effective;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use fs_fat::protect::SECRET_KEYS;
use zeroize::Zeroize;

/// Whether `name` is a secret, read as loosely as the loosest reader: any case, a BOM before it. The one answer
/// for the kernel's `config::is_secret` too; the list itself is fs-fat's, the file tools redact by it.
pub fn is_secret(name: &str) -> bool {
    let name = name.trim().trim_start_matches('\u{feff}').trim();
    SECRET_KEYS.iter().any(|k| k.eq_ignore_ascii_case(name))
}

/// Where the secret value in `line` starts. Every `#` piece counts: a passphrase kept in a comment is still one. So
/// does a piece behind a bare `\r` (an old Mac file): no parser reads it as a line, but it is the file's passphrase.
fn value_start(line: &[u8]) -> Option<usize> {
    let mut at = 0;
    for piece in line.split(|&b| b == b'#' || b == b'\r') {
        if let Some(eq) = piece.iter().position(|&b| b == b'=')
            && is_secret(&String::from_utf8_lossy(&piece[..eq]))
        {
            return Some(at + eq + 1);
        }
        at += piece.len() + 1;
    }
    None
}

/// The byte ranges of every secret value in `text`: from behind the `=` to the line's end (a `#` in a passphrase
/// ends the value for the parsers, not for the blanking), a `\r` stays. Empty values have no range.
pub fn value_ranges(text: &[u8]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in text.split(|&b| b == b'\n') {
        if let Some(v) = value_start(line) {
            let end = line.len() - line.ends_with(b"\r") as usize;
            if v < end {
                out.push(at + v..at + end);
            }
        }
        at += line.len() + 1;
    }
    out
}

/// `text` as `set_boot_info` leaves it.
pub fn blanked(text: &[u8]) -> Vec<u8> {
    let mut out = text.to_vec();
    for r in value_ranges(text) {
        out[r].fill(b' ');
    }
    out
}

/// Whether `text` still holds a secret value (anything but white space where a value would be).
pub fn holds_value(text: &[u8]) -> bool {
    value_ranges(text).into_iter().any(|r| text[r].iter().any(|b| !b.is_ascii_whitespace()))
}

/// Where the last line the kernel's parser reads as `key` has its value (`#` comment, trimmed, exact name), up to the
/// line's end.
fn last_value(text: &[u8], key: &str) -> Option<Range<usize>> {
    let mut found = None;
    let mut at = 0;
    for line in text.split(|&b| b == b'\n') {
        let code = line.split(|&b| b == b'#').next().unwrap_or(b"");
        if let Some(eq) = code.iter().position(|&b| b == b'=')
            && String::from_utf8_lossy(&code[..eq]).trim() == key
        {
            found = Some(at + eq + 1..at + line.len() - line.ends_with(b"\r") as usize);
        }
        at += line.len() + 1;
    }
    found
}

/// The value `key` has as blanks.
fn last_blank(text: &[u8], key: &str) -> Option<Range<usize>> {
    last_value(text, key).filter(|r| text[r.clone()].iter().all(u8::is_ascii_whitespace))
}

/// RB-18: the text for the next kernel when no file can be handed on (the stick did not answer, safe mode, a file
/// too large for the KEEP block): the blanked text with what the running kernel still holds (`live`) written back
/// over its blanks. In place, because a blank is as long as what it replaced: the text keeps its size, so it fits
/// wherever the blanked one did. `None`: nothing to write back. A take-once secret is gone by then (rtw89 keeps
/// only its PMK): [`missing`] names it.
pub fn refill(boot: &[u8], live: &dyn Fn(&str) -> Option<String>) -> Option<Vec<u8>> {
    let mut out = None;
    for k in SECRET_KEYS.iter().copied() {
        let Some(blank) = last_blank(boot, k) else { continue };
        let Some(v) = live(k) else { continue };
        let mut v = v.into_bytes();
        if !v.is_empty() && v.len() <= blank.len() {
            let text = out.get_or_insert_with(|| boot.to_vec());
            text[blank.start..blank.start + v.len()].copy_from_slice(&v);
        }
        v.zeroize();
    }
    out
}

/// The secret keys the new kernel starts without: `boot` gave them a value (blanks are as long as it was, an empty
/// line never had one) and `handed` gives none. Not `claude_api_key`: nothing reads it.
pub fn missing(boot: &[u8], handed: &[u8]) -> Vec<&'static str> {
    let has = |k: &str| effective(handed, k).is_some_and(|v| !v.is_empty());
    let had = |k: &str| k != "claude_api_key" && last_value(boot, k).is_some_and(|r| !r.is_empty());
    SECRET_KEYS.iter().copied().filter(|k| had(k) && !has(k)).collect()
}

/// Whether a Reload that loses `lost` must be refused without `force`: it may take the machine off the network, with
/// nobody at it. The token is MCP's way in; the passphrase counts where WLAN is on.
pub fn loss_blocks_reload(lost: &[&str], wlan_on: bool) -> bool {
    lost.iter().any(|k| *k != "wlan_psk" || wlan_on)
}
