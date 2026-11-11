//! GM-2b: the pure part of the stand-in for Valve's `steamrt64/reaper`: how its arguments split, what stderr may say of the
//! words it ignores, and the exit code of a wait status. `main.rs` holds the order of events, `sys.rs` the calls into libc.
#![forbid(unsafe_code)]

use std::ffi::{OsStr, OsString};

/// The first `--` splits the words Valve's reaper ignores from the command, as in its `main`: later ones belong to the
/// command (`-- steam-launch-wrapper -- game`). None when there is no command.
pub fn split(args: &[OsString]) -> Option<(&[OsString], &[OsString])> {
    let at = args.iter().position(|a| a == "--")?;
    let command = &args[at + 1..];
    (!command.is_empty()).then_some((&args[..at], command))
}

/// What stderr says of an ignored word. Steam's log may be read by anyone helping, and Valve's `%s` in
/// `reaper SteamLaunch AppId=%u%s -- ` is not known in full: so `AppId=<digits>` and plain names, never another word's value.
pub fn label(word: &OsStr) -> String {
    const LONGEST: usize = 32;
    let Some(text) = word.to_str() else {
        return format!("a word of {} bytes", word.len());
    };
    let (name, value) = match text.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (text, None),
    };
    let plain = !name.is_empty() && name.len() <= LONGEST && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !plain {
        return format!("a word of {} bytes", word.len());
    }
    match value {
        None => name.to_string(),
        Some(v) if name == "AppId" && !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => text.to_string(),
        Some(_) => format!("{name}=(value not shown)"),
    }
}

/// The reaper's exit code for the command's wait status: its own code, or 128 + the signal as a shell says it.
pub fn exit_code(status: i32) -> i32 {
    match status & 0x7f {
        0 => (status >> 8) & 0xff,
        sig => 128 + sig,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(w: &[&str]) -> Vec<OsString> {
        w.iter().map(OsString::from).collect()
    }

    #[test]
    fn splits_at_the_first_separator() {
        let a = words(&["SteamLaunch", "AppId=480", "--", "wrapper", "--verb=x", "--", "game"]);
        let (ignored, command) = split(&a).unwrap();
        assert_eq!(ignored, &a[..2]);
        assert_eq!(command, &a[3..]);
    }

    #[test]
    fn no_command_is_none() {
        assert!(split(&words(&[])).is_none());
        assert!(split(&words(&["SteamLaunch", "AppId=480"])).is_none());
        assert!(split(&words(&["SteamLaunch", "--"])).is_none());
    }

    #[test]
    fn only_the_exact_word_splits() {
        assert!(split(&words(&["--verb", "---", "game"])).is_none());
    }

    #[test]
    fn labels_name_the_word_never_a_value() {
        assert_eq!(label("SteamLaunch".as_ref()), "SteamLaunch");
        assert_eq!(label("AppId=480".as_ref()), "AppId=480");
        assert_eq!(label("AppId=abc".as_ref()), "AppId=(value not shown)");
        assert_eq!(label("token=abc".as_ref()), "token=(value not shown)");
        assert_eq!(label("=abc".as_ref()), "a word of 4 bytes");
        assert_eq!(label("/some/path".as_ref()), "a word of 10 bytes");
        assert_eq!(label("x".repeat(33).as_ref()), "a word of 33 bytes");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(42 << 8), 42);
        assert_eq!(exit_code(9), 137);
        assert_eq!(exit_code(15 | 0x80), 143);
    }
}
