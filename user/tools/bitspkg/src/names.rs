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

/// PK-6d: a pacman package name as a BitsOS program name. pacman allows alphanumerics and `@ . _ + -`, not
/// starting with `-` or `.` (doc/PKGBUILD.5 pkgname); a program name is check_name's narrow grammar. A name that
/// already fits stays; otherwise upper case goes lower, `.` becomes `_`, `+` becomes `plus`, `@` becomes `_at_`,
/// and a name that does not start with a letter gets `pkg-` in front (`7zip` -> `pkg-7zip`, `libsigc++` ->
/// `libsigcplusplus`). The mapping is not one to one (`a.b` and `a_b` meet): the install refuses a program folder
/// whose packages do not include this pacman name. The pacman name stays in the manifest's [[origin]].
pub fn program_of(pkgname: &str) -> Result<String, String> {
    if check_name(pkgname).is_ok() {
        return Ok(pkgname.to_string());
    }
    if pkgname.is_empty() {
        return Err("leerer Paketname".into());
    }
    let mut p = String::new();
    for c in pkgname.chars() {
        match c {
            'a'..='z' | '0'..='9' | '_' | '-' => p.push(c),
            'A'..='Z' => p.push(c.to_ascii_lowercase()),
            '.' => p.push('_'),
            '+' => p.push_str("plus"),
            '@' => p.push_str("_at_"),
            _ => return Err(format!("{pkgname:?}: Zeichen {c:?} passt in keinen Programmnamen")),
        }
    }
    if !p.starts_with(|c: char| c.is_ascii_lowercase()) {
        p.insert_str(0, "pkg-");
    }
    check_name(&p).map_err(|_| format!("{pkgname:?}: als Programmname {p:?} zu lang oder ungültig (a-z, 0-9, _ und -, vorn ein Buchstabe, höchstens {NAME_MAX} Zeichen)"))?;
    Ok(p)
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

/// PK-6d (plan 1.6): a pacman version (`[epoch:]pkgver-pkgrel`) as a folder name: the epoch `N:` is spelled `eN_`
/// (a `:` is no folder name), everything else stays. A version whose other characters no folder may carry is
/// refused with the reason (pacman allows little more than check_version: libmakepkg/lint_pkgbuild/pkgver.sh).
/// A version without an epoch that already looks like the spelling (`e1_2-1`) is refused, so the mapping stays
/// one to one. The folder name is for the file system only: the real pacman version is the manifest's [[origin]].
pub fn folder_of(pacver: &str) -> Result<String, String> {
    let f = match pacver.split_once(':') {
        Some((e, rest)) if digits(e) => format!("e{e}_{rest}"),
        Some(_) => return Err(format!("Version {pacver:?}: die Epoche ist keine Zahl")),
        None if pacver.strip_prefix('e').and_then(|r| r.split_once('_')).is_some_and(|(e, _)| digits(e)) => {
            return Err(format!("Version {pacver:?}: sieht aus wie BitsOS' Schreibweise einer Epoche (e<N>_), abgelehnt"));
        }
        None => pacver.to_string(),
    };
    check_version(&f).map_err(|e| format!("Version {pacver:?} passt in keinen Ordnernamen: {e}"))?;
    Ok(f)
}

/// The program version of a closure: the top level's folder name, then `+<n>` counting the closure changes of
/// that same top-level version (`e1_1.98.63-1+2`).
pub fn program_version(pacver: &str, n: u32) -> Result<String, String> {
    let v = format!("{}+{n}", folder_of(pacver)?);
    check_version(&v)?;
    Ok(v)
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit())
}

/// The way back for `info` and `list`: a program version folder as pacman spells it (`e1_1.0-1+2` -> `1:1.0-1`
/// and the closure count 2). A pacman version ends in `-<pkgrel>`, and a pkgrel is `integer[.integer]`
/// (libmakepkg/lint_pkgbuild/pkgrel.sh): bitspkg's own suffixes, `~<8 hex>` (another build of the same version
/// in .pkgs) and `+<n>` (the closure count), are read only after the last `-`, where a pkgver's own `~` and `+`
/// cannot be (`1.0~rc1-1` stays itself). A folder bitspkg did not make comes back as it is, without a count.
pub fn pacman_of(folder: &str) -> (String, Option<u32>) {
    let Some((head, tail)) = folder.rsplit_once('-') else { return (folder.to_string(), None) };
    let (tail, n) = match tail.split_once('+') {
        Some((t, n)) if digits(n) => (t, n.parse().ok()),
        _ => (tail, None),
    };
    let tail = match tail.split_once('~') {
        Some((t, h)) if h.len() == 8 && h.bytes().all(|c| c.is_ascii_hexdigit()) => t,
        _ => tail,
    };
    let base = format!("{head}-{tail}");
    let v = match base.strip_prefix('e').and_then(|r| r.split_once('_')) {
        Some((e, rest)) if digits(e) => format!("{e}:{rest}"),
        _ => base,
    };
    (v, n)
}


/// Where a repository package may keep non-ASCII names (plan 1.6, PACKAGES.md 6.7): glibc's locales and
/// translations and ICU's data need them; anywhere else such a name is dropped at unpack and reported.
pub const NON_ASCII_DIRS: [&str; 5] = ["usr/share/locale/", "usr/share/i18n/", "usr/lib/locale/", "usr/share/icu/", "usr/lib/icu/"];

/// May `p` (relative to a root tree, or `root/...` inside a format-2 version) carry a non-ASCII name?
pub fn non_ascii_allowed(p: &str) -> bool {
    let q = p.strip_prefix("root/").unwrap_or(p);
    NON_ASCII_DIRS.iter().any(|d| q.starts_with(d))
}

/// A path of a format-2 version (PK-6d): check_rel_path's rules, except that `\` is a name byte like any other
/// (these trees are never copied over FAT) and non-ASCII UTF-8 is allowed below NON_ASCII_DIRS.
pub fn check_repo_path(p: &str) -> Result<(), String> {
    if p.is_empty() || p.len() > PATH_MAX {
        return Err(format!("bad path {p:?} (empty or longer than {PATH_MAX})"));
    }
    if p.starts_with('/') {
        return Err(format!("absolute path {p:?} (paths are inside the package)"));
    }
    if let Some(b) = p.bytes().find(|&b| b < 0x20 || b == 0x7f) {
        return Err(format!("path {p:?}: control byte 0x{b:02x}"));
    }
    if !p.is_ascii() && !non_ascii_allowed(p) {
        return Err(format!("path {p:?}: non-ASCII name outside {}", NON_ASCII_DIRS.join(", ")));
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." || c.len() > COMPONENT_MAX {
            return Err(format!("bad path {p:?} (empty, . or .. component, or one longer than {COMPONENT_MAX})"));
        }
    }
    Ok(())
}

/// A symlink of a format-2 version: it is read in the program's view, where the root tree is `/`, so an absolute
/// target (`/usr/share/fontconfig/conf.default/x`) or a relative one with `..` names a file of the same view;
/// nothing on the host follows these links (install and verify read them, bitsrun binds folders, not links).
pub fn check_view_link(link: &str, target: &str) -> Result<(), String> {
    if target.is_empty() || target.len() > PATH_MAX || target.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(format!("symlink {link}: target {target:?} empty, too long or with a control byte"));
    }
    Ok(())
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn epoch_round_trip() {
        for (a, b) in [("foot", "foot"), ("7zip", "pkg-7zip"), ("0ad", "pkg-0ad"), ("libsigc++", "libsigcplusplus"), ("dvd+rw-tools", "dvdplusrw-tools"), ("python-foo.bar", "python-foo_bar"), ("lib32-x@y", "lib32-x_at_y"), ("Foo", "foo")] {
            assert_eq!(program_of(a).unwrap(), b, "{a}");
        }
        assert!(program_of("a b").is_err() && program_of("").is_err() && program_of(&"x.".repeat(40)).is_err());
        assert_eq!(folder_of("1:1.98.63-1").unwrap(), "e1_1.98.63-1");
        assert_eq!(folder_of("2.44+r50+g1848099f063e-3").unwrap(), "2.44+r50+g1848099f063e-3");
        assert_eq!(program_version("1:1.98.63-1", 2).unwrap(), "e1_1.98.63-1+2");
        assert_eq!(pacman_of("e1_1.98.63-1+2"), ("1:1.98.63-1".to_string(), Some(2)));
        assert_eq!(pacman_of("2.44+r50+g1848099f063e-3+1"), ("2.44+r50+g1848099f063e-3".to_string(), Some(1)));
        assert_eq!(pacman_of("1.8.2-1.1~0a1b2c3d"), ("1.8.2-1.1".to_string(), None));
        assert_eq!(pacman_of("1.0~rc1-1"), ("1.0~rc1-1".to_string(), None));
        assert_eq!(pacman_of("1.0~rc1-1+3"), ("1.0~rc1-1".to_string(), Some(3)));
        assert_eq!(pacman_of("1.0+1-1"), ("1.0+1-1".to_string(), None));
        assert_eq!(pacman_of("e2_1.0+1-1~deadbeef"), ("2:1.0+1-1".to_string(), None));
        assert!(folder_of("e1_2-1").is_err(), "a pkgver that looks like the epoch spelling");
        assert_eq!(folder_of("e1x_2-1").unwrap(), "e1x_2-1");
        assert!(folder_of("x:1-1").is_err());
        assert!(folder_of("1:a b-1").is_err());
    }
}
