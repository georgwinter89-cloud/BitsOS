//! Names a package may use. They end up as folder names under `/apps`, as settings namespaces (`app.<name>`) and
//! as MCP tool names (`app.<name>.<action>`), so they are narrow on purpose: a hostile manifest must not be able
//! to reach outside its own folder or pass for another program.

/// Bounds against hostile input: one name, one path (Linux's NAME_MAX and PATH_MAX).
pub const NAME_MAX: usize = 64;
pub const COMPONENT_MAX: usize = 255;
pub const PATH_MAX: usize = 4096;

/// A package name: a legal settings key segment that starts with a letter (`brave`, `cobalt`, `node-svc`).
pub fn check_name(n: &str) -> Result<(), String> {
    let ok = !n.is_empty()
        && n.len() <= NAME_MAX
        && n.as_bytes()[0].is_ascii_lowercase()
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok { Ok(()) } else { Err(format!("bad package name {n:?} (a-z, 0-9, _ and -, starting with a letter)")) }
}

/// An action or service name: the same grammar as a package name.
pub fn check_ident(what: &str, n: &str) -> Result<(), String> {
    check_name(n).map_err(|_| format!("bad {what} name {n:?} (a-z, 0-9, _ and -, starting with a letter)"))
}

/// A version: one folder name, starting with a digit or letter (`1.97.59`, `2026.10.22-1`, `0.1.0+git3`). It
/// shares `apps/<name>/` with the links, the id file and their `*.new` temporaries, so those names are taken.
pub fn check_version(v: &str) -> Result<(), String> {
    let ok = !v.is_empty()
        && v.len() <= NAME_MAX
        && v.as_bytes()[0].is_ascii_alphanumeric()
        && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'~'))
        && !matches!(v, "current" | "previous" | "id")
        && !v.ends_with(".new");
    if ok {
        Ok(())
    } else {
        Err(format!("bad version {v:?} (letters, digits and . _ - + ~, one folder name; not current, previous, id or *.new)"))
    }
}

/// A path inside a package: relative, `/`-separated, no empty, `.` or `..` component, printable ASCII without
/// `\` (Unicode look-alikes and normalisation twins cannot pass, and a package stays copyable over FAT).
pub fn check_rel_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.len() > PATH_MAX {
        return Err(format!("bad path {p:?} (empty or longer than {PATH_MAX})"));
    }
    if p.starts_with('/') {
        return Err(format!("absolute path {p:?} (paths are inside the package)"));
    }
    if let Some(b) = p.bytes().find(|&b| !(0x20..0x7f).contains(&b) || b == b'\\') {
        return Err(format!("path {p:?}: byte 0x{b:02x} is not allowed (printable ASCII without \\)"));
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." || c.len() > COMPONENT_MAX {
            return Err(format!("bad path {p:?} (empty, . or .. component, or one longer than {COMPONENT_MAX})"));
        }
    }
    Ok(())
}

/// A symlink target must stay inside the package: relative, `..` only as leading components and never more of
/// them than the link's folder is deep. `..` after a name is refused because the kernel resolves it physically:
/// `d/up/..` with `d/up -> ..` would leave the package although it looks harmless lexically. Leading `..`
/// climb real folders (a link's parents are never links, see manifest's prefix check), the rest only descends,
/// and every link passed on the way is checked by the same rule.
pub fn check_link_target(link: &str, target: &str) -> Result<(), String> {
    let bad = |why: &str| Err(format!("symlink {link}: target {target:?} {why}"));
    if target.is_empty() || target.starts_with('/') || target.len() > PATH_MAX {
        return bad("must be relative and not empty");
    }
    if target.bytes().any(|b| !(0x20..0x7f).contains(&b) || b == b'\\') {
        return bad("has a byte outside printable ASCII or a \\");
    }
    let mut up = link.split('/').count() - 1;
    let mut named = false;
    for c in target.split('/') {
        match c {
            "" | "." => return bad("has an empty or . component"),
            ".." if named => return bad("has .. after a name"),
            ".." => up = match up.checked_sub(1) {
                Some(u) => u,
                None => return bad("leaves the package"),
            },
            c if c.len() > COMPONENT_MAX => return bad("has a component that is too long"),
            _ => named = true,
        }
    }
    if !named {
        return bad("names no file");
    }
    Ok(())
}

/// An absolute path a package asks for (a permission): normalised, no `.`/`..`, no NUL.
pub fn check_abs_path(p: &str) -> Result<(), String> {
    if p == "/" {
        return Ok(());
    }
    match p.strip_prefix('/') {
        Some(rest) if !rest.ends_with('/') => check_rel_path(rest).map_err(|e| format!("permission path: {e}")),
        _ => Err(format!("permission path {p:?} must be absolute and normalised")),
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn check_sha256(h: &str) -> Result<(), String> {
    if h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        Ok(())
    } else {
        Err(format!("bad sha256 {h:?} (64 lower-case hex digits)"))
    }
}
