//! YT-2: a whole directory tree on the stick (`tree <dir> host:/abs/dir` in the manifest): every file below the
//! host directory goes to `\BITSOS2\<dir>\` with the same relative path (links followed, FAT has none; empty
//! directories are left out). The libraries the tree's programs need and the initramfs set lacks go to
//! `\BITSOS2\<dir>\lib\` (a program finds them through an RPATH `$ORIGIN/lib`; Cobalt's launcher
//! sets no LD_LIBRARY_PATH); what the set has stays in the initramfs only, so glibc is never there twice. A library a
//! program dlopens is the stage's business: put into the tree's lib/, its own needs come too (BR-2); one elsewhere in
//! the tree is not looked at (Cobalt's libcobalt.so has no DT_NEEDED at all).
//! Names must be safe on FAT and in the package list: no whitespace, none of `"*/:<>?\|`, no trailing dot,
//! at most 255 UTF-16 units (a FAT long name, linux-6.6/fs/fat/namei_vfat.c:597 FAT_LFN_LEN), and no two names in one
//! directory that differ only in case (a directory and a file included). A link is followed; a dangling one is an
//! error.

use super::{is_dynamic, is_shared_lib, ldd, ldd_in};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Where a tree's libraries go, below the tree.
pub const LIB_DIR: &str = "lib";
/// Names under `\BITSOS2\` a tree may not take (case-insensitive): the program directory, the system
/// generations, the user's home (YT-2 change request), and the two protected files.
const RESERVED: &[&str] = &["BIN", "SYS", "HOME", "STICK.ID", "CONFIG.TXT"];
/// Deeper than this is a link loop or not a program tree.
const MAX_DEPTH: usize = 32;

/// One FAT long-name component that is also one word of the package list.
fn component_ok(c: &str) -> bool {
    !c.is_empty()
        && c != "."
        && c != ".."
        && c.encode_utf16().count() <= 255
        && !c.ends_with('.')
        && c.chars().all(|ch| !ch.is_whitespace() && !ch.is_control() && !"\"*/:<>?\\|".contains(ch))
}

/// A tree's directory name under `\BITSOS2\`: one plain component, not reserved, not hidden.
pub fn name_ok(name: &str) -> bool {
    component_ok(name) && !name.starts_with('.') && !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name))
}

/// The files of the tree at `src`: (path relative to it, host file), sorted by path.
pub fn files(src: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let meta = std::fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", src.display()));
    }
    let mut out = Vec::new();
    walk(src, "", 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, rel: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("{}: deeper than {MAX_DEPTH} directories (a link loop?)", dir.display()));
    }
    let mut es: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    for e in es {
        let name = e.file_name().to_string_lossy().into_owned();
        if !component_ok(&name) {
            return Err(format!("{}: the name '{name}' is not safe on FAT or in the package list", dir.display()));
        }
        if let Some(other) = folded.insert(name.to_lowercase(), name.clone()) {
            return Err(format!("{}: '{other}' and '{name}' are the same name on FAT (case-insensitive)", dir.display()));
        }
        let path = e.path();
        let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let m = std::fs::metadata(&path).map_err(|e| match std::fs::symlink_metadata(&path) {
            Ok(l) if l.file_type().is_symlink() => format!("{}: a dangling link ({e})", path.display()),
            _ => format!("{}: {e}", path.display()),
        })?;
        if m.is_dir() {
            walk(&path, &r, depth + 1, out)?;
        } else if m.is_file() {
            out.push((r, path));
        } else {
            return Err(format!("{}: neither a file nor a directory", path.display()));
        }
    }
    Ok(())
}

/// The libraries `ldd` names for the tree's dynamic programs, and (BR-2) for the shared libraries the stage put into
/// the tree's lib/ because a program dlopens them (Brave's GTK 3: its own needs come along, found next to it first),
/// that are neither in `have` (the initramfs set's sonames) nor files of the tree itself: (soname, host file),
/// sorted by soname.
pub fn libs(src: &Path, files: &[(String, PathBuf)], have: &BTreeSet<String>) -> Result<Vec<(String, PathBuf)>, String> {
    let inside = std::fs::canonicalize(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let own_lib = inside.join(LIB_DIR);
    let mut want: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (rel, f) in files {
        let Some(head) = read_head(f) else { continue };
        let found = if is_dynamic(&head) {
            ldd(f)
        } else if rel.starts_with(&format!("{LIB_DIR}/")) && is_shared_lib(&head) {
            ldd_in(f, Some(&own_lib))
        } else {
            continue;
        };
        for (so, real) in found.map_err(|e| format!("{rel}: {e}"))? {
            if !have.contains(&so) && !real.starts_with(&inside) {
                want.entry(so).or_insert(real);
            }
        }
    }
    Ok(want.into_iter().collect())
}

/// The first 64 KiB of a file: enough for the ELF and program headers `is_dynamic` reads (never the whole of a
/// 124 MB library).
fn read_head(f: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    std::fs::File::open(f).ok()?.take(64 << 10).read_to_end(&mut b).ok()?;
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["cobalt", "nplb", "my-app_2", "a.b"] {
            assert!(name_ok(ok), "{ok}");
        }
        for bad in ["", ".", "..", ".hidden", "bin", "Bin", "SYS", "home", "stick.id", "Config.Txt", "a b", "a/b", "a:b", "x.", "a\\b", "a*"] {
            assert!(!name_ok(bad), "{bad}");
        }
    }

    fn scratch(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xt-tree-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn walks_sorted_and_refuses_fat_clashes() {
        let d = scratch("walk");
        std::fs::create_dir_all(d.join("app/content/fonts")).unwrap();
        std::fs::create_dir_all(d.join("empty")).unwrap();
        std::fs::write(d.join("loader"), b"x").unwrap();
        std::fs::write(d.join("app/content/fonts/B.ttf"), b"b").unwrap();
        std::fs::write(d.join("app/content/fonts/a.ttf"), b"a").unwrap();
        let f: Vec<String> = files(&d).unwrap().into_iter().map(|(r, _)| r).collect();
        assert_eq!(f, ["app/content/fonts/B.ttf", "app/content/fonts/a.ttf", "loader"]);
        std::fs::write(d.join("app/content/fonts/b.ttf"), b"c").unwrap();
        let e = files(&d).unwrap_err();
        assert!(e.contains("same name on FAT"), "{e}");
        std::fs::remove_file(d.join("app/content/fonts/b.ttf")).unwrap();
        std::fs::write(d.join("app/with space"), b"s").unwrap();
        assert!(files(&d).unwrap_err().contains("not safe"));
        std::fs::remove_file(d.join("app/with space")).unwrap();
        assert!(files(&d.join("loader")).is_err(), "a file is not a tree");
        // A directory and a file that differ only in case are one name on FAT too.
        std::fs::create_dir_all(d.join("app/Foo")).unwrap();
        std::fs::write(d.join("app/Foo/x"), b"x").unwrap();
        std::fs::write(d.join("app/foo"), b"f").unwrap();
        assert!(files(&d).unwrap_err().contains("same name on FAT"));
        std::fs::remove_file(d.join("app/foo")).unwrap();
        std::fs::remove_dir_all(d.join("app/Foo")).unwrap();
        // 255 UTF-16 units fit a FAT long name, 256 do not; a character outside the BMP counts twice.
        let long = "a".repeat(255);
        std::fs::write(d.join(&long), b"l").unwrap();
        assert!(files(&d).unwrap().iter().any(|(r, _)| r == &long));
        std::fs::remove_file(d.join(&long)).unwrap();
        // Longer names cannot come from a Linux host (NAME_MAX is 255 bytes), so the rule is checked directly.
        assert!(!component_ok(&"a".repeat(256)));
        assert!(!component_ok(&format!("{}\u{1F600}", "a".repeat(254))));
        assert!(component_ok(&format!("{}\u{1F600}", "a".repeat(253))));
        // A link is followed: the file it names is copied under the link's name; a dangling one fails.
        std::os::unix::fs::symlink(d.join("loader"), d.join("app/link")).unwrap();
        let f = files(&d).unwrap();
        let (_, host) = f.iter().find(|(r, _)| r == "app/link").expect("the link is listed");
        assert_eq!(std::fs::read(host).unwrap(), b"x");
        std::fs::remove_file(d.join("app/link")).unwrap();
        std::os::unix::fs::symlink(d.join("nowhere"), d.join("app/dangling")).unwrap();
        assert!(files(&d).unwrap_err().contains("dangling"));
        std::fs::remove_file(d.join("app/dangling")).unwrap();
        // A link loop ends at the depth limit.
        std::os::unix::fs::symlink(&d, d.join("app/loop")).unwrap();
        assert!(files(&d).unwrap_err().contains("deeper than"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A host program in a tree: the libraries the set has stay out, the others are named with their host file.
    #[test]
    fn libs_only_what_the_set_lacks() {
        let Ok(ls) = std::fs::canonicalize("/usr/bin/ls") else { return };
        if !super::super::have_tool("ldd") {
            return;
        }
        let d = scratch("libs");
        std::fs::copy(&ls, d.join("ls")).unwrap();
        let fs = files(&d).unwrap();
        let all: BTreeSet<String> = ldd(&ls).unwrap().into_iter().map(|(s, _)| s).collect();
        assert!(all.contains("libc.so.6"));
        let mut have = all.clone();
        let extra = all.iter().find(|s| !s.starts_with("ld-linux") && *s != "libc.so.6").cloned();
        if let Some(x) = &extra {
            have.remove(x);
        }
        let got: Vec<String> = libs(&d, &fs, &have).unwrap().into_iter().map(|(s, _)| s).collect();
        assert_eq!(got, extra.into_iter().collect::<Vec<_>>());
        assert!(libs(&d, &fs, &all).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// BR-2: a shared library the stage put into lib/ (a dlopened one) brings its needs; one of them that the tree's
    /// lib/ has already is taken from there; a library outside lib/ is the tree's own business.
    #[test]
    fn libs_of_a_dlopened_library() {
        let (Ok(cxx), Ok(gcc)) = (std::fs::canonicalize("/usr/lib/libstdc++.so.6"), std::fs::canonicalize("/usr/lib/libgcc_s.so.1")) else { return };
        if !super::super::have_tool("ldd") {
            return;
        }
        assert!(super::super::is_shared_lib(&read_head(&cxx).unwrap()));
        if let Ok(ls) = std::fs::canonicalize("/usr/bin/ls") {
            assert!(!super::super::is_shared_lib(&read_head(&ls).unwrap()), "a PIE program is not a library");
        }
        let d = scratch("dlopened");
        std::fs::create_dir_all(d.join("lib")).unwrap();
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::fs::copy(&cxx, d.join("other/libstdc++.so.6")).unwrap();
        let have: BTreeSet<String> = ["libc.so.6", "ld-linux-x86-64.so.2"].into_iter().map(String::from).collect();
        assert!(libs(&d, &files(&d).unwrap(), &have).unwrap().is_empty(), "outside lib/");
        std::fs::copy(&cxx, d.join("lib/libstdc++.so.6")).unwrap();
        let got: Vec<String> = libs(&d, &files(&d).unwrap(), &have).unwrap().into_iter().map(|(s, _)| s).collect();
        assert!(got.contains(&"libm.so.6".to_string()) && got.contains(&"libgcc_s.so.1".to_string()), "{got:?}");
        std::fs::copy(&gcc, d.join("lib/libgcc_s.so.1")).unwrap();
        let got: Vec<String> = libs(&d, &files(&d).unwrap(), &have).unwrap().into_iter().map(|(s, _)| s).collect();
        assert!(got.contains(&"libm.so.6".to_string()) && !got.contains(&"libgcc_s.so.1".to_string()), "{got:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
