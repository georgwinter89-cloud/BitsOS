//! UL-5: packaging. The manifest (`user/packaging/manifest.txt`) adds programs from outside the user workspace
//! and places each in the initramfs `/bin` or on the stick (`\BITSOS2\bin`, `/boot/BITSOS2/bin` on V2). The glibc
//! library set (`user/packaging/libset.txt` plus what `ldd` names for the image's dynamic programs) is copied
//! from CachyOS into the initramfs `/usr/lib/<soname>`, with CachyOS's merged-/usr links (`/lib`, `/lib64` ->
//! `usr/lib`, `/usr/lib64` -> `lib`), the licence texts pacman names and a list of every packaged file with size
//! and SHA-256 (`/usr/share/bitsos/package.txt`). The library lines are compared with `user/packaging/libset.lock`
//! (`cargo xtask initrd --lock` rewrites it): CachyOS is a rolling release, so a changed set is a loud note, an
//! error only with V2_PKG_STRICT_LOCK=1. CachyOS is the build host (V2-PLAN 1a): a missing tool (ldd, gcc, g++),
//! manifest source, library or licence text is an error, so no test can pass on an image without the set;
//! V2_PKG_OPTIONAL=1 turns those into notes, the list says `mode optional` and only then the `ul.pkg` ktests skip.
//! YT-2: a `tree` entry puts a whole directory onto the stick (`\BITSOS2\<dir>\`, Cobalt's ~150 MB), with the
//! libraries its programs need beyond the initramfs set in its own `lib\` (`package/tree.rs`).

#[path = "package/tree.rs"]
mod tree;

use crate::cpio;
use crate::util::{run_status, xdir};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MANIFEST: &str = "user/packaging/manifest.txt";
pub const LIBSET: &str = "user/packaging/libset.txt";
pub const LOCK: &str = "user/packaging/libset.lock";
/// The list in the initramfs (cpio form, no leading slash).
pub const LIST: &str = "usr/share/bitsos/package.txt";
/// The stick's program directory in the ESP tree; V2 mounts the stick at /boot (crates/fs-fat/src/stick.rs MOUNT).
pub const STICK_BIN: &str = "BITSOS2/bin";
pub const STICK_BIN_V2: &str = "/boot/BITSOS2/bin";
/// YT-2: the stick's system directory; a `tree` entry's files go below `BITSOS2/<dir>/` (V2: `/boot/BITSOS2/<dir>`).
pub const STICK_SYS: &str = "BITSOS2";
/// The build host's Mozilla CA bundle (UL-4a) and where the initramfs carries it.
const CA_HOST: &str = "/etc/ssl/certs/ca-certificates.crt";
const CA_DEST: &str = "etc/ssl/certs/ca-certificates.crt";

/// V2_PKG_OPTIONAL=1: a host without the package's tools or sources still builds an image (notes, not errors).
pub fn optional() -> bool {
    std::env::var("V2_PKG_OPTIONAL").is_ok_and(|v| v == "1")
}

/// The zone file /etc/localtime links to (manifest line `data /usr/share/zoneinfo/Europe/Berlin`).
const LOCALTIME_ZONE: &str = "/usr/share/zoneinfo/Europe/Berlin";

/// A missing part of the package: an error, or a note under V2_PKG_OPTIONAL=1.
fn missing(what: String) -> Result<(), String> {
    if optional() {
        eprintln!("xtask: note: {what} (V2_PKG_OPTIONAL=1)");
        Ok(())
    } else {
        Err(format!("{what}; V2_PKG_OPTIONAL=1 builds the image without it"))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    Initrd,
    Stick,
    /// WY-1a: a data file or directory tree in the initramfs at an absolute V2 path (`name`).
    Data,
    /// YT-2: a whole directory tree on the stick at `\BITSOS2\<name>\` (`tree.rs`).
    Tree,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    Host(PathBuf),
    Gcc(PathBuf),
    Gxx(PathBuf),
    /// WY-1a: the binary of the same name from the cargo package in this directory (`compile`).
    Cargo(PathBuf),
    /// YT-2: a file of this repository copied unchanged (a launcher script).
    File(PathBuf),
    /// UL-6: a glibc locale directory made by the host's `localedef -i <input> -f <charmap> --no-archive` (only
    /// for `data`; CachyOS keeps de_DE/en_US only inside its locale-archive, which cannot be split into files).
    Localedef { input: String, charmap: String },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Item {
    pub place: Place,
    pub name: String,
    pub source: Source,
}

fn body(l: &str) -> &str {
    l.split('#').next().unwrap_or("").trim()
}

pub fn parse_manifest(text: &str) -> Result<Vec<Item>, String> {
    let mut v: Vec<Item> = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let l = body(l);
        if l.is_empty() {
            continue;
        }
        let at = format!("{MANIFEST}:{}", n + 1);
        let f: Vec<&str> = l.split_whitespace().collect();
        let [place, name, src] = f[..] else { return Err(format!("{at}: want <place> <name> <source>")) };
        let place = match place {
            "initrd" => Place::Initrd,
            "stick" => Place::Stick,
            "data" => Place::Data,
            "tree" => Place::Tree,
            p => return Err(format!("{at}: place '{p}': initrd, stick, data or tree")),
        };
        let rel = |p: &str| !p.is_empty() && !p.starts_with('/') && !p.split('/').any(|c| c == "..");
        if place == Place::Data {
            // An absolute V2 path below a directory (never /, never with an empty, . or .. component).
            let p = name.strip_prefix('/').unwrap_or("");
            if !name.starts_with('/') || p.is_empty() || p.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
                return Err(format!("{at}: data name '{name}': an absolute path like /usr/share/x/y"));
            }
            match src.split_once(':') {
                Some(("host", h)) if h.starts_with('/') => {}
                Some(("localedef", _)) => {}
                // INT-W9 (SN-9 CR 1): a configuration file of this repository (/etc/asound.conf).
                Some(("file", f)) if rel(f) => {}
                _ => return Err(format!("{at}: data source '{src}': host:/abs/path (a file or a directory), file:rel/path or localedef:<locale>/<charmap>")),
            }
        } else if place == Place::Tree {
            if !tree::name_ok(name) {
                return Err(format!("{at}: tree name '{name}': one directory name under \\BITSOS2\\ (not bin, sys, home, ...)"));
            }
            match src.split_once(':') {
                Some(("host", h)) if h.starts_with('/') => {}
                _ => return Err(format!("{at}: tree source '{src}': host:/abs/dir")),
            }
        } else if name.starts_with('.') || name.contains('/') || lib_dest(name).is_some() {
            return Err(format!("{at}: name '{name}': a file name without '/' or '.so', not starting with '.'"));
        }
        let source = match src.split_once(':') {
            Some(("host", p)) if p.starts_with('/') => Source::Host(p.into()),
            Some(("gcc", p)) if rel(p) => Source::Gcc(p.into()),
            Some(("g++", p)) if rel(p) => Source::Gxx(p.into()),
            Some(("cargo", p)) if rel(p) => Source::Cargo(p.into()),
            Some(("file", p)) if rel(p) => Source::File(p.into()),
            Some(("localedef", p)) if place == Place::Data => {
                let word = |w: &str| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric() || "_@.-".contains(c));
                match p.split_once('/') {
                    Some((i, c)) if word(i) && word(c) => Source::Localedef { input: i.into(), charmap: c.into() },
                    _ => return Err(format!("{at}: source '{src}': localedef:<locale>/<charmap>, e.g. localedef:de_DE/UTF-8")),
                }
            }
            _ => return Err(format!("{at}: source '{src}': host:/abs/path, gcc:rel/path.c, g++:rel/path.cc, cargo:rel/dir or file:rel/path")),
        };
        // A tree's name is a directory under \BITSOS2\, apart from the programs' names (`cobalt` may be both).
        if v.iter().any(|i| i.name == name && (i.place == Place::Tree) == (place == Place::Tree)) {
            return Err(format!("{at}: '{name}' twice"));
        }
        v.push(Item { place, name: name.to_string(), source });
    }
    Ok(v)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum When {
    Always,
    Needed,
}

pub fn parse_libset(text: &str) -> Result<Vec<(String, When)>, String> {
    let mut v = Vec::new();
    for (n, l) in text.lines().enumerate() {
        let l = body(l);
        if l.is_empty() {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        let when = match f[..] {
            [so, "always"] if lib_dest(so).is_some() => When::Always,
            [so, "needed"] if lib_dest(so).is_some() => When::Needed,
            _ => return Err(format!("{LIBSET}:{}: want <soname> always|needed", n + 1)),
        };
        v.push((f[0].to_string(), when));
    }
    Ok(v)
}

/// Where a library of the set lives (cpio path): usr/lib, the loader too, as on CachyOS (merged /usr: PT_INTERP's
/// /lib64/ld-linux-x86-64.so.2 reaches it through the lib64 -> usr/lib link); None for a program. AB-8's
/// `abi_dyn::lib_path` (/lib64 for the loader) names the same file through that link.
pub fn lib_dest(name: &str) -> Option<String> {
    name.contains(".so").then(|| format!("usr/lib/{name}"))
}

/// An ELF file with a PT_INTERP header: a dynamic program.
pub fn is_dynamic(elf: &[u8]) -> bool {
    if elf.len() < 64 || !elf.starts_with(b"\x7fELF") || elf[4] != 2 {
        return false;
    }
    let u16_at = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(elf[0x20..0x28].try_into().unwrap_or([0; 8])) as usize;
    let (ent, num) = (u16_at(0x36), u16_at(0x38));
    (0..num).any(|i| {
        let o = phoff.saturating_add(i * ent);
        elf.get(o..o.saturating_add(4)).is_some_and(|t| t == 3u32.to_le_bytes())
    })
}

/// BR-2: an ELF shared object (ET_DYN with a PT_DYNAMIC and no PT_INTERP): a library, not a program.
pub fn is_shared_lib(elf: &[u8]) -> bool {
    if elf.len() < 64 || !elf.starts_with(b"\x7fELF") || elf[4] != 2 || u16::from_le_bytes([elf[0x10], elf[0x11]]) != 3 {
        return false;
    }
    let u16_at = |o: usize| u16::from_le_bytes([elf[o], elf[o + 1]]) as usize;
    let phoff = u64::from_le_bytes(elf[0x20..0x28].try_into().unwrap_or([0; 8])) as usize;
    let (ent, num) = (u16_at(0x36), u16_at(0x38));
    let has = |t: u32| (0..num).any(|i| elf.get(phoff.saturating_add(i * ent)..).and_then(|b| b.get(..4)).is_some_and(|b| b == t.to_le_bytes()));
    has(2) && !has(3)
}

/// `ldd file`: (soname, the real file) of every library and the loader; the vDSO has no file.
pub fn ldd(file: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    ldd_in(file, None)
}

/// `ldd` with LD_LIBRARY_PATH=`lib_path` (BR-2: a tree's library finds its neighbours in the tree's lib/ as the
/// launcher's LD_LIBRARY_PATH or an RPATH $ORIGIN does on V2).
pub fn ldd_in(file: &Path, lib_path: Option<&Path>) -> Result<Vec<(String, PathBuf)>, String> {
    let mut c = Command::new("ldd");
    c.arg(file).env("LC_ALL", "C");
    if let Some(p) = lib_path {
        c.env("LD_LIBRARY_PATH", p);
    }
    let o = c.output().map_err(|e| format!("ldd: {e}"))?;
    if !o.status.success() {
        return Err(format!("ldd {}: {}", file.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    let mut v = Vec::new();
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        let l = l.trim();
        // "libc.so.6 => /usr/lib/libc.so.6 (0x...)", "/lib64/ld-linux-x86-64.so.2 (0x...)" or "libx.so.1 => not found"
        let (name, file) = match l.split_once(" => ") {
            Some((n, rest)) => (n.trim(), rest.split(" (").next().unwrap_or("").trim()),
            None => {
                let n = l.split(" (").next().unwrap_or("").trim();
                (n, n)
            }
        };
        if file == "not found" {
            return Err(format!("needs {name}, which this host does not have"));
        }
        if !file.starts_with('/') {
            continue;
        }
        let soname = Path::new(name).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let real = std::fs::canonicalize(file).map_err(|e| format!("{file}: {e}"))?;
        v.push((soname, real));
    }
    Ok(v)
}

/// A program on PATH (util's `have_tool` is for tests only).
fn have_tool(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// The host file of a library the set names but no program needs.
fn find_lib(soname: &str) -> Option<PathBuf> {
    ["/usr/lib", "/lib64", "/usr/lib64"].iter().find_map(|d| std::fs::canonicalize(Path::new(d).join(soname)).ok().filter(|p| p.is_file()))
}

/// What the manifest adds to an image.
#[derive(Default)]
pub struct Built {
    /// Programs for the initramfs /bin and the library set (soname, file): appended to the image's programs.
    pub initrd: Vec<(String, PathBuf)>,
    /// Files for the stick: (path below the stick's root, file): `BITSOS2/bin/<name>` for a stick program (a copy
    /// in `stick_dir`), `BITSOS2/<dir>/<path>` for a tree's file (the host file itself) and its libraries.
    pub stick: Vec<(String, PathBuf)>,
}

/// YT-2: what the last build put on the stick, one `<kind>\t<path below the stick's root>\t<file>` line per file
/// (kind prog, file or lib), for the initramfs list (`initrd_extra`).
fn stick_list(root: &Path) -> PathBuf {
    xdir(root).join("pkg/stick.list")
}

/// A stick file of a tree (not a program of `\BITSOS2\bin`).
pub fn is_tree_file(rel: &str) -> bool {
    !rel.starts_with(&format!("{STICK_BIN}/"))
}

/// One line per stick program and one per tree (files, bytes) for xtask's build summary.
pub fn stick_summary(stick: &[(String, PathBuf)]) -> Vec<String> {
    let mut trees: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let mut v = Vec::new();
    for (rel, f) in stick {
        match rel.strip_prefix(&format!("{STICK_BIN}/")) {
            Some(n) => v.push(format!("{STICK_BIN_V2}/{n} <- {}", f.display())),
            None => {
                let top = rel.splitn(3, '/').take(2).last().unwrap_or("");
                let t = trees.entry(top).or_default();
                t.0 += 1;
                t.1 += std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    for (top, (n, bytes)) in trees {
        v.push(format!("/boot/{STICK_SYS}/{top}/ ({n} files, {} MB)", bytes.div_ceil(1 << 20)));
    }
    v
}

/// Where the stick programs of the last build are (the initramfs list names them).
pub fn stick_dir(root: &Path) -> PathBuf {
    xdir(root).join("pkg/stick")
}

/// WY-1a: `cargo:rel/dir` builds the binary named like the entry from the package in `dir` for the host
/// (x86_64-unknown-linux-gnu, CachyOS glibc, dynamic): release, without the package's default features (bitscomp's
/// `nested` back end stays out), stripped, x86-64-v3 (R10), from the lock and the local cache only (`--locked
/// --offline`, no download), in the package's directory so its rust-toolchain file applies, own target directory.
fn cargo_build(root: &Path, dir: &Path, name: &str) -> Result<PathBuf, String> {
    let pkg = root.join(dir);
    if !pkg.join("Cargo.toml").is_file() {
        return Err(format!("{}: no Cargo.toml", pkg.display()));
    }
    if !have_tool("cargo") {
        return Err("no cargo on this host".into());
    }
    let tdir = xdir(root).join("pkg/cargo");
    let mut c = Command::new("cargo");
    c.current_dir(&pkg)
        .args(["build", "--release", "--locked", "--offline", "--no-default-features", "-q", "--bin", name, "--manifest-path"])
        .arg(pkg.join("Cargo.toml"));
    for v in ["RUSTUP_TOOLCHAIN", "RUSTC", "RUSTC_WRAPPER", "RUSTDOC", "RUSTFLAGS", "CARGO_BUILD_TARGET", "CARGO_MANIFEST_DIR"] {
        c.env_remove(v);
    }
    c.env("CARGO_TARGET_DIR", &tdir)
        .env("CARGO_PROFILE_RELEASE_DEBUG", "0")
        .env("CARGO_PROFILE_RELEASE_STRIP", "symbols")
        .env("CARGO_ENCODED_RUSTFLAGS", "-Ctarget-cpu=x86-64-v3");
    run_status(&mut c, &format!("building {name} (cargo, CachyOS glibc, {})", dir.display()))?;
    Ok(tdir.join("release").join(name))
}

fn compile(root: &Path, item: &Item, out: &Path) -> Result<PathBuf, String> {
    let (cc, std, src) = match &item.source {
        Source::Gcc(p) => ("gcc", "-std=gnu11", root.join(p)),
        Source::Gxx(p) => ("g++", "-std=c++17", root.join(p)),
        Source::Cargo(p) => return cargo_build(root, p, &item.name),
        Source::File(p) => {
            let f = root.join(p);
            return if f.is_file() { Ok(f) } else { Err(format!("{} is not in this repository", f.display())) };
        }
        Source::Host(p) => return if p.is_file() { Ok(p.clone()) } else { Err(format!("{} is not on this host", p.display())) },
        Source::Localedef { .. } => return Err(format!("{}: localedef only makes data directories", item.name)),
    };
    if !have_tool(cc) {
        return Err(format!("no {cc} on this host"));
    }
    let exe = out.join(&item.name);
    let mut c = Command::new(cc);
    c.args(["-O2", "-Wall", "-Werror", std, "-pthread", "-fPIE", "-pie", "-o"]).arg(&exe).arg(&src);
    run_status(&mut c, &format!("building {} ({cc}, CachyOS glibc)", item.name))?;
    Ok(exe)
}

/// Builds the manifest's programs and the library set for `programs` (the image's other programs). A missing
/// source, tool or library is an error (a note under V2_PKG_OPTIONAL=1); a broken manifest always is.
pub fn build(root: &Path, programs: &[(String, PathBuf)]) -> Result<Built, String> {
    let items = match std::fs::read_to_string(root.join(MANIFEST)) {
        Ok(t) => parse_manifest(&t)?,
        Err(_) => Vec::new(),
    };
    let out = xdir(root).join("pkg/bin");
    let sdir = stick_dir(root);
    let _ = std::fs::remove_dir_all(&sdir);
    for d in [&out, &sdir] {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let mut b = Built::default();
    for it in items.iter().filter(|i| i.place != Place::Data && i.place != Place::Tree) {
        if programs.iter().any(|(n, _)| *n == it.name) {
            eprintln!("xtask: note: package {}: the image has a program of that name already; the manifest entry is left out", it.name);
            continue;
        }
        let file = match compile(root, it, &out) {
            Ok(f) => f,
            Err(e) => {
                missing(format!("package {} not in the image: {e}", it.name))?;
                continue;
            }
        };
        match it.place {
            Place::Initrd => b.initrd.push((it.name.clone(), file)),
            Place::Data | Place::Tree => {}
            Place::Stick => {
                let dst = sdir.join(&it.name);
                std::fs::copy(&file, &dst).map_err(|e| format!("copy {} -> {}: {e}", file.display(), dst.display()))?;
                b.stick.push((format!("{STICK_BIN}/{}", it.name), dst));
            }
        }
    }
    match lib_set(root, programs.iter().chain(&b.initrd).chain(&b.stick)) {
        Ok(libs) => {
            for (so, f) in libs {
                match programs.iter().find(|(n, _)| *n == so) {
                    // AB-8's copy of the same library (abi_dyn): kept, so the abi-diff harness sees its own file.
                    Some((_, q)) if std::fs::read(q).ok() == std::fs::read(&f).ok() => {}
                    Some((_, q)) => eprintln!("xtask: warning: {so}: {} differs from the set's {}; the image keeps the first", q.display(), f.display()),
                    None => b.initrd.push((so, f)),
                }
            }
        }
        Err(e) => missing(format!("no glibc library set in the image: {e}"))?,
    }
    let mut kinds: Vec<(&str, String, PathBuf)> = b.stick.iter().map(|(r, f)| ("prog", r.clone(), f.clone())).collect();
    trees(&items, programs.iter().chain(&b.initrd), &mut kinds)?;
    b.stick = kinds.iter().map(|(_, r, f)| (r.clone(), f.clone())).collect();
    let list: String = kinds.iter().map(|(k, r, f)| format!("{k}\t{r}\t{}\n", f.display())).collect();
    std::fs::write(stick_list(root), list).map_err(|e| format!("{}: {e}", stick_list(root).display()))?;
    Ok(b)
}

/// YT-2: the manifest's trees with the libraries their programs need beyond `set` (the initramfs's programs and
/// libraries), appended to `out` as (kind, path below the stick's root, file).
fn trees<'a>(items: &[Item], set: impl Iterator<Item = &'a (String, PathBuf)>, out: &mut Vec<(&'static str, String, PathBuf)>) -> Result<(), String> {
    let have: BTreeSet<String> = set.filter(|(n, _)| lib_dest(n).is_some()).map(|(n, _)| n.clone()).collect();
    for it in items.iter().filter(|i| i.place == Place::Tree) {
        let Source::Host(src) = &it.source else { continue };
        let files = match tree::files(src) {
            Ok(f) => f,
            Err(e) => {
                missing(format!("tree {} not on the stick: {e}", it.name))?;
                continue;
            }
        };
        // Without the initramfs set (V2_PKG_OPTIONAL=1 on a host without ldd) every library would land in the tree.
        let libs = if have.iter().any(|s| s.starts_with("ld-linux")) {
            tree::libs(src, &files, &have).or_else(|e| missing(format!("tree {}: {e}", it.name)).map(|_| Vec::new()))?
        } else {
            Vec::new()
        };
        let base = format!("{STICK_SYS}/{}", it.name);
        for (rel, f) in &files {
            out.push(("file", format!("{base}/{rel}"), f.clone()));
        }
        for (so, f) in libs {
            let rel = format!("{}/{so}", tree::LIB_DIR);
            if files.iter().any(|(r, _)| r.eq_ignore_ascii_case(&rel)) {
                eprintln!("xtask: note: tree {}: {rel} is the tree's own; the host's {} is left out", it.name, f.display());
                continue;
            }
            out.push(("lib", format!("{base}/{rel}"), f));
        }
    }
    Ok(())
}

/// The set: libset.txt's `always` libraries and everything ldd names for a dynamic program (sorted by soname).
fn lib_set<'a>(root: &Path, progs: impl Iterator<Item = &'a (String, PathBuf)>) -> Result<Vec<(String, PathBuf)>, String> {
    if !have_tool("ldd") {
        return Err("no ldd on this host".into());
    }
    let listed = match std::fs::read_to_string(root.join(LIBSET)) {
        Ok(t) => parse_libset(&t)?,
        Err(e) => return Err(format!("{LIBSET}: {e}")),
    };
    let mut set: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (n, f) in progs {
        if lib_dest(n).is_some() || !std::fs::read(f).is_ok_and(|b| is_dynamic(&b)) {
            continue;
        }
        for (so, real) in ldd(f).map_err(|e| format!("{n}: {e}"))? {
            set.entry(so).or_insert(real);
        }
    }
    for (so, when) in listed {
        if when == When::Always && !set.contains_key(&so) {
            match find_lib(&so) {
                Some(f) => {
                    set.insert(so, f);
                }
                None => missing(format!("{LIBSET}: {so} is not on this host"))?,
            }
        }
    }
    if !set.keys().any(|k| k.starts_with("ld-linux")) {
        return Err("no ld-linux loader on this host".into());
    }
    Ok(set.into_iter().collect())
}

/// Package, version and licences of a host file, as pacman knows them ("-" where it does not).
#[derive(Clone, Debug, PartialEq)]
struct Origin {
    package: String,
    version: String,
    licenses: Vec<String>,
}

fn pacman(args: &[&str]) -> Option<String> {
    let o = Command::new("pacman").args(args).env("LC_ALL", "C").output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

fn origin(file: &Path, cache: &mut BTreeMap<String, Origin>) -> Origin {
    let unknown = Origin { package: "-".into(), version: "-".into(), licenses: Vec::new() };
    // "/usr/lib/libc.so.6 is owned by glibc 2.44+r50+g1848099f063e-3"
    let Some(owner) = pacman(&["-Qo", &file.to_string_lossy()]) else { return unknown };
    let Some((pkg, ver)) = owner.trim().rsplit_once(" is owned by ").and_then(|(_, r)| r.split_once(' ')) else { return unknown };
    if let Some(o) = cache.get(pkg) {
        return o.clone();
    }
    let info = pacman(&["-Qi", pkg]).unwrap_or_default();
    // "Licenses        : GPL-3.0-or-later WITH GCC-exception-3.1  GFDL-1.3-or-later" (two spaces between entries)
    let lic = info.lines().find_map(|l| l.strip_prefix("Licenses").and_then(|r| r.split_once(':')).map(|(_, r)| r.trim().to_string())).unwrap_or_default();
    let o = Origin { package: pkg.into(), version: ver.into(), licenses: lic.split("  ").map(str::trim).filter(|s| !s.is_empty() && *s != "None").map(String::from).collect() };
    cache.insert(pkg.into(), o.clone());
    o
}

/// SHA-256 (FIPS 180-4), for the reproducible file list.
pub fn sha256(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let mut v = h;
        for i in 0..64 {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7].wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [t1.wrapping_add(t2), v[0], v[1], v[2], v[3].wrapping_add(t1), v[4], v[5], v[6]];
        }
        for (a, b) in h.iter_mut().zip(v) {
            *a = a.wrapping_add(b);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// One line of the list: `<kind> <path on V2> <size> <sha256>` (+ ` <package> <version> <licences>` for a library).
fn line(kind: &str, path: &str, data: &[u8], o: Option<&Origin>) -> String {
    let mut s = format!("{kind} {path} {} {}", data.len(), sha256(data));
    if let Some(o) = o {
        let lic = if o.licenses.is_empty() { "-".to_string() } else { o.licenses.join(" | ") };
        s.push_str(&format!(" {} {} {lic}", o.package, o.version));
    }
    s
}

/// The initramfs side of the package for `programs` (the image's programs, libraries included): the merged-/usr
/// links, the CA bundle, the licence texts and `/usr/share/bitsos/package.txt`, which lists every file the package
/// puts into the image or onto the stick with size and SHA-256 (sorted: the same inputs give the same bytes).
/// Notes (an error with V2_PKG_STRICT_LOCK=1) when the library set differs from `libset.lock`.
pub fn initrd_extra(root: &Path, programs: &[(String, PathBuf)]) -> Result<Vec<cpio::Entry>, String> {
    let items = std::fs::read_to_string(root.join(MANIFEST)).ok().map(|t| parse_manifest(&t)).transpose()?.unwrap_or_default();
    let mut v = Vec::new();
    let mut list = String::from(
        "# BitsOS V2 package list (UL-5, host/xtask/src/package.rs): <kind> <path> <size> <sha256> [<package> <version> <licences>]\n\
         # kinds: lib (glibc set), bin (initramfs /bin), stick (on the stick: \\BITSOS2\\bin and the trees), data, licence\n",
    );
    list.push_str(if optional() { "mode optional: built with V2_PKG_OPTIONAL=1; what is missing is not listed\n" } else { "mode required\n" });
    let (mut libs, mut lines) = (Vec::new(), Vec::new());
    let mut cache = BTreeMap::new();
    let mut lic_files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut lic_missing = BTreeSet::new();
    for (n, f) in programs {
        let data = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
        let Some(dest) = lib_dest(n) else {
            lines.push(line("bin", &format!("/bin/{n}"), &data, None));
            continue;
        };
        // AB-8's copies (abi_dyn) are not pacman's files: ask about the host file with the same bytes.
        let host = find_lib(n).filter(|h| std::fs::read(h).ok().as_deref() == Some(&data[..])).unwrap_or_else(|| f.clone());
        let o = origin(&host, &mut cache);
        licence_texts(&o, &mut lic_files, &mut lic_missing);
        libs.push(line("lib", &format!("/{dest}"), &data, Some(&o)));
    }
    libs.sort();
    if libs.is_empty() {
        list.push_str("libset none: this image was built without a glibc library set (see xtask's note)\n");
    } else {
        // Merged /usr as on CachyOS: /lib and /lib64 lead to usr/lib, /usr/lib64 to lib.
        v.push(cpio::symlink("lib", "usr/lib"));
        v.push(cpio::symlink("lib64", "usr/lib"));
        v.push(cpio::symlink("usr/lib64", "lib"));
        check_lock(root, &libs)?;
    }
    lines.extend(libs);
    match std::fs::read(CA_HOST) {
        Ok(data) => {
            let o = origin(Path::new(CA_HOST), &mut cache);
            licence_texts(&o, &mut lic_files, &mut lic_missing);
            lines.push(line("data", &format!("/{CA_DEST}"), &data, Some(&o)));
            v.push(cpio::file(CA_DEST, 0o644, data));
        }
        // UL-4a: HTTPS programs then need --ca FILE; not part of the dynamic set, so never an error.
        Err(e) => eprintln!("xtask: note: no CA bundle in the image ({CA_HOST}: {e}); HTTPS programs need --ca FILE"),
    }
    // /etc/localtime as a link into the zoneinfo the manifest ships, as on CachyOS (YT-4a CR 1: Starboard reads
    // the zone name from the link target).
    if items.iter().any(|i| i.place == Place::Data && i.name == LOCALTIME_ZONE) {
        v.push(cpio::symlink("etc/localtime", LOCALTIME_ZONE));
    }
    // UL-6: a locale file with the same bytes as an earlier one becomes a link to it (en_US's LC_COLLATE is de_DE's,
    // its LC_CTYPE is C.utf8's: 2.9 MB less in every image; glibc opens them through the link); the list still names
    // it with its own path. Only under LOCALE_DIR: elsewhere a program may lstat or O_NOFOLLOW its data files, and
    // the initramfs has no hard links that would keep them regular files.
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut staged: Vec<PathBuf> = Vec::new();
    for it in items.iter().filter(|i| i.place == Place::Data) {
        let (src, owner) = match &it.source {
            Source::Host(p) => (p.clone(), None),
            Source::File(p) => (root.join(p), None),
            Source::Localedef { input, charmap } => match localedef(root, input, charmap, &it.name) {
                Ok(d) => {
                    staged.push(d.clone());
                    (d, Some(PathBuf::from(format!("{I18N}/locales/{input}"))))
                }
                Err(e) => {
                    missing(format!("data {}: {e}", it.name))?;
                    continue;
                }
            },
            _ => continue,
        };
        let mut files = Vec::new();
        if let Err(e) = data_files(&src, &it.name, &mut files) {
            missing(format!("data {}: {e}", it.name))?;
            continue;
        }
        for (dest, host) in files {
            let data = std::fs::read(&host).map_err(|e| format!("{}: {e}", host.display()))?;
            let o = origin(owner.as_deref().unwrap_or(&host), &mut cache);
            licence_texts(&o, &mut lic_files, &mut lic_missing);
            let l = line("data", &dest, &data, Some(&o));
            let sha = l.split_whitespace().nth(3).unwrap_or_default().to_string();
            lines.push(l);
            match link_to(&mut seen, &dest, sha) {
                Some(first) => v.push(cpio::symlink(dest.trim_start_matches('/'), &first)),
                None => v.push(cpio::file(dest.trim_start_matches('/'), 0o644, data)),
            }
        }
    }
    // This run's localedef output is in the image now; another xtask run builds its own (`localedef`).
    for d in staged {
        let _ = std::fs::remove_dir_all(&d);
        if let Some(p) = d.parent() {
            let _ = std::fs::remove_dir(p);
        }
    }
    // Stick programs and trees as the last build put them (`stick_list`); a tree's libraries with their origin.
    for l in std::fs::read_to_string(stick_list(root)).unwrap_or_default().lines() {
        let mut f = l.splitn(3, '\t');
        let (Some(kind), Some(rel), Some(host)) = (f.next(), f.next(), f.next()) else { continue };
        let Ok(data) = std::fs::read(host) else { continue };
        let o = (kind == "lib").then(|| origin(Path::new(host), &mut cache));
        if let Some(o) = &o {
            licence_texts(o, &mut lic_files, &mut lic_missing);
        }
        lines.push(line("stick", &format!("/boot/{rel}"), &data, o.as_ref()));
    }
    if !lic_missing.is_empty() {
        missing(format!("no licence text on this host for: {}", lic_missing.iter().cloned().collect::<Vec<_>>().join(", ")))?;
    }
    for (p, data) in lic_files {
        lines.push(line("licence", &format!("/{p}"), &data, None));
        v.push(cpio::file(&p, 0o644, data));
    }
    lines.sort_by(|a, b| a.split_whitespace().nth(1).cmp(&b.split_whitespace().nth(1)));
    for l in lines {
        list.push_str(&l);
        list.push('\n');
    }
    v.push(cpio::file(LIST, 0o644, list.into_bytes()));
    Ok(v)
}

/// glibc's locale sources on the host (package glibc: locales/, charmaps/).
const I18N: &str = "/usr/share/i18n";

/// The earlier locale file with the same bytes (`sha`) that `dest` becomes a link to; None for the first one and
/// for every file outside LOCALE_DIR.
fn link_to(seen: &mut BTreeMap<String, String>, dest: &str, sha: String) -> Option<String> {
    if !dest.starts_with(LOCALE_DIR) {
        return None;
    }
    if let Some(first) = seen.get(&sha) {
        return Some(first.clone());
    }
    seen.insert(sha, dest.to_string());
    None
}

/// Where glibc looks for locale directories (no LOCPATH): the only place data files are deduplicated.
const LOCALE_DIR: &str = "/usr/lib/locale/";

/// The files glibc's setlocale(LC_ALL, ...) opens in a locale directory; localedef must have written all of them.
const LOCALE_FILES: &[&str] = &[
    "LC_CTYPE", "LC_NUMERIC", "LC_TIME", "LC_COLLATE", "LC_MONETARY", "LC_MESSAGES/SYS_LC_MESSAGES", "LC_PAPER", "LC_NAME",
    "LC_ADDRESS", "LC_TELEPHONE", "LC_MEASUREMENT", "LC_IDENTIFICATION",
];

/// UL-6: `localedef -i input -f charmap --no-archive` into a fresh directory named like the entry's last path
/// part (glibc's normalized name, e.g. de_DE.utf8); the files must come from the glibc the image's library set
/// is copied from (the locale files carry glibc's format version), which the host's localedef guarantees. The
/// directory carries this process's id, so two xtask runs in one worktree never share or delete each other's.
fn localedef(root: &Path, input: &str, charmap: &str, name: &str) -> Result<PathBuf, String> {
    if !have_tool("localedef") {
        return Err("no localedef on this host".into());
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    let dir = xdir(root).join(format!("pkg/locale/{}", std::process::id())).join(base);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.parent().unwrap_or(&dir)).map_err(|e| format!("{}: {e}", dir.display()))?;
    let o = Command::new("localedef")
        .args(["-i", input, "-f", charmap, "--no-archive"])
        .arg(&dir)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("localedef: {e}"))?;
    // localedef ends with 1 after warnings it still wrote the locale for; anything else, or a missing file, is an error.
    let lost: Vec<&str> = LOCALE_FILES.iter().copied().filter(|f| !dir.join(f).is_file()).collect();
    let made = lost.is_empty();
    if !made && matches!(o.status.code(), Some(0 | 1)) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("localedef -i {input} -f {charmap} wrote no {}", lost.join(", ")));
    }
    match o.status.code() {
        Some(0) if made => Ok(dir),
        Some(1) if made => {
            eprintln!("xtask: note: localedef {input}/{charmap} warned: {}", String::from_utf8_lossy(&o.stderr).trim());
            Ok(dir)
        }
        _ => Err(format!("localedef -i {input} -f {charmap}: {} {}", o.status, String::from_utf8_lossy(&o.stderr).trim())),
    }
}

/// The files of a `data` entry: (V2 path, host file), a directory walked in sorted order (links followed).
fn data_files(src: &Path, dest: &str, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if meta.is_file() {
        out.push((dest.to_string(), src.to_path_buf()));
        return Ok(());
    }
    if !meta.is_dir() {
        return Err(format!("{}: neither a file nor a directory", src.display()));
    }
    let mut es: Vec<_> = std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))?.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        data_files(&e.path(), &format!("{dest}/{}", e.file_name().to_string_lossy()), out)?;
    }
    Ok(())
}

/// The package's own licence files (`/usr/share/licenses/<package>/`) and the SPDX texts its licences name. A
/// pacman licence entry with no text at all (neither the package's files nor one SPDX text of its terms) goes to
/// `missing` (CachyOS ships no glibc licence directory; its LGPL/GPL texts come from the `licenses` package).
fn licence_texts(o: &Origin, into: &mut BTreeMap<String, Vec<u8>>, missing: &mut BTreeSet<String>) {
    let base = Path::new("/usr/share/licenses");
    let mut own = false;
    if o.package != "-"
        && let Ok(rd) = std::fs::read_dir(base.join(&o.package))
    {
        for e in rd.flatten() {
            if let Ok(data) = std::fs::read(e.path()) {
                into.insert(format!("usr/share/licenses/{}/{}", o.package, e.file_name().to_string_lossy()), data);
                own = true;
            }
        }
    }
    for entry in &o.licenses {
        let mut found = false;
        for id in entry.split_whitespace().filter(|t| !matches!(*t, "WITH" | "OR" | "AND")) {
            let id = legacy_spdx(id.trim_matches(|c| c == '(' || c == ')'));
            for (from, to) in [(base.join(format!("spdx/{id}.txt")), format!("usr/share/licenses/spdx/{id}.txt")), (base.join(format!("spdx/exceptions/{id}.txt")), format!("usr/share/licenses/spdx/exceptions/{id}.txt"))] {
                if let Ok(data) = std::fs::read(&from) {
                    into.insert(to, data);
                    found = true;
                }
            }
        }
        if !found && !own {
            missing.insert(format!("{} ({entry})", o.package));
        }
    }
}

/// BR-2: pacman's licence names from before Arch's SPDX switch (libidn2 still says `GPL2  LGPL3`) as the SPDX file
/// with that licence's text (`-only` and `-or-later` share one text); any other name stays as it is.
fn legacy_spdx(id: &str) -> &str {
    match id {
        "GPL2" => "GPL-2.0-only",
        "GPL3" => "GPL-3.0-only",
        "LGPL2" => "LGPL-2.0-only",
        "LGPL2.1" => "LGPL-2.1-only",
        "LGPL3" => "LGPL-3.0-only",
        "AGPL3" => "AGPL-3.0-only",
        "MPL2" => "MPL-2.0",
        "Apache" => "Apache-2.0",
        other => other,
    }
}

/// The lock's lines (without comments).
fn lock_lines(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join(LOCK)).unwrap_or_default().lines().map(body).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// What differs between the set and the lock, by path (empty: the same set).
pub fn lock_drift(lock: &[String], libs: &[String]) -> Vec<String> {
    let key = |l: &String| l.split_whitespace().nth(1).unwrap_or("").to_string();
    let locked: BTreeMap<String, &String> = lock.iter().map(|l| (key(l), l)).collect();
    let mut d = Vec::new();
    for l in libs {
        match locked.get(&key(l)) {
            Some(x) if *x == l => {}
            Some(x) => d.push(format!("changed: {} (lock: {})", l, x.split_whitespace().skip(4).take(2).collect::<Vec<_>>().join(" "))),
            None => d.push(format!("not in the lock: {l}")),
        }
    }
    d
}

/// Only the libraries this image carries are compared: a lock line for a library no program of this image needs
/// (libcap of AB-8's dyn-ls) is no drift.
fn check_lock(root: &Path, libs: &[String]) -> Result<(), String> {
    let d = lock_drift(&lock_lines(root), libs);
    if d.is_empty() {
        return Ok(());
    }
    eprintln!("xtask: note: the glibc library set differs from {LOCK} (CachyOS updated?); `cargo xtask initrd --lock` accepts it:");
    for l in &d {
        eprintln!("xtask: note:   {l}");
    }
    if std::env::var("V2_PKG_STRICT_LOCK").is_ok_and(|v| v == "1") {
        return Err(format!("V2_PKG_STRICT_LOCK=1: the library set differs from {LOCK} in {} lines", d.len()));
    }
    Ok(())
}

/// Writes `libset.lock` from the library lines of an initramfs list.
pub fn write_lock(root: &Path, list: &str) -> Result<usize, String> {
    let libs: Vec<&str> = list.lines().filter(|l| l.starts_with("lib ")).collect();
    if libs.is_empty() {
        return Err("this build has no glibc library set; nothing to lock".into());
    }
    let mut s = String::from(
        "# UL-5: the glibc library set of the image, as the build host (CachyOS) gave it. Written by `cargo xtask initrd\n\
         # --lock`; every image build compares its set with this file and notes what differs.\n\
         # lib <path on V2> <size> <sha256> <package> <version> <licences, ' | ' between pacman's entries>\n\
         # A build compares only the libraries it carries (a needed one, like libcap for AB-8's dyn-ls, may be absent);\n\
         # V2_PKG_STRICT_LOCK=1 makes a difference an error.\n",
    );
    for l in &libs {
        s.push_str(l);
        s.push('\n');
    }
    std::fs::write(root.join(LOCK), s).map_err(|e| format!("{LOCK}: {e}"))?;
    Ok(libs.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_vectors() {
        assert_eq!(sha256(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(sha256(two), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    }

    #[test]
    fn manifest_parses_and_refuses() {
        let m = parse_manifest("# c\n\ninitrd a  g++:user/x.cc # why\nstick b host:/usr/bin/b\n").unwrap();
        assert_eq!(m[0], Item { place: Place::Initrd, name: "a".into(), source: Source::Gxx("user/x.cc".into()) });
        assert_eq!(m[1], Item { place: Place::Stick, name: "b".into(), source: Source::Host("/usr/bin/b".into()) });
        // WY-1a: a cargo-built program and data at an absolute V2 path.
        let m = parse_manifest("stick c cargo:user/services/c\ndata /usr/share/d/e host:/usr/share/e\n").unwrap();
        assert_eq!(m[0], Item { place: Place::Stick, name: "c".into(), source: Source::Cargo("user/services/c".into()) });
        assert_eq!(m[1], Item { place: Place::Data, name: "/usr/share/d/e".into(), source: Source::Host("/usr/share/e".into()) });
        for bad in ["data rel host:/x", "data / host:/x", "data /a/../b host:/x", "data /a//b host:/x", "data /a cargo:x", "initrd a cargo:/abs", "initrd a cargo:../up"] {
            assert!(parse_manifest(bad).is_err(), "{bad}");
        }
        for bad in ["usb a host:/x", "initrd a/b host:/x", "initrd libz.so.1 host:/x", "initrd a host:x", "initrd a gcc:../x.c", "initrd a gcc:/x.c", "initrd a", "initrd a host:/x\nstick a host:/y"] {
            assert!(parse_manifest(bad).is_err(), "{bad}");
        }
        // YT-2: a tree under \BITSOS2\ and a repository file; a tree and a program may share a name.
        let m = parse_manifest("tree cobalt host:/var/x\nstick cobalt file:user/ports/c/cobalt\n").unwrap();
        assert_eq!(m[0], Item { place: Place::Tree, name: "cobalt".into(), source: Source::Host("/var/x".into()) });
        assert_eq!(m[1], Item { place: Place::Stick, name: "cobalt".into(), source: Source::File("user/ports/c/cobalt".into()) });
        for bad in ["tree bin host:/x", "tree SYS host:/x", "tree home host:/x", "tree a/b host:/x", "tree .a host:/x", "tree a file:x", "tree a host:rel", "tree a host:/x\ntree a host:/y", "stick a file:/abs", "stick a file:../x"] {
            assert!(parse_manifest(bad).is_err(), "{bad}");
        }
        // INT-W9 (SN-9): a repository file as data (/etc/asound.conf).
        let m = parse_manifest("data /etc/asound.conf file:host/tests/sn/asound.conf\n").unwrap();
        assert_eq!(m[0].source, Source::File("host/tests/sn/asound.conf".into()));
        for bad in ["data /etc/a file:/abs", "data /etc/a file:../x"] {
            assert!(parse_manifest(bad).is_err(), "{bad}");
        }
        // UL-6: a locale made by localedef, only as data.
        let m = parse_manifest("data /usr/lib/locale/de_DE.utf8 localedef:de_DE/UTF-8\n").unwrap();
        assert_eq!(m[0].source, Source::Localedef { input: "de_DE".into(), charmap: "UTF-8".into() });
        for bad in ["initrd a localedef:de_DE/UTF-8", "data /a localedef:de_DE", "data /a localedef:/UTF-8", "data /a localedef:de/x/y", "data /a localedef:de_DE/../x"] {
            assert!(parse_manifest(bad).is_err(), "{bad}");
        }
        assert!(parse_manifest(&std::fs::read_to_string(crate::util::root().join(MANIFEST)).unwrap()).is_ok());
    }

    /// UL-6: the host's localedef makes de_DE.utf8 with every LC_* file glibc opens (skipped without localedef).
    #[test]
    fn localedef_makes_a_locale() {
        if !have_tool("localedef") || !Path::new(I18N).join("locales/de_DE").is_file() {
            eprintln!("skip: no localedef or no de_DE source on this host");
            return;
        }
        let root = std::env::temp_dir().join(format!("ul6-localedef-{}", std::process::id()));
        let d = localedef(&root, "de_DE", "UTF-8", "/usr/lib/locale/de_DE.utf8").unwrap();
        assert!(d.ends_with("de_DE.utf8"));
        assert!(d.to_string_lossy().contains(&format!("/{}/", std::process::id())), "{}", d.display());
        for f in LOCALE_FILES {
            assert!(d.join(f).is_file(), "{f}");
        }
        assert!(localedef(&root, "xx_NOPE", "UTF-8", "/usr/lib/locale/xx").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// UL-6 fixer: only locale files become links; equal data files elsewhere stay regular files.
    #[test]
    fn only_locale_files_are_linked() {
        let mut seen = BTreeMap::new();
        assert_eq!(link_to(&mut seen, "/usr/lib/locale/C.utf8/LC_CTYPE", "a".into()), None);
        assert_eq!(link_to(&mut seen, "/usr/lib/locale/en_US.utf8/LC_CTYPE", "a".into()).as_deref(), Some("/usr/lib/locale/C.utf8/LC_CTYPE"));
        assert_eq!(link_to(&mut seen, "/usr/share/alsa/pcm/a.conf", "b".into()), None);
        assert_eq!(link_to(&mut seen, "/usr/share/alsa/pcm/b.conf", "b".into()), None);
        assert_eq!(link_to(&mut seen, "/usr/share/zoneinfo/x", "a".into()), None);
    }

    #[test]
    fn stick_summary_counts_trees() {
        let s = stick_summary(&[("BITSOS2/bin/ninja".into(), "/nonexistent/ninja".into()), ("BITSOS2/cobalt/a".into(), "/nonexistent/a".into()), ("BITSOS2/cobalt/lib/b.so.1".into(), "/nonexistent/b".into())]);
        assert_eq!(s, ["/boot/BITSOS2/bin/ninja <- /nonexistent/ninja", "/boot/BITSOS2/cobalt/ (2 files, 0 MB)"]);
        assert!(!is_tree_file("BITSOS2/bin/ninja") && is_tree_file("BITSOS2/cobalt/loader_app") && is_tree_file("BITSOS2/binx/a"));
    }

    /// BR-2: libidn2's pacman licences `GPL2  LGPL3` find their SPDX texts.
    #[test]
    fn legacy_licence_names() {
        assert_eq!((legacy_spdx("GPL2"), legacy_spdx("LGPL3"), legacy_spdx("MPL2")), ("GPL-2.0-only", "LGPL-3.0-only", "MPL-2.0"));
        assert_eq!(legacy_spdx("MIT"), "MIT");
        let o = Origin { package: "-".into(), version: "-".into(), licenses: vec!["GPL2  LGPL3".into()] };
        let (mut into, mut missing) = (BTreeMap::new(), BTreeSet::new());
        licence_texts(&o, &mut into, &mut missing);
        if Path::new("/usr/share/licenses/spdx/GPL-2.0-only.txt").exists() {
            assert!(missing.is_empty() && into.contains_key("usr/share/licenses/spdx/GPL-2.0-only.txt"), "{missing:?}");
        }
    }

    #[test]
    fn libset_parses() {
        let s = parse_libset(&std::fs::read_to_string(crate::util::root().join(LIBSET)).unwrap()).unwrap();
        assert!(s.contains(&("ld-linux-x86-64.so.2".into(), When::Always)) && s.contains(&("libstdc++.so.6".into(), When::Always)));
        assert!(parse_libset("libc.so.6 maybe").is_err() && parse_libset("ls always").is_err());
    }

    #[test]
    fn library_places() {
        assert_eq!(lib_dest("ld-linux-x86-64.so.2").as_deref(), Some("usr/lib/ld-linux-x86-64.so.2"));
        assert_eq!(lib_dest("libstdc++.so.6").as_deref(), Some("usr/lib/libstdc++.so.6"));
        assert_eq!(lib_dest("ninja"), None);
    }

    #[test]
    fn dynamic_is_pt_interp() {
        assert!(!is_dynamic(b"\x7fELF"));
        if let Ok(b) = std::fs::read("/usr/bin/ls") {
            assert!(is_dynamic(&b));
        }
        if let Ok(b) = std::fs::read("/usr/lib/ld-linux-x86-64.so.2") {
            assert!(!is_dynamic(&b), "the loader has no PT_INTERP");
        }
    }

    #[test]
    fn drift_is_by_path() {
        let lock = vec!["lib /usr/lib/libc.so.6 10 aa glibc 1 X".to_string(), "lib /usr/lib/libm.so.6 5 bb glibc 1 X".to_string()];
        assert!(lock_drift(&lock, &lock).is_empty());
        let now = vec!["lib /usr/lib/libc.so.6 11 ab glibc 2 X".to_string(), "lib /usr/lib/libz.so.1 1 cc zlib 1 Z".to_string()];
        let d = lock_drift(&lock, &now);
        assert_eq!(d.len(), 2);
        assert!(d[0].starts_with("changed: ") && d[0].ends_with("(lock: glibc 1)") && d[1].starts_with("not in the lock: "), "{d:?}");
    }

    /// The CachyOS oracle of ktest ul.pkg.cxx: the program built as the image builds it prints pkg-cxx.out.
    #[test]
    fn pkg_cxx_on_the_host() {
        if !have_tool("g++") {
            eprintln!("no g++, skipped");
            return;
        }
        let root = crate::util::root();
        let out = std::env::temp_dir().join(format!("xt-pkg-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let it = Item { place: Place::Initrd, name: "pkg-cxx".into(), source: Source::Gxx("user/packaging/tests/pkg-cxx.cc".into()) };
        let exe = compile(&root, &it, &out).unwrap();
        let o = Command::new(&exe).output().unwrap();
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout), std::fs::read_to_string(root.join("user/packaging/tests/pkg-cxx.out")).unwrap());
        let names: Vec<String> = ldd(&exe).unwrap().into_iter().map(|(n, _)| n).collect();
        for n in ["libstdc++.so.6", "libm.so.6", "libgcc_s.so.1", "libc.so.6", "ld-linux-x86-64.so.2"] {
            assert!(names.iter().any(|x| x == n), "{n} not in {names:?}");
        }
        let _ = std::fs::remove_dir_all(&out);
    }
}
