//! PK-B D1/D2 (plan/spec/program-library.md section 2): what bitspkg needs before its first refresh, taken from the build
//! host at build time and never kept in the repository.
//! D1, the first keyrings (`/etc/bitspkg/keyrings`, the trust root of program installation): the host's pacman
//! keyring files, shipped only when (a) the keyring package pacman has installed lies in pacman's cache with its
//! signature, (b) the image's own bitspkg, run here, says `gut` to that signature, judged against the keyring of
//! the last image this tree built (kept below target/xtask; without one against the host's own folder: the first
//! bootstrap, said in the build log) and (c) every file equals the package's copy. A host without the tools or
//! packages is `Absent` (an error; a note under V2_PKG_OPTIONAL=1), a failed check is `Bad` and always an error.
//! D2, the mirror lists (`/etc/pacman.d`): only the active servers of the host's lists, because their comments
//! carry the host's ranking run (time, tool arguments) and the image may be published.

use super::{Origin, missing, origin, pacman};
use crate::util::xdir;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const KEYRING_HOST: &str = "/usr/share/pacman/keyrings";
const KEYRING_DEST: &str = "etc/bitspkg/keyrings";
/// Where a keyring package carries its files.
const KEYRING_IN_PKG: &str = "usr/share/pacman/keyrings";
const CACHE: &str = "/var/cache/pacman/pkg";
/// The keyring of the last image this tree built, below target/xtask.
const LAST: &str = "pkgtrust/keyrings";
/// The keyring packages and their files.
const RINGS: &[(&str, [&str; 3])] = &[
    ("archlinux-keyring", ["archlinux.gpg", "archlinux-trusted", "archlinux-revoked"]),
    ("cachyos-keyring", ["cachyos.gpg", "cachyos-trusted", "cachyos-revoked"]),
];
/// The same folder on the host and in the image (bitspkg's `include:` lists, repo/sources.rs `default_source`).
const MIRROR_DIR: &str = "etc/pacman.d";
const MIRROR_LISTS: &[&str] = &["mirrorlist", "cachyos-mirrorlist", "cachyos-v3-mirrorlist", "cachyos-v4-mirrorlist"];

/// One file for the initramfs (`dest` without a leading slash, mode 0644) and the package it is from.
pub(super) struct Shipped {
    pub dest: String,
    pub data: Vec<u8>,
    pub origin: Origin,
}

#[derive(Debug, PartialEq)]
enum Fail {
    /// The host lacks a tool, a package or a file: nothing was judged.
    Absent(String),
    /// The host has everything and a check failed.
    Bad(String),
}

/// What the keyring checks read; the tests point it at a temporary folder.
struct Host<'a> {
    keyrings: &'a Path,
    cache: &'a Path,
    last: &'a Path,
    verifier: &'a Path,
    bsdtar: &'a str,
    version: &'a dyn Fn(&str) -> Option<String>,
}

/// A keyring file that passed (a), (b) and (c).
#[derive(Debug, PartialEq)]
struct Checked {
    package: &'static str,
    version: String,
    file: &'static str,
    data: Vec<u8>,
}

/// The version in the answer of `pacman -Q NAME` ("archlinux-keyring 1:20260909-1").
fn q_version(out: &str, name: &str) -> Option<String> {
    let mut w = out.split_whitespace();
    if w.next() != Some(name) {
        return None;
    }
    w.next().map(String::from)
}

/// The cache file of an `any` package. The version becomes part of a path, so it must be one word of pacman's
/// version characters (epoch `1:` included, as the cache spells it).
fn pkg_file(name: &str, version: &str) -> Option<String> {
    let ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_alphanumeric() || b".:+_-".contains(&b));
    ok.then(|| format!("{name}-{version}-any.pkg.tar.zst"))
}

/// The keyring the signatures are judged against, and whether this is the first bootstrap.
fn judge<'a>(h: &Host<'a>) -> (&'a Path, bool) {
    let whole = RINGS.iter().flat_map(|(_, files)| files).all(|f| h.last.join(f).is_file());
    if whole { (h.last, false) } else { (h.keyrings, true) }
}

fn check_ring(h: &Host, package: &'static str, files: &[&'static str; 3], judge: &Path) -> Result<Vec<Checked>, Fail> {
    let version = (h.version)(package).ok_or_else(|| Fail::Absent(format!("pacman -Q names no package {package}")))?;
    let name = pkg_file(package, &version).ok_or_else(|| Fail::Bad(format!("{package}: '{version}' is not a package version")))?;
    let (pkg, sig) = (h.cache.join(&name), h.cache.join(format!("{name}.sig")));
    for f in [&pkg, &sig] {
        if !f.is_file() {
            return Err(Fail::Absent(format!("{} is missing", f.display())));
        }
    }
    // Read once: the bytes compared with the package are the bytes the image gets.
    let mut out = Vec::new();
    for file in files {
        let at = h.keyrings.join(file);
        let data = std::fs::read(&at).map_err(|e| Fail::Absent(format!("{}: {e}", at.display())))?;
        out.push(Checked { package, version: version.clone(), file, data });
    }
    eprintln!("xtask: keyring {package} {version}: (a) {name} and its .sig are in {}", h.cache.display());
    let o = Command::new(h.verifier)
        .args(["pgp", "verify"])
        .args([&pkg, &sig])
        .arg("--keyring")
        .arg(judge)
        .output()
        .map_err(|e| Fail::Bad(format!("{}: cannot start: {e}", h.verifier.display())))?;
    let said = String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("").trim().to_string();
    if !o.status.success() || !said.starts_with("gut ") {
        let err = String::from_utf8_lossy(&o.stderr);
        return Err(Fail::Bad(format!(
            "bitspkg pgp verify {} against {} does not say gut ({}): {said} {}",
            pkg.display(),
            judge.display(),
            o.status,
            err.lines().last().unwrap_or("").trim()
        )));
    }
    eprintln!("xtask: keyring {package} {version}: (b) bitspkg pgp verify against {}: {said}", judge.display());
    for c in &out {
        let member = format!("{KEYRING_IN_PKG}/{}", c.file);
        let o = Command::new(h.bsdtar).arg("-xOf").arg(&pkg).arg(&member).output().map_err(|e| Fail::Absent(format!("{}: cannot start: {e}", h.bsdtar)))?;
        if !o.status.success() {
            return Err(Fail::Bad(format!("{}: no {member} ({})", pkg.display(), String::from_utf8_lossy(&o.stderr).trim())));
        }
        if o.stdout != c.data {
            return Err(Fail::Bad(format!("{} differs from {member} in {}", h.keyrings.join(c.file).display(), pkg.display())));
        }
    }
    eprintln!("xtask: keyring {package} {version}: (c) {} equal the package's copies", files.join(", "));
    Ok(out)
}

/// All keyring files, or the first reason why none may be shipped.
fn keyrings(h: &Host) -> Result<Vec<Checked>, Fail> {
    let (judge, first) = judge(h);
    if first {
        eprintln!(
            "xtask: keyrings: first bootstrap: no keyring of an earlier image in {}, the signatures are judged against the host's own {}",
            h.last.display(),
            h.keyrings.display()
        );
    }
    let mut out = Vec::new();
    for (package, files) in RINGS {
        out.extend(check_ring(h, package, files, judge).map_err(|e| match e {
            Fail::Bad(m) if !first => Fail::Bad(format!("{m} (judged against the last image's keyring; without {} the host's own folder judges again)", h.last.display())),
            e => e,
        })?);
    }
    Ok(out)
}

/// Keeps the checked keyring for the next build's check (b). Each file lands by rename from outside the folder:
/// bitspkg reads every `*.gpg` of a keyring folder, a half-written one must never be there.
fn remember(last: &Path, files: &[Checked]) -> Result<(), String> {
    std::fs::create_dir_all(last).map_err(|e| format!("{}: {e}", last.display()))?;
    for c in files {
        let at = last.join(c.file);
        if std::fs::read(&at).ok().as_deref() == Some(&c.data[..]) {
            continue;
        }
        let tmp = last.with_file_name(format!("{}.tmp{}", c.file, std::process::id()));
        std::fs::write(&tmp, &c.data).and_then(|()| std::fs::rename(&tmp, &at)).map_err(|e| format!("{}: {e}", at.display()))?;
    }
    Ok(())
}

/// The active servers of a pacman mirror list, read as bitspkg reads it (repo/sources.rs `mirrorlist`), and the
/// number of `Server` lines left out: only a plain http(s) address goes into an image that may be published (no
/// local path, no user name, password or query).
fn servers(text: &str) -> (Vec<String>, usize) {
    let (mut keep, mut left_out) = (Vec::new(), 0);
    for l in text.lines() {
        let l = l.split('#').next().unwrap_or("").trim();
        let Some(url) = l.split_once('=').filter(|(k, _)| k.trim() == "Server").map(|(_, v)| v.trim()) else { continue };
        let host = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).map(|r| r.split('/').next().unwrap_or(""));
        let plain = host.is_some_and(|h| !h.is_empty() && !h.contains('@')) && !url.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'?');
        if !plain {
            left_out += 1;
        } else if !keep.iter().any(|k| k == url) {
            keep.push(url.to_string());
        }
    }
    (keep, left_out)
}

fn list_text(servers: &[String]) -> String {
    let mut s = String::from("# The build host's active servers in its order; bitspkg tries them from the top.\n");
    for u in servers {
        s.push_str(&format!("Server = {u}\n"));
    }
    s
}

/// The image's text of the host list `at`; `Absent` without the file, `Bad` when it names no usable server.
fn mirror_list(at: &Path) -> Result<String, Fail> {
    let text = std::fs::read_to_string(at).map_err(|e| Fail::Absent(format!("{}: {e}", at.display())))?;
    let (keep, left_out) = servers(&text);
    if keep.is_empty() {
        return Err(Fail::Bad(format!("{} has no active http(s) Server line", at.display())));
    }
    let note = if left_out == 0 { String::new() } else { format!(", {left_out} Server lines left out (not a plain http(s) address)") };
    eprintln!("xtask: mirror list {}: {} servers, comments not copied{note}", at.display(), keep.len());
    Ok(list_text(&keep))
}

/// The keyrings and mirror lists of an image with bitspkg (`programs`); an image without it gets none.
pub(super) fn files(root: &Path, programs: &[(String, PathBuf)], cache: &mut BTreeMap<String, Origin>) -> Result<Vec<Shipped>, String> {
    let Some((_, bitspkg)) = programs.iter().find(|(n, _)| n == "bitspkg") else {
        eprintln!("xtask: note: no keyrings and mirror lists in the image: it has no bitspkg");
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let last = xdir(root).join(LAST);
    let version = |name: &str| pacman(&["-Q", name]).and_then(|o| q_version(&o, name));
    let h = Host { keyrings: Path::new(KEYRING_HOST), cache: Path::new(CACHE), last: &last, verifier: bitspkg, bsdtar: "bsdtar", version: &version };
    match keyrings(&h) {
        Ok(checked) => {
            for c in &checked {
                let at = h.keyrings.join(c.file);
                let o = origin(&at, cache);
                if o.package != c.package || o.version != c.version {
                    return Err(format!("keyrings: pacman names {} {} as the owner of {}, the checked package is {} {}", o.package, o.version, at.display(), c.package, c.version));
                }
                out.push(Shipped { dest: format!("{KEYRING_DEST}/{}", c.file), data: c.data.clone(), origin: o });
            }
            remember(&last, &checked)?;
            eprintln!("xtask: keyrings: {} files -> /{KEYRING_DEST} (0644)", checked.len());
        }
        Err(Fail::Absent(m)) => missing(format!("no keyrings in the image (bitspkg installs nothing without them): {m}"))?,
        Err(Fail::Bad(m)) => return Err(format!("keyrings: {m}; no image without the check: they are the trust root of program installation")),
    }
    for name in MIRROR_LISTS {
        let at = Path::new("/").join(MIRROR_DIR).join(name);
        match mirror_list(&at) {
            Ok(text) => out.push(Shipped { dest: format!("{MIRROR_DIR}/{name}"), data: text.into_bytes(), origin: origin(&at, cache) }),
            Err(Fail::Absent(m)) => missing(format!("no mirror list /{MIRROR_DIR}/{name} in the image: {m}"))?,
            Err(Fail::Bad(m)) => return Err(format!("mirror list: {m}")),
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod tests;
