//! WY-1b: the terminal bitscomp starts (Strg+Alt+Enter, circle menu "Neues Terminal") and the environment its
//! started programs get on V2, where init and MCP `run` set little (HOME=/, PATH, no locale, no SHELL).
//!
//! foot from CachyOS: the font is named, since V2 ships one monospace family (Noto Sans Mono) and no fontconfig
//! aliases; 11 pt with foot's default `dpi-aware=no` is sized by the output scale (1.25 on the Mini-PC's
//! 2560x1440: 18 px glyphs, rows about 26 px). V2 has no utempter (and no utmp), so foot is told not to look for it.

use std::path::{Path, PathBuf};

pub const TERMINAL: &[&str] = &[
    "foot",
    "--font=Noto Sans Mono:size=11",
    "--override=main.utmp-helper=none",
];

/// GPY: the exec-time form of a `--font=` naming V2's shipped family: the symbol fonts appended as foot's fallbacks
/// (foot.ini(5): the fonts after the first are used for glyphs the first lacks) at its size, so Claude Code's ✳ ❯ ✻ ⏵
/// ⚠ ⎿ are drawn. Only the exec sees it, as with `--config=`: a saved session keeps the plain argument and gets the
/// fallbacks of the day it is restored. Another family or a list of the user's own stays as it is.
pub fn with_symbols(arg: &str) -> Option<String> {
    let spec = arg.strip_prefix("--font=")?;
    if spec.contains(',') {
        return None;
    }
    let (family, opts) = spec.split_once(':').unwrap_or((spec, ""));
    if family.trim() != "Noto Sans Mono" {
        return None;
    }
    let size: String =
        opts.split(':').filter(|o| o.starts_with("size=") || o.starts_with("pixelsize=")).map(|o| format!(":{o}")).collect();
    let mut s = arg.to_string();
    for (name, _) in crate::symbols::FONTS {
        s.push_str(&format!(",{name}{size}"));
    }
    Some(s)
}

/// Variables set for started programs where bitscomp's own environment lacks them (`get` reads it, `writable`
/// says whether a directory can take files). A variable that is set is never changed.
///
/// - SHELL: foot runs `$SHELL`, else the passwd entry's shell; V2's is BusyBox `/bin/sh`.
/// - LC_CTYPE=C.UTF-8 when no locale variable is set: glibc's "C" is ASCII only, foot then warns and falls back
///   itself, and the shell would not see umlauts as characters. The image ships /usr/lib/locale/C.utf8.
/// - XDG_CACHE_HOME=<run dir>/cache when $HOME/.cache cannot be written (V2: HOME=/ for uid 1000): fontconfig
///   writes its cache there, else it rescans the fonts at every start and prints "No writable cache directories".
pub fn child_env(
    get: impl Fn(&str) -> Option<String>,
    writable: impl Fn(&Path) -> bool,
    rundir: &Path,
) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let set = |k: &str| get(k).is_some_and(|s| !s.is_empty());
    if !set("SHELL") {
        v.push(("SHELL".into(), "/bin/sh".into()));
    }
    if !set("LC_ALL") && !set("LC_CTYPE") && !set("LANG") {
        v.push(("LC_CTYPE".into(), "C.UTF-8".into()));
    }
    if !set("XDG_CACHE_HOME") {
        let home = get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let ok = home.is_some_and(|h| {
            let c = h.join(".cache");
            if c.is_dir() { writable(&c) } else { writable(&h) }
        });
        if !ok {
            v.push(("XDG_CACHE_HOME".into(), rundir.join("cache").display().to_string()));
        }
    }
    v
}

/// WY-11: the pointer theme for started programs (their own cursors, e.g. a program that ignores
/// wp_cursor_shape_v1): XCURSOR_THEME=Adwaita (the theme the image ships) unless bitscomp's environment names one,
/// and XCURSOR_SIZE always theme.cursor.size (bitscomp's setting decides, also when it changes: `cursor_size`).
pub fn cursor_env(get: impl Fn(&str) -> Option<String>, env: &mut Vec<(String, String)>, size: u32) {
    if get("XCURSOR_THEME").is_none_or(|s| s.is_empty()) && !env.iter().any(|(k, _)| k == "XCURSOR_THEME") {
        env.push(("XCURSOR_THEME".into(), crate::cursor_theme::THEME.into()));
    }
    env.retain(|(k, _)| k != "XCURSOR_SIZE");
    env.push(("XCURSOR_SIZE".into(), size.to_string()));
}

/// Whether this process may create files in `d` (access(2) with W_OK|X_OK).
pub fn writable(d: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(d.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated string that outlives the call; access only reads it.
    unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) == 0 }
}

/// The environment for this process: `child_env` over std::env, and the cache directory made (0700) when used.
pub fn prepare(rundir: &Path) -> Vec<(String, String)> {
    let v = child_env(|k| std::env::var(k).ok(), |d| d.is_dir() && writable(d), rundir);
    if let Some((_, dir)) = v.iter().find(|(k, _)| k == "XDG_CACHE_HOME") {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(e) = std::fs::DirBuilder::new().mode(0o700).create(dir)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            eprintln!("bitscomp: cache dir {dir}: {e} (fontconfig rescans at every start)");
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn v2_defaults_fill_what_is_missing() {
        let e = env(&[("HOME", "/"), ("PATH", "/bin")]);
        let v = child_env(|k| e.get(k).cloned(), |_| false, Path::new("/tmp/bitscomp-1000"));
        assert_eq!(
            v,
            vec![
                ("SHELL".to_string(), "/bin/sh".to_string()),
                ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
                ("XDG_CACHE_HOME".to_string(), "/tmp/bitscomp-1000/cache".to_string()),
            ]
        );
    }

    #[test]
    fn set_variables_are_kept() {
        let e = env(&[("HOME", "/nonexistent-home"), ("SHELL", "/bin/zsh"), ("LANG", "de_DE.UTF-8"), ("XDG_CACHE_HOME", "/c")]);
        assert!(child_env(|k| e.get(k).cloned(), |_| false, Path::new("/r")).is_empty());
        // A writable home keeps fontconfig's own cache place.
        let e = env(&[("HOME", "/nonexistent-home"), ("SHELL", "/bin/sh"), ("LC_ALL", "C.UTF-8")]);
        assert!(child_env(|k| e.get(k).cloned(), |_| true, Path::new("/r")).is_empty());
        // No HOME at all: the run dir's cache.
        let e = env(&[("SHELL", "/bin/sh"), ("LC_CTYPE", "C.UTF-8")]);
        assert_eq!(child_env(|k| e.get(k).cloned(), |_| true, Path::new("/r")), vec![("XDG_CACHE_HOME".into(), "/r/cache".into())]);
    }

    #[test]
    fn cursor_theme_and_size_for_programs() {
        let mut v = Vec::new();
        cursor_env(|_| None, &mut v, 32);
        assert_eq!(v, vec![("XCURSOR_THEME".into(), "Adwaita".into()), ("XCURSOR_SIZE".into(), "32".into())]);
        // A size change replaces it; a theme of bitscomp's own environment is kept (not added).
        cursor_env(|_| None, &mut v, 48);
        assert_eq!(v.iter().filter(|(k, _)| k.starts_with("XCURSOR")).count(), 2);
        assert_eq!(v.last(), Some(&("XCURSOR_SIZE".to_string(), "48".to_string())));
        let mut w = Vec::new();
        cursor_env(|k| (k == "XCURSOR_THEME").then(|| "breeze".to_string()), &mut w, 24);
        assert_eq!(w, vec![("XCURSOR_SIZE".into(), "24".into())]);
    }

    #[test]
    fn the_shipped_font_gets_the_symbol_fonts_at_its_size() {
        assert_eq!(
            with_symbols("--font=Noto Sans Mono:size=11").as_deref(),
            Some("--font=Noto Sans Mono:size=11,Noto Sans Symbols 2:size=11,Noto Sans Symbols:size=11,DejaVu Sans Mono:size=11")
        );
        assert_eq!(
            with_symbols("--font=Noto Sans Mono:weight=bold:pixelsize=20").as_deref(),
            Some("--font=Noto Sans Mono:weight=bold:pixelsize=20,Noto Sans Symbols 2:pixelsize=20,Noto Sans Symbols:pixelsize=20,DejaVu Sans Mono:pixelsize=20")
        );
        assert_eq!(
            with_symbols("--font=Noto Sans Mono").as_deref(),
            Some("--font=Noto Sans Mono,Noto Sans Symbols 2,Noto Sans Symbols,DejaVu Sans Mono")
        );
        // The user's own choice: another family, an own list, not a font argument.
        for a in ["--font=Hack:size=12", "--font=Noto Sans Mono:size=11,Twemoji", "--font", "-fNoto Sans Mono", "--override=x"] {
            assert_eq!(with_symbols(a), None, "{a}");
        }
        assert!(TERMINAL.iter().any(|a| with_symbols(a).is_some()), "bitscomp's own terminal gets them");
    }

    #[test]
    fn terminal_is_foot_with_the_shipped_font() {
        assert_eq!(TERMINAL[0], "foot");
        assert!(TERMINAL.iter().any(|a| a.starts_with("--font=Noto Sans Mono")));
    }
}
