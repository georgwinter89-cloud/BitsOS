//! alpm hook files (pacman 7.1 doc/alpm-hooks.5.asciidoc; parser lib/libalpm/hook.c:126 _alpm_hook_parse_cb,
//! matching :303 _alpm_hook_trigger_match_file, :358 _alpm_hook_trigger_match_pkg, util.c:1528
//! _alpm_fnmatch_patterns). BitsOS never runs a hook: trigger.rs emulates the known ones inside the program's
//! staging view. Here only what a hook says and whether an install of the closure would set it off: every closure
//! package counts as Operation=Install (a new program version is a fresh tree), Path targets are matched against
//! the packages' file lists (folders with a trailing `/`, as pacman's lists have them).

use super::pkgs::List;

/// One hook file is a few hundred bytes; this bounds a hostile one.
pub const HOOK_MAX: usize = 64 << 10;
pub const HOOK_DIR: &str = "usr/share/libalpm/hooks/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Path,
    Package,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Trigger {
    pub install: bool,
    pub upgrade: bool,
    pub remove: bool,
    pub ty: Type,
    pub targets: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hook {
    /// The file name without `.hook` (`11-glibc-ldconfig`).
    pub name: String,
    pub triggers: Vec<Trigger>,
    pub post: bool,
    pub exec: String,
    pub needs_targets: bool,
    /// hook.c:660: a failing PreTransaction hook with AbortOnFail stops pacman's transaction. BitsOS runs no
    /// PreTransaction hook, so the report says where pacman would have stopped (trigger.rs).
    pub abort_on_fail: bool,
    pub depends: Vec<String>,
    pub description: String,
}

/// hook.c's ini rules: `[Trigger]`/`[Action]` sections, `key = value`, a key without a value for the flags;
/// unknown keys are an error there too (hook.c:204 "hook %s line %d: invalid option").
pub fn parse(name: &str, text: &str) -> Result<Hook, String> {
    if text.len() > HOOK_MAX {
        return Err(format!("{name}: größer als {HOOK_MAX} Bytes"));
    }
    let mut h = Hook { name: name.to_string(), triggers: Vec::new(), post: false, exec: String::new(), needs_targets: false, abort_on_fail: false, depends: Vec::new(), description: String::new() };
    let mut section = "";
    let mut when = None;
    for (n, raw) in text.lines().enumerate() {
        let l = raw.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let bad = |w: &str| Err(format!("{name}.hook Zeile {}: {w}", n + 1));
        if l.starts_with('[') {
            section = match l {
                "[Trigger]" => {
                    h.triggers.push(Trigger { install: false, upgrade: false, remove: false, ty: Type::Path, targets: Vec::new() });
                    "trigger"
                }
                "[Action]" => "action",
                _ => return bad("unbekannter Abschnitt"),
            };
            continue;
        }
        let (k, v) = match l.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (l, ""),
        };
        match (section, k) {
            ("trigger", "Operation") => {
                let t = h.triggers.last_mut().ok_or("kein Trigger")?;
                match v {
                    "Install" => t.install = true,
                    "Upgrade" => t.upgrade = true,
                    "Remove" => t.remove = true,
                    _ => return bad("Operation unbekannt"),
                }
            }
            ("trigger", "Type") => {
                let t = h.triggers.last_mut().ok_or("kein Trigger")?;
                t.ty = match v {
                    "Path" | "File" => Type::Path,
                    "Package" => Type::Package,
                    _ => return bad("Type unbekannt"),
                };
            }
            ("trigger", "Target") => h.triggers.last_mut().ok_or("kein Trigger")?.targets.push(v.to_string()),
            ("action", "When") => {
                when = Some(match v {
                    "PreTransaction" => false,
                    "PostTransaction" => true,
                    _ => return bad("When unbekannt"),
                })
            }
            ("action", "Exec") => h.exec = v.to_string(),
            ("action", "Description") => h.description = v.to_string(),
            ("action", "Depends") => h.depends.push(v.to_string()),
            ("action", "NeedsTargets") => h.needs_targets = true,
            ("action", "AbortOnFail") => h.abort_on_fail = true,
            _ => return bad("unbekannter Schlüssel"),
        }
    }
    // hook.c:96 _alpm_hook_validate: at least one trigger, each with an operation and a target; Exec and When.
    if h.triggers.is_empty() || h.triggers.iter().any(|t| t.targets.is_empty() || !(t.install || t.upgrade || t.remove)) {
        return Err(format!("{name}.hook: ein Trigger ohne Operation oder Ziel"));
    }
    if h.exec.is_empty() {
        return Err(format!("{name}.hook: Exec fehlt"));
    }
    h.post = when.ok_or_else(|| format!("{name}.hook: When fehlt"))?;
    Ok(h)
}

/// fnmatch(3) without flags (util.c:1561 _alpm_fnmatch): `*` and `?` match `/` too; `[...]` with `!`/`^` and
/// ranges; `\` quotes the next character.
pub fn fnmatch(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                b'*' => {
                    star = Some(pi);
                    mark = si;
                    pi += 1;
                    continue;
                }
                b'?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                b'[' => {
                    if let Some((ok, end)) = class(&p[pi..], s[si]) {
                        if ok {
                            pi += end;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == b'[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                b'\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == s[si] {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some(st) => {
                pi = st + 1;
                mark += 1;
                si = mark;
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// A bracket expression at the start of `p`: (matches `c`, its length), or None when it is not closed.
fn class(p: &[u8], c: u8) -> Option<(bool, usize)> {
    let mut i = 1;
    let neg = matches!(p.get(1), Some(b'!' | b'^'));
    if neg {
        i += 1;
    }
    let mut ok = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == b']' && !first {
            return Some((ok != neg, i + 1));
        }
        first = false;
        let lo = p[i];
        if i + 2 < p.len() && p[i + 1] == b'-' && p[i + 2] != b']' {
            ok |= (lo..=p[i + 2]).contains(&c);
            i += 3;
        } else {
            ok |= lo == c;
            i += 1;
        }
    }
    None
}

/// util.c:1528: the last pattern that matches decides; one with `!` means no.
pub fn patterns(targets: &[String], s: &str) -> bool {
    for t in targets.iter().rev() {
        let (inv, pat) = match t.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, t.strip_prefix('\\').unwrap_or(t)),
        };
        if fnmatch(pat.as_bytes(), s.as_bytes()) {
            return !inv;
        }
    }
    false
}

/// What sets the hook off when the closure is installed: the matching files (`Path`) or package names
/// (`Package`), deduplicated; empty when it does not run (hook.c:427 _alpm_hook_triggered with Install only).
pub fn targets(h: &Hook, closure: &[(String, &List)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in h.triggers.iter().filter(|t| t.install) {
        for (name, l) in closure {
            match t.ty {
                Type::Package => {
                    if patterns(&t.targets, name) && !out.contains(name) {
                        out.push(name.clone());
                    }
                }
                Type::Path => {
                    let files = l.files.iter().map(|f| f.path.clone()).chain(l.symlinks.iter().map(|s| s.path.clone()));
                    for p in files.chain(l.dirs.iter().map(|d| format!("{d}/"))) {
                        if patterns(&t.targets, &p) && !out.contains(&p) {
                            out.push(p);
                        }
                    }
                }
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::FileEntry;

    #[test]
    fn fnmatch_as_libc() {
        assert!(fnmatch(b"usr/lib/gio/modules/*.so", b"usr/lib/gio/modules/libdconf.so"));
        assert!(fnmatch(b"usr/share/icons/*/", b"usr/share/icons/hicolor/48x48/"), "no FNM_PATHNAME: * takes /");
        assert!(!fnmatch(b"usr/lib/*.so", b"usr/lib/x.so.1"));
        assert!(fnmatch(b"etc/[a-c]?", b"etc/b1") && !fnmatch(b"etc/[!a-c]?", b"etc/b1"));
        assert!(patterns(&["usr/share/icons/*/".into(), "!usr/share/icons/*/?*".into()], "usr/share/icons/hicolor/"));
        assert!(!patterns(&["usr/share/icons/*/".into(), "!usr/share/icons/*/?*".into()], "usr/share/icons/hicolor/48x48/"));
    }

    #[test]
    fn parses_and_matches_real_hooks() {
        let ld = parse(
            "11-glibc-ldconfig",
            "[Trigger]\nOperation = Install\nOperation = Upgrade\nTarget = glibc\nType = Package\n\n[Trigger]\nOperation = Upgrade\nTarget = etc/ld.so.conf\nType = Path\n\n[Action]\nDepends = glibc\nDescription = Configuring...\nExec = /usr/bin/ldconfig -r .\nWhen = PostTransaction\n",
        )
        .unwrap();
        let mut l = List::default();
        l.files.push(FileEntry { path: "usr/lib/gio/modules/libx.so".into(), sha256: "0".repeat(64), size: 1, exec: false });
        l.dirs.push("usr/share/icons/hicolor".into());
        let closure = vec![("glibc".to_string(), &l)];
        assert_eq!(targets(&ld, &closure), ["glibc"]);
        let gio = parse("gio-querymodules", "[Trigger]\nType = Path\nOperation = Install\nOperation = Remove\nTarget = usr/lib/gio/modules/*.so\n[Action]\nWhen = PostTransaction\nExec = /usr/bin/gio-querymodules /usr/lib/gio/modules\n").unwrap();
        assert_eq!(targets(&gio, &closure), ["usr/lib/gio/modules/libx.so"]);
        let rm = parse("x", "[Trigger]\nType = Path\nOperation = Remove\nTarget = *\n[Action]\nWhen = PreTransaction\nExec = /bin/true\n").unwrap();
        assert!(targets(&rm, &closure).is_empty(), "a Remove-only hook never runs at install");
        assert!(!rm.abort_on_fail && !gio.abort_on_fail);
        let pre = parse("y", "[Trigger]\nType = Package\nOperation = Install\nTarget = glibc\n[Action]\nWhen = PreTransaction\nExec = /bin/false\nAbortOnFail\n").unwrap();
        assert!(pre.abort_on_fail && !pre.post);
        for bad in ["[Action]\nExec = x\nWhen = PostTransaction\n", "[Trigger]\nOperation = Install\nTarget = x\n[Action]\nWhen = PostTransaction\n", "[Trigger]\nFoo = 1\n"] {
            assert!(parse("bad", bad).is_err(), "{bad}");
        }
    }
}
