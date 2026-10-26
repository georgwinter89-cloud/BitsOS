#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A fresh folder per test under cargo's integration-test temp dir.
pub fn tmp(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bitspkg").join(name);
    if d.exists() {
        make_writable(&d);
        fs::remove_dir_all(&d).unwrap();
    }
    fs::create_dir_all(&d).unwrap();
    d
}

pub fn make_writable(p: &Path) {
    if let Ok(md) = fs::symlink_metadata(p)
        && md.is_dir()
    {
        let _ = fs::set_permissions(p, fs::Permissions::from_mode(0o755));
        for e in fs::read_dir(p).unwrap().flatten() {
            make_writable(&e.path());
        }
    }
}

pub fn write(p: &Path, data: &str, mode: u32) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

pub const SPEC: &str = r#"format = 1
name = "demo"
version = "VERSION"
en = "A demo program"
program = "bin/demo"

[[permission]]
kind = "net_connect"
proto = "tcp"
port = 443
why = "fetches pages"

[[action]]
name = "open_url"
safety = "change"
de = "Öffnet eine Adresse"
en = "Opens an address"
args = '{"type":"object","properties":{"url":{"type":"string"}}}'

[[setting]]
key = "app.demo.homepage"
type = "text"
max_bytes = 200
default = "about:blank"
de = "Startseite"
en = "Home page"
"#;

/// A package tree for `demo` at `version`: a shared library (same in every version), a program that names the
/// version, a symlink and an empty folder.
pub fn tree(dir: &Path, version: &str) -> PathBuf {
    let t = dir.join(format!("tree-{version}"));
    write(&t.join("package.toml"), &SPEC.replace("VERSION", version), 0o644);
    write(&t.join("bin/demo"), &format!("#!/bin/sh\necho demo {version}\n"), 0o755);
    write(&t.join("lib/libshared.so.1.0"), "shared library bytes\n", 0o644);
    std::os::unix::fs::symlink("libshared.so.1.0", t.join("lib/libshared.so.1")).unwrap();
    write(&t.join("share/notes.txt"), &format!("notes for {version}\n"), 0o644);
    fs::create_dir_all(t.join("var/empty")).unwrap();
    t
}

/// Builds `demo` `version` into `<dir>/pkg-<version>` and returns that package folder.
pub fn package(dir: &Path, version: &str) -> PathBuf {
    let t = tree(dir, version);
    let out = dir.join(format!("pkg-{version}"));
    bitspkg::build::build(&t, None, Some(&out)).unwrap();
    out
}

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_bitspkg")
}

pub fn run(root: &Path, args: &[&str], cut_at: Option<u64>) -> (i32, String, String) {
    let mut c = Command::new(bin());
    c.arg("--root").arg(root).args(args);
    match cut_at {
        Some(n) => c.env(bitspkg::cut::ENV, n.to_string()),
        None => c.env_remove(bitspkg::cut::ENV),
    };
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

/// What `current` of demo runs: the version line of bin/demo, read through the `current` link.
pub fn current_program(root: &Path) -> Option<String> {
    fs::read_to_string(root.join("apps/demo/current/bin/demo")).ok().and_then(|s| s.lines().nth(1).map(str::to_string))
}

/// Every path under `p` (relative, sorted), with a short type tag, for leftover checks.
pub fn tree_list(p: &Path) -> Vec<String> {
    fn walk(base: &Path, p: &Path, out: &mut Vec<String>) {
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let md = fs::symlink_metadata(e.path()).unwrap();
            let rel = e.path().strip_prefix(base).unwrap().to_string_lossy().to_string();
            let tag = if md.file_type().is_symlink() { "l" } else if md.is_dir() { "d" } else { "f" };
            out.push(format!("{tag} {rel}"));
            if md.is_dir() {
                walk(base, &e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(p, p, &mut out);
    out.sort();
    out
}
