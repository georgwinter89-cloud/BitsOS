//! PK-4 `bitspkg import SRC OUT`: a foreign program folder (copied from CachyOS: a vendor folder like
//! /opt/brave-bin, or a tree that mirrors a root) becomes a tree `bitspkg build` takes (plan/PACKAGES.md 7):
//!
//! - files are copied (modes kept, setuid/setgid and special files left out and reported), names that are not
//!   printable ASCII are left out (PACKAGES.md 2), `--exclude` paths too;
//! - absolute symlinks that point inside the tree become relative (`/usr/lib/x.so -> /usr/lib/x.so.1` is
//!   `x.so.1`; with `--origin /opt/brave-bin` a link to `/opt/brave-bin/y` is one to `y`), links that leave the
//!   tree or dangle are dropped with a report line;
//! - the libraries `ldd` names for every program and shared library in it (and for `--dlopen` names, which the
//!   program loads itself) are copied in under their sonames, with their own needs, until nothing is missing.
//!   Layout `libpath` (default): into `lib/`, without the glibc set every V2 image has (`--system`, UL-5's
//!   libset.txt `always` lines); the program finds them through LD_LIBRARY_PATH, as Brave's and Cobalt's
//!   launchers set it. Layout `view`: everything, glibc and the loader too, into `usr/lib/` with `lib64 ->
//!   usr/lib`, so the program view of PK-3 maps them onto /usr/lib and PT_INTERP finds the package's own ld.so;
//!   until views exist the wrapper starts the program through that ld.so (`--library-path`).
//! - `--launcher REL` writes the wrapper `bitsos/run` (the manifest's `program`) for REL.

use crate::elf::{self, Kind};
use crate::fsx::err;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
pub enum Layout {
    LibPath,
    View,
}

pub struct Import {
    pub origin: Option<PathBuf>,
    pub system: BTreeSet<String>,
    pub dlopen: Vec<String>,
    pub exclude: Vec<String>,
    pub layout: Layout,
    pub launcher: Option<String>,
}

/// Where `--dlopen` names are looked up (CachyOS: merged /usr, the loader's own default path).
const SYSTEM_DIRS: [&str; 3] = ["/usr/lib", "/usr/lib64", "/lib64"];
const LOADER: &str = "ld-linux-x86-64.so.2";
/// The package folder from the wrapper's own path `<pkg>/bitsos/run` (also used by brave/run).
const PROLOGUE: &str = "case $0 in */*) D=${0%/*} ;; *) D=. ;; esac\nT=$(cd \"$D/..\" && pwd) || exit 1\n";
/// Deeper than this is a link loop or no program folder.
const MAX_DEPTH: usize = 64;

/// The sonames UL-5 puts into every V2 image (`<soname> always` lines of user/packaging/libset.txt).
pub fn read_libset(p: &Path) -> Result<BTreeSet<String>, String> {
    let t = fs::read_to_string(p).map_err(|e| err(p, e))?;
    Ok(t.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            match (w.next(), w.next()) {
                (Some(n), Some("always")) if !n.starts_with('#') => Some(n.to_string()),
                _ => None,
            }
        })
        .collect())
}

fn ascii_ok(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'\\')
}

/// A library name from a foreign DT_NEEDED or `--dlopen`: one file name in the lib folder, nothing more.
fn lib_name_ok(name: &str) -> bool {
    ascii_ok(name) && name != "." && name != ".." && !name.contains('/') && !name.starts_with('-')
}

/// A path the wrapper script quotes: no character the shell treats specially inside double quotes.
fn script_safe(rel: &str) -> bool {
    !rel.is_empty() && rel.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/+-@".contains(&b))
}

struct Run<'a> {
    src: &'a Path,
    out: &'a Path,
    opt: &'a Import,
    report: Vec<String>,
}

impl Run<'_> {
    fn excluded(&self, rel: &str) -> bool {
        self.opt.exclude.iter().any(|x| rel == x || rel.starts_with(&format!("{x}/")))
    }

    fn copy_tree(&mut self, rel: &str, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err(format!("{}: deeper than {MAX_DEPTH} folders", self.src.join(rel).display()));
        }
        let dir = self.src.join(rel);
        let mut es: Vec<_> = fs::read_dir(&dir).map_err(|e| err(&dir, e))?.collect::<Result<_, _>>().map_err(|e| err(&dir, e))?;
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let name = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if self.excluded(&r) {
                self.report.push(format!("excluded {r}"));
                continue;
            }
            if !ascii_ok(&name) || e.file_name().to_str().is_none() {
                self.report.push(format!("left out {r} (not a printable ASCII name)"));
                continue;
            }
            let (from, to) = (e.path(), self.out.join(&r));
            let md = fs::symlink_metadata(&from).map_err(|x| err(&from, x))?;
            let ft = md.file_type();
            if ft.is_symlink() {
                self.link(&r, &from, &to)?;
            } else if ft.is_dir() {
                fs::create_dir(&to).map_err(|x| err(&to, x))?;
                self.copy_tree(&r, depth + 1)?;
            } else if !ft.is_file() {
                self.report.push(format!("left out {r} (not a regular file)"));
            } else if md.mode() & 0o6000 != 0 {
                self.report.push(format!("left out {r} (setuid/setgid: packages never carry it)"));
            } else {
                fs::copy(&from, &to).map_err(|x| err(&from, x))?;
                fs::set_permissions(&to, fs::Permissions::from_mode(md.mode() & 0o777 | 0o200)).map_err(|x| err(&to, x))?;
            }
        }
        Ok(())
    }

    /// A link the package can carry: relative, inside the tree, to something the tree keeps.
    fn link(&mut self, rel: &str, from: &Path, to: &Path) -> Result<(), String> {
        let t = fs::read_link(from).map_err(|e| err(from, e))?;
        let ts = t.to_string_lossy().into_owned();
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        let target = if t.is_absolute() {
            let inner = self.opt.origin.as_ref().and_then(|o| t.strip_prefix(o).ok()).map(Path::to_path_buf);
            normal("", &inner.unwrap_or_else(|| t.strip_prefix("/").unwrap_or(&t).to_path_buf()).to_string_lossy())
        } else {
            normal(dir, &ts)
        };
        let keep = target.filter(|x| !x.is_empty() && !self.excluded(x) && x.split('/').all(ascii_ok) && fs::symlink_metadata(self.src.join(x)).is_ok());
        let Some(x) = keep else {
            self.report.push(format!("dropped link {rel} -> {ts} (outside the tree, dangling or left out)"));
            return Ok(());
        };
        let new = relative(dir, &x);
        if new != ts {
            self.report.push(format!("rewrote link {rel} -> {new} (was {ts})"));
        }
        std::os::unix::fs::symlink(&new, to).map_err(|e| err(to, e))
    }

    fn lib_dir(&self) -> &'static str {
        if self.opt.layout == Layout::View { "usr/lib" } else { "lib" }
    }

    /// Copies the library closure into the lib folder; every file it names must be found.
    fn libs(&mut self) -> Result<(), String> {
        let inside = fs::canonicalize(self.out).map_err(|e| err(self.out, e))?;
        // Absolute, so ldd names what it finds there with absolute paths.
        let lib = inside.join(self.lib_dir());
        fs::create_dir_all(&lib).map_err(|e| err(&lib, e))?;
        let mut have: BTreeSet<String> = fs::read_dir(&lib).map_err(|e| err(&lib, e))?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        let mut queue = Vec::new();
        elves(self.out, &mut queue)?;
        for n in &self.opt.dlopen {
            if !lib_name_ok(n) {
                return Err(format!("--dlopen {n:?}: not a library file name"));
            }
            if have.contains(n) {
                continue;
            }
            let f = SYSTEM_DIRS.iter().map(|d| Path::new(d).join(n)).find(|p| p.is_file()).ok_or_else(|| format!("--dlopen {n}: not in {}", SYSTEM_DIRS.join(", ")))?;
            queue.push(self.add(&lib, n, &f, &mut have)?);
        }
        let mut missing = BTreeSet::new();
        while let Some(f) = queue.pop() {
            for n in elf::ldd(&f, &lib)? {
                let name = if n.loader { LOADER.to_string() } else { n.name.clone() };
                if !lib_name_ok(&name) {
                    // A DT_NEEDED like `../../x` or `sub/libx.so` would write outside the lib folder.
                    return Err(format!("{} needs {name:?}, which is not a library file name", f.strip_prefix(self.out).unwrap_or(&f).display()));
                }
                let Some(real) = n.file else {
                    missing.insert(format!("{name} (needed by {})", f.strip_prefix(self.out).unwrap_or(&f).display()));
                    continue;
                };
                let in_tree = fs::canonicalize(&real).is_ok_and(|c| c.starts_with(&inside));
                let system = self.opt.layout == Layout::LibPath && (n.loader || self.opt.system.contains(&name));
                if in_tree || system || have.contains(&name) {
                    continue;
                }
                let dst = self.add(&lib, &name, &real, &mut have)?;
                if !n.loader {
                    queue.push(dst);
                }
            }
        }
        if !missing.is_empty() {
            return Err(format!("libraries not found on this host: {}", missing.into_iter().collect::<Vec<_>>().join(", ")));
        }
        if self.opt.layout == Layout::View && fs::symlink_metadata(self.out.join("lib64")).is_err() {
            std::os::unix::fs::symlink("usr/lib", self.out.join("lib64")).map_err(|e| err(self.out, e))?;
        }
        Ok(())
    }

    fn add(&mut self, lib: &Path, name: &str, real: &Path, have: &mut BTreeSet<String>) -> Result<PathBuf, String> {
        let dst = lib.join(name);
        // Never through a link or onto a file the tree brought (lib_name_ok keeps `dst` directly in `lib`).
        if fs::symlink_metadata(&dst).is_ok() {
            return Err(format!("{}: already in the tree, not replaced by {}", dst.display(), real.display()));
        }
        fs::copy(real, &dst).map_err(|e| err(real, e))?;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).map_err(|e| err(&dst, e))?;
        self.report.push(format!("lib {}/{name} <- {}", self.lib_dir(), real.display()));
        have.insert(name.to_string());
        Ok(dst)
    }

    fn launcher(&mut self, rel: &str) -> Result<(), String> {
        if !script_safe(rel) {
            return Err(format!("--launcher {rel:?}: only letters, digits and ._/+-@ (the wrapper script quotes it)"));
        }
        if !self.out.join(rel).is_file() {
            return Err(format!("--launcher {rel}: no such file in the tree"));
        }
        let start = match self.opt.layout {
            Layout::LibPath => format!("export LD_LIBRARY_PATH=\"$T/lib${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}}\"\nexec \"$T/{rel}\" \"$@\""),
            // Only the first process: a child exec'd directly gets the image's loader and libraries until PK-3's
            // view maps usr/lib (PACKAGES.md 7.2); a program that re-execs itself needs layout libpath until then.
            Layout::View => format!("exec \"$T/usr/lib/{LOADER}\" --library-path \"$T/usr/lib\" \"$T/{rel}\" \"$@\""),
        };
        // T absolute (a relative LD_LIBRARY_PATH entry would be read against each exec's working folder) and
        // logical (through `current`, not the version folder), however the wrapper was started.
        let text = format!(
            "#!/bin/sh\n# bitspkg import: starts {rel} with the package's own libraries (plan/PACKAGES.md 7).\n{PROLOGUE}{start}\n"
        );
        let p = self.out.join("bitsos/run");
        fs::create_dir_all(self.out.join("bitsos")).map_err(|e| err(&p, e))?;
        fs::write(&p, text).map_err(|e| err(&p, e))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).map_err(|e| err(&p, e))
    }
}

/// Regular files below `dir` that are dynamic programs or shared libraries.
fn elves(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    for e in fs::read_dir(dir).map_err(|e| err(dir, e))?.flatten() {
        let ft = e.file_type().map_err(|x| err(&e.path(), x))?;
        if ft.is_dir() {
            elves(&e.path(), out)?;
        } else if ft.is_file() && !matches!(elf::kind(&e.path()), Kind::Other) {
            out.push(e.path());
        }
    }
    Ok(())
}

/// `t` (relative to folder `dir`, both inside the tree) as a clean tree path; None when it leaves the tree.
pub fn normal(dir: &str, t: &str) -> Option<String> {
    let mut v: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    for c in t.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                v.pop()?;
            }
            c => v.push(c),
        }
    }
    Some(v.join("/"))
}

/// The relative link from folder `dir` to tree path `to`.
pub fn relative(dir: &str, to: &str) -> String {
    let a: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    let b: Vec<&str> = to.split('/').collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len() - common];
    parts.extend(&b[common..]);
    parts.join("/")
}

/// SRC -> OUT (a new folder). Returns the report lines.
pub fn import(src: &Path, out: &Path, opt: &Import) -> Result<Vec<String>, String> {
    if fs::symlink_metadata(out).is_ok() {
        return Err(format!("{}: exists already (import makes a new folder)", out.display()));
    }
    if !fs::metadata(src).map_err(|e| err(src, e))?.is_dir() {
        return Err(format!("{}: not a folder", src.display()));
    }
    fs::create_dir_all(out).map_err(|e| err(out, e))?;
    let mut r = Run { src, out, opt, report: Vec::new() };
    r.copy_tree("", 0)?;
    r.libs()?;
    if let Some(l) = &opt.launcher {
        r.launcher(l)?;
    }
    Ok(r.report)
}
