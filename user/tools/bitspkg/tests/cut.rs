//! "A cut during install leaves the old version working": every changing command is ended before each of its
//! steps in turn (BITSPKG_CUT_AT=N, the process exits at once as if the power went off). After each cut the old
//! state must still work, verify must be clean once the next command has recovered, and finishing the command
//! must give exactly the tree an uncut run gives.

mod common;

use common::*;
use std::path::Path;

/// Runs `args` cut at step 1, 2, ... until a run finishes; `check` sees the root after every cut.
fn cut_every_step(name: &str, setup: impl Fn(&Path, &Path), args: impl Fn(&Path) -> Vec<String>, check: impl Fn(&Path, u64)) {
    let d = tmp(name);
    let work = d.join("pkgs");
    std::fs::create_dir_all(&work).unwrap();
    // The reference: the same command without a cut.
    let ref_root = d.join("ref");
    setup(&ref_root, &work);
    let a = args(&work);
    let a: Vec<&str> = a.iter().map(String::as_str).collect();
    let (code, out, err) = run(&ref_root, &a, None);
    assert_eq!(code, 0, "uncut run: {out}{err}");
    let want = tree_list(&ref_root);
    let mut n = 1;
    loop {
        let root = d.join(format!("cut-{n}"));
        setup(&root, &work);
        let (code, out, err) = run(&root, &a, Some(n));
        if code == 0 {
            assert!(n > 1, "{name}: the command was never cut");
            break;
        }
        assert_eq!(code, bitspkg::cut::EXIT, "{name} step {n}: not a cut: {out}{err}");
        check(&root, n);
        // The next command recovers (gc drops a version the cut left unswitched); then verify is clean, and
        // finishing the command gives the uncut tree.
        let (code, out, err) = run(&root, &["gc"], None);
        assert_eq!(code, 0, "{name} step {n}: gc: {out}{err}");
        let (code, out, err) = run(&root, &["verify", "--store"], None);
        assert_eq!(code, 0, "{name} step {n}: verify after the cut: {out}{err}");
        let (code, out, err) = run(&root, &a, None);
        if code != 0 {
            // A removal the cut already completed cannot run again; the tree must still match.
            assert!(out.contains("not installed") || err.contains("not installed"), "{name} step {n}: rerun: {out}{err}");
        }
        assert_eq!(tree_list(&root), want, "{name} step {n}: the finished tree differs");
        let _ = std::fs::remove_dir_all(&root).or_else(|_| {
            make_writable(&root);
            std::fs::remove_dir_all(&root)
        });
        n += 1;
        assert!(n < 2000, "{name}: no end");
    }
    eprintln!("{name}: cut at each of {} steps", n - 1);
}

fn install_pkg(root: &Path, pkg: &Path) {
    let (code, out, err) = run(root, &["install", pkg.to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
}

fn pkg(work: &Path, v: &str) -> std::path::PathBuf {
    let p = work.join(format!("pkg-{v}"));
    if !p.exists() {
        package(work, v);
    }
    p
}

#[test]
fn cut_first_install() {
    cut_every_step(
        "cut-first",
        |_, _| {},
        |w| vec!["install".into(), pkg(w, "1.0").to_str().unwrap().into()],
        |root, n| {
            // Either nothing runs yet or the whole new version does: never half a program.
            match current_program(root) {
                None => assert!(!root.join("apps/demo/current").exists(), "step {n}: dangling current"),
                Some(p) => assert_eq!(p, "echo demo 1.0", "step {n}"),
            }
        },
    );
}

#[test]
fn cut_upgrade() {
    cut_every_step(
        "cut-upgrade",
        |root, w| install_pkg(root, &pkg(w, "1.0")),
        |w| vec!["install".into(), pkg(w, "2.0").to_str().unwrap().into()],
        |root, n| {
            let p = current_program(root).unwrap_or_else(|| panic!("step {n}: no current program"));
            assert!(p == "echo demo 1.0" || p == "echo demo 2.0", "step {n}: {p}");
            let v = std::fs::read_link(root.join("apps/demo/current")).unwrap();
            let lib = root.join("apps/demo").join(v).join("lib/libshared.so.1");
            assert_eq!(std::fs::read_to_string(lib).unwrap(), "shared library bytes\n", "step {n}");
        },
    );
}

#[test]
fn cut_upgrade_that_drops_an_old_version() {
    cut_every_step(
        "cut-third",
        |root, w| {
            install_pkg(root, &pkg(w, "1.0"));
            install_pkg(root, &pkg(w, "2.0"));
        },
        |w| vec!["install".into(), pkg(w, "3.0").to_str().unwrap().into()],
        |root, n| {
            let p = current_program(root).unwrap_or_else(|| panic!("step {n}: no current program"));
            assert!(p == "echo demo 2.0" || p == "echo demo 3.0", "step {n}: {p}");
        },
    );
}

#[test]
fn cut_rollback() {
    cut_every_step(
        "cut-rollback",
        |root, w| {
            install_pkg(root, &pkg(w, "1.0"));
            install_pkg(root, &pkg(w, "2.0"));
        },
        |_| vec!["rollback".into(), "demo".into()],
        |root, n| assert!(current_program(root).is_some(), "step {n}"),
    );
}

#[test]
fn cut_remove() {
    cut_every_step(
        "cut-remove",
        |root, w| {
            install_pkg(root, &pkg(w, "1.0"));
            install_pkg(root, &pkg(w, "2.0"));
            std::fs::write(root.join("data/appdata/demo/profile"), "x").unwrap();
        },
        |_| vec!["remove".into(), "demo".into()],
        |root, n| {
            // The package is either whole or gone.
            if root.join("apps/demo").exists() {
                assert_eq!(current_program(root).as_deref(), Some("echo demo 2.0"), "step {n}");
            }
        },
    );
}

#[test]
fn cut_remove_version() {
    cut_every_step(
        "cut-remove-version",
        |root, w| {
            install_pkg(root, &pkg(w, "1.0"));
            install_pkg(root, &pkg(w, "2.0"));
        },
        |_| vec!["remove".into(), "demo".into(), "--version".into(), "1.0".into()],
        |root, n| assert_eq!(current_program(root).as_deref(), Some("echo demo 2.0"), "step {n}"),
    );
}

/// What a power cut can leave beyond a clean stop between steps: a half-written store file, a torn staging
/// folder, a dangling `current.new`, temporaries of the id counter. The next command cleans them up, and the
/// next install of the same content is whole.
#[test]
fn torn_leftovers_are_cleaned_up() {
    let d = tmp("torn");
    let work = d.join("pkgs");
    std::fs::create_dir_all(&work).unwrap();
    let root = d.join("root");
    install_pkg(&root, &pkg(&work, "1.0"));
    let apps = root.join("apps");
    write(&apps.join(".store/.tmp-4242-1"), "shared lib", 0o444);
    write(&apps.join("demo/.new-2.0/share/notes.txt"), "notes fo", 0o444);
    std::os::unix::fs::symlink("2.0", apps.join("demo/current.new")).unwrap();
    std::os::unix::fs::symlink("1.0", apps.join("demo/previous.new")).unwrap();
    write(&apps.join("demo/id.new"), "10", 0o444);
    write(&apps.join(".next-id.new"), "1", 0o644);
    let (code, out, err) = run(&root, &["gc"], None);
    assert_eq!(code, 0, "{out}{err}");
    let left: Vec<String> = tree_list(&apps).into_iter().filter(|l| l.contains(".new") || l.contains(".tmp-")).collect();
    assert!(left.is_empty(), "leftovers: {left:?}");
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 1.0"));
    let (code, out, err) = run(&root, &["verify", "--store"], None);
    assert_eq!(code, 0, "{out}{err}");
    install_pkg(&root, &pkg(&work, "2.0"));
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    let (code, out, err) = run(&root, &["verify", "--store"], None);
    assert_eq!(code, 0, "{out}{err}");
}
