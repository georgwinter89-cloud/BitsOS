//! PK-4: `bitspkg import` turns a CachyOS program folder into a package tree: links rewritten or dropped,
//! setuid/non-ASCII/excluded names left out, the library closure copied in (libpath and view layouts); the
//! package then installs, runs from /apps-like storage through its wrapper, and removes without leftovers.

mod common;

use bitspkg::foreign::{Import, Layout, import, normal, read_libset, relative};
use bitspkg::store::Store;
use bitspkg::{build, install, remove, snapshot};
use common::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

fn opt(layout: Layout) -> Import {
    let libset = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/libset.txt");
    Import { origin: None, system: read_libset(&libset).unwrap(), dlopen: Vec::new(), exclude: Vec::new(), layout, launcher: None }
}

#[test]
fn paths() {
    assert_eq!(normal("lib", "../usr/lib/x.so.1").as_deref(), Some("usr/lib/x.so.1"));
    assert_eq!(normal("lib", "../../etc"), None);
    assert_eq!(normal("", "usr/./lib//x"), Some("usr/lib/x".into()));
    assert_eq!(relative("usr/lib", "usr/lib/x.so.1"), "x.so.1");
    assert_eq!(relative("usr/lib", "usr/share/a"), "../share/a");
    assert_eq!(relative("", "bin/x"), "bin/x");
    assert_eq!(relative("a/b", "c"), "../../c");
}

#[test]
fn links_names_and_setuid() {
    let d = tmp("import-tree");
    let src = d.join("src");
    write(&src.join("usr/lib/libx.so.1"), "not an elf\n", 0o644);
    write(&src.join("opt/app/data.txt"), "data\n", 0o644);
    write(&src.join("bin/helper"), "#!/bin/sh\n", 0o755);
    fs::set_permissions(src.join("bin/helper"), fs::Permissions::from_mode(0o4755)).unwrap();
    write(&src.join("share/caf\u{e9}.txt"), "x\n", 0o644);
    write(&src.join("cron/job"), "x\n", 0o644);
    symlink("/usr/lib/libx.so.1", src.join("usr/lib/libx.so")).unwrap();
    symlink("/opt/app/data.txt", src.join("usr/lib/data")).unwrap();
    symlink("/etc/passwd", src.join("usr/lib/passwd")).unwrap();
    symlink("../../..", src.join("usr/lib/up")).unwrap();
    symlink("libx.so.1", src.join("usr/lib/libx.so.1.0")).unwrap();
    let mut o = opt(Layout::LibPath);
    o.exclude = vec!["cron".into()];
    let rep = import(&src, &d.join("out"), &o).unwrap().join("\n");
    let out = d.join("out");
    assert_eq!(fs::read_link(out.join("usr/lib/libx.so")).unwrap(), PathBuf::from("libx.so.1"), "{rep}");
    assert_eq!(fs::read_link(out.join("usr/lib/data")).unwrap(), PathBuf::from("../../opt/app/data.txt"));
    assert_eq!(fs::read_link(out.join("usr/lib/libx.so.1.0")).unwrap(), PathBuf::from("libx.so.1"));
    for gone in ["usr/lib/passwd", "usr/lib/up", "bin/helper", "share/caf\u{e9}.txt", "cron"] {
        assert!(fs::symlink_metadata(out.join(gone)).is_err(), "{gone} must be left out:\n{rep}");
    }
    for line in ["dropped link usr/lib/passwd", "dropped link usr/lib/up", "left out bin/helper (setuid", "left out share/caf", "excluded cron"] {
        assert!(rep.contains(line), "report lacks {line:?}:\n{rep}");
    }
    // `--origin`: a vendor folder's absolute links to itself.
    let v = d.join("vendor");
    write(&v.join("brave"), "x\n", 0o755);
    symlink("/opt/brave-bin/brave", v.join("brave-browser")).unwrap();
    let mut o = opt(Layout::LibPath);
    o.origin = Some("/opt/brave-bin".into());
    import(&v, &d.join("vout"), &o).unwrap();
    assert_eq!(fs::read_link(d.join("vout/brave-browser")).unwrap(), PathBuf::from("brave"));
    // What import gives, build takes.
    build::build(&out, Some(&write_spec(&d, "tree")), Some(&d.join("pkg"))).unwrap();
}

fn write_spec(d: &Path, name: &str) -> PathBuf {
    let p = d.join(format!("{name}.toml"));
    fs::write(&p, format!("format = 1\nname = \"{name}\"\nversion = \"1\"\n")).unwrap();
    p
}

/// jq (a dynamic CachyOS program with a library the V2 image lacks, libonig) as a package in both layouts:
/// installed, run through its wrapper from the store, removed: the store tree as before. Without jq the same
/// with coreutils' ls (always there; its libraries may all be glibc's), so the closure is never untested.
#[test]
fn a_cachyos_program_runs_from_its_package() {
    let jq = Path::new("/usr/bin/jq").is_file();
    let (prog, args, want, lib): (&str, &[&str], &str, Option<&str>) =
        if jq { ("/usr/bin/jq", &["-n", "-c", "[1,2,3]|add"], "6", Some("libonig.so.5")) } else { ("/usr/bin/ls", &["-d", "/"], "/", None) };
    eprintln!("program: {prog}");
    let d = tmp("import-jq");
    let src = d.join("src");
    fs::create_dir_all(src.join("bin")).unwrap();
    fs::copy(prog, src.join("bin/jq")).unwrap();
    let st = Store::new(&d.join("root"));
    st.lock().unwrap();
    let before = snapshot::take(&d.join("root")).unwrap();
    for (n, layout) in [("libpath", Layout::LibPath), ("view", Layout::View)] {
        let mut o = opt(layout);
        o.launcher = Some("bin/jq".into());
        let tree = d.join(format!("tree-{n}"));
        let rep = import(&src, &tree, &o).unwrap().join("\n");
        if let Some(lib) = lib {
            assert!(rep.contains(lib), "{rep}");
        }
        assert_eq!(rep.contains("libc.so.6"), layout == Layout::View, "glibc only in the view layout:\n{rep}");
        fs::write(tree.join("package.toml"), format!("format = 1\nname = \"jq-{n}\"\nversion = \"1.8\"\nprogram = \"bitsos/run\"\n")).unwrap();
        build::build(&tree, None, Some(&d.join(format!("pkg-{n}")))).unwrap();
        install::install(&st, &d.join(format!("pkg-{n}"))).unwrap();
        let run = st.pkg(&format!("jq-{n}")).join("current/bitsos/run");
        let mut c = Command::new(&run);
        c.args(args).env_remove("LD_LIBRARY_PATH");
        let o = c.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), want, "{n}: {}", String::from_utf8_lossy(&o.stderr));
        // Started by a relative path from its own folder, the wrapper still finds the package (absolute T).
        let mut c = Command::new("sh");
        c.arg("bitsos/run").args(args).current_dir(st.pkg(&format!("jq-{n}")).join("current")).env_remove("LD_LIBRARY_PATH");
        let o = c.output().unwrap();
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), want, "{n} (relative): {}", String::from_utf8_lossy(&o.stderr));
        if layout == Layout::View {
            // Every library and the loader come from the package.
            let v = st.pkg("jq-view").join("current");
            let l = Command::new(v.join("usr/lib/ld-linux-x86-64.so.2")).arg("--library-path").arg(v.join("usr/lib")).arg("--list").arg(v.join("bin/jq")).output().unwrap();
            let l = String::from_utf8_lossy(&l.stdout).to_string();
            for line in l.lines().filter(|x| x.contains("=>")) {
                assert!(line.contains(&v.to_string_lossy().to_string()), "not from the package: {line}\n{l}");
            }
        }
        remove::remove(&st, &format!("jq-{n}"), false).unwrap();
    }
    let left = snapshot::diff(&before, &snapshot::take(&d.join("root")).unwrap(), &["apps/.next-id"]);
    assert!(left.is_empty(), "{left:#?}");
}

/// A foreign file whose DT_NEEDED is a path (`sub/x.so`, `../../x.so`, `/etc/x.so`) cannot make import write
/// outside the lib folder: refused before anything is copied for it. A launcher path the wrapper could not quote
/// safely is refused too. (ls with its DT_NEEDED libcap.so.2 patched; skipped where ls does not need libcap.)
#[test]
fn hostile_library_names_and_launcher_paths_are_refused() {
    let ls = fs::read("/usr/bin/ls").unwrap();
    let needle = b"libcap.so.2\0";
    if ls.windows(needle.len()).filter(|w| w == needle).count() != 1 {
        eprintln!("skip: /usr/bin/ls does not need libcap.so.2 here");
        return;
    }
    for (i, bad) in [&b"sub/libc.so\0"[..], b"../../ab.so\0", b"/etc/ab.so2\0"].iter().enumerate() {
        let d = tmp(&format!("import-hostile-{i}"));
        let src = d.join("src");
        fs::create_dir_all(src.join("bin")).unwrap();
        let pos = ls.windows(needle.len()).position(|w| w == needle).unwrap();
        let mut b = ls.clone();
        b[pos..pos + needle.len()].copy_from_slice(bad);
        write_bytes(&src.join("bin/ls"), &b);
        for layout in [Layout::LibPath, Layout::View] {
            let tree = d.join(format!("tree-{}", layout == Layout::View));
            let e = import(&src, &tree, &opt(layout)).err().unwrap_or_default();
            // Refused by import's name check, or earlier by the loader when the path does not resolve here.
            assert!(e.contains("not a library file name") || e.contains("cannot open shared object"), "{e}");
            assert!(!d.join("ab.so").exists() && !tree.join("lib/sub").exists() && !tree.join("usr/lib/sub").exists());
        }
    }
    let d = tmp("import-hostile-launcher");
    write(&d.join("src/bin/a$(x)"), "#!/bin/sh\n", 0o755);
    let mut o = opt(Layout::LibPath);
    o.launcher = Some("bin/a$(x)".into());
    assert!(import(&d.join("src"), &d.join("tree"), &o).unwrap_err().contains("only letters"));
    let mut o = opt(Layout::LibPath);
    o.dlopen = vec!["../libx.so".into()];
    assert!(import(&d.join("src"), &d.join("tree2"), &o).unwrap_err().contains("not a library file name"));
}

fn write_bytes(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}
