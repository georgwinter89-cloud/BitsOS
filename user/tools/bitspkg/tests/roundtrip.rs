//! install / upgrade / rollback / remove round trips, content sharing, verify, gc, and hostile package folders.

mod common;

use bitspkg::install::{self, Outcome};
use bitspkg::store::Store;
use bitspkg::{remove, verify};
use common::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[test]
fn install_upgrade_rollback_remove() {
    let d = tmp("round");
    let root = d.join("root");
    let st = Store::new(&root);
    let (p1, p2, p3) = (package(&d, "1.0"), package(&d, "2.0"), package(&d, "3.0"));

    let r = install::install(&st, &p1).unwrap();
    assert_eq!(r.outcome, Outcome::Installed);
    assert_eq!(r.program_id, bitspkg::store::FIRST_ID);
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 1.0"));
    assert!(root.join("data/appdata/demo").is_dir());
    assert_eq!(fs::read_link(root.join("apps/demo/current/lib/libshared.so.1")).unwrap().to_str(), Some("libshared.so.1.0"));
    assert!(root.join("apps/demo/current/var/empty").is_dir());
    assert!(verify::verify(&st, None, true).unwrap().is_empty());

    // Data written by the program survives upgrades.
    fs::write(root.join("data/appdata/demo/profile"), "mine").unwrap();
    let r = install::install(&st, &p2).unwrap();
    assert_eq!(r.previous.as_deref(), Some("1.0"));
    assert_eq!(r.program_id, bitspkg::store::FIRST_ID, "the id stays with the program");
    assert_eq!(r.shared, 1, "the library is shared, not copied again");
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    let ino = |v: &str| fs::metadata(root.join(format!("apps/demo/{v}/lib/libshared.so.1.0"))).unwrap().ino();
    assert_eq!(ino("1.0"), ino("2.0"));

    install::install(&st, &p3).unwrap();
    assert_eq!(st.versions("demo").unwrap(), vec!["2.0", "3.0"], "only current and previous are kept");
    assert_eq!(install::rollback(&st, "demo").unwrap(), ("3.0".into(), "2.0".into(), 0));
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    assert_eq!(st.previous("demo").as_deref(), Some("3.0"));
    install::rollback(&st, "demo").unwrap();
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 3.0"));
    assert_eq!(fs::read_to_string(root.join("data/appdata/demo/profile")).unwrap(), "mine");

    // Installing the same version again only makes it current; other content under that version is refused.
    let again = install::install(&st, &p2).unwrap();
    assert_eq!(again.outcome, Outcome::AlreadyThere);
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    let t = tree(&d.join("other"), "2.0");
    write(&t.join("bin/demo"), "#!/bin/sh\necho changed\n", 0o755);
    let other = d.join("pkg-2.0-other");
    bitspkg::build::build(&t, None, Some(&other)).unwrap();
    assert!(install::install(&st, &other).unwrap_err().contains("other content"));

    assert!(remove::remove_version(&st, "demo", "2.0").unwrap_err().contains("current version"));
    remove::remove_version(&st, "demo", "3.0").unwrap();
    assert_eq!(st.previous("demo"), None);
    assert!(install::rollback(&st, "demo").unwrap_err().contains("no previous"));
    assert!(verify::verify(&st, None, true).unwrap().is_empty());

    let r = remove::remove(&st, "demo", false).unwrap();
    assert!(r.data_removed);
    assert_eq!(tree_list(&root), vec!["d apps", "d apps/.store", "d apps/.trash", "d data", "d data/appdata", "f apps/.lock", "f apps/.next-id"]);

    // A new install gets a new id: ids are never reused.
    let r = install::install(&st, &p1).unwrap();
    assert_eq!(r.program_id, bitspkg::store::FIRST_ID + 1);
    assert!(root.join("data/appdata/demo").is_dir());
    remove::remove(&st, "demo", true).unwrap();
    assert!(root.join("data/appdata/demo").is_dir(), "--keep-data keeps the data folder");
}

#[test]
fn installed_versions_are_read_only_and_verify_sees_changes() {
    let d = tmp("verify");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    let v = root.join("apps/demo/1.0");
    let mode = |p: &str| fs::symlink_metadata(v.join(p)).unwrap().mode() & 0o7777;
    assert_eq!(mode("bin/demo"), 0o555);
    assert_eq!(mode("share/notes.txt"), 0o444);
    assert_eq!(mode("bin"), 0o555);
    assert_eq!(mode("manifest.toml"), 0o444);

    // Tamper as the owner can (root could anyway): verify names each problem.
    make_writable(&v);
    let notes = v.join("share/notes.txt");
    fs::set_permissions(&notes, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    fs::write(&notes, "tampered\n").unwrap();
    fs::write(v.join("share/extra"), "x").unwrap();
    fs::remove_file(v.join("lib/libshared.so.1")).unwrap();
    let bad = verify::verify(&st, Some("demo"), true).unwrap();
    let all = bad.join("\n");
    for want in ["share/notes.txt has mode 644", "share/notes.txt changed", "share/extra is not part", "symlink lib/libshared.so.1 is missing", "store: "] {
        assert!(all.contains(want), "verify missed {want:?}:\n{all}");
    }
    let (code, out, _) = run(&root, &["verify"], None);
    assert_eq!(code, 1, "{out}");
}

#[test]
fn hostile_package_folders_are_refused_and_change_nothing() {
    let d = tmp("hostile");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    let before = tree_list(&root);

    // A file whose content does not match its hash.
    let p2 = package(&d, "2.0");
    fs::write(p2.join("share/notes.txt"), "swapped after build\n").unwrap();
    assert!(install::install(&st, &p2).unwrap_err().contains("does not match"));
    assert_eq!(tree_list(&root), before);
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 1.0"));

    // A package file replaced by a symlink to something outside.
    let p3 = package(&d, "3.0");
    fs::remove_file(p3.join("share/notes.txt")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", p3.join("share/notes.txt")).unwrap();
    assert!(install::install(&st, &p3).is_err());
    assert_eq!(tree_list(&root), before);

    // A package sub-folder that is a symlink to somewhere else.
    let p6 = package(&d, "6.0");
    let outside = d.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::rename(p6.join("share"), outside.join("share")).unwrap();
    std::os::unix::fs::symlink(outside.join("share"), p6.join("share")).unwrap();
    assert!(install::install(&st, &p6).unwrap_err().contains("not a folder of the package"));
    assert_eq!(tree_list(&root), before);

    // A FIFO where a file should be: refused at once, not an open that waits for a writer (with the lock held).
    let p7 = package(&d, "7.0");
    fs::remove_file(p7.join("share/notes.txt")).unwrap();
    let st_ok = std::process::Command::new("mkfifo").arg(p7.join("share/notes.txt")).status().unwrap();
    assert!(st_ok.success());
    let t0 = std::time::Instant::now();
    assert!(install::install(&st, &p7).unwrap_err().contains("not a regular file"));
    assert!(t0.elapsed() < std::time::Duration::from_secs(20));
    assert_eq!(tree_list(&root), before);

    // A missing file.
    let p4 = package(&d, "4.0");
    fs::remove_file(p4.join("bin/demo")).unwrap();
    assert!(install::install(&st, &p4).is_err());
    assert_eq!(tree_list(&root), before);

    // A manifest that is not a regular file.
    let p5 = d.join("pkg-link");
    fs::create_dir(&p5).unwrap();
    std::os::unix::fs::symlink(package(&d, "5.0").join("manifest.toml"), p5.join("manifest.toml")).unwrap();
    assert!(install::install(&st, &p5).unwrap_err().contains("not a regular file"));
    assert!(verify::verify(&st, None, true).unwrap().is_empty());
}

#[test]
fn build_refuses_what_cannot_be_packaged() {
    let d = tmp("build");
    let t = tree(&d, "1.0");
    std::os::unix::fs::symlink("/usr/lib/libc.so.6", t.join("lib/libc.so.6")).unwrap();
    assert!(bitspkg::build::build(&t, None, None).unwrap_err().contains("must be relative"));
    fs::remove_file(t.join("lib/libc.so.6")).unwrap();
    write(&t.join("caf\u{e9}"), "x", 0o644);
    assert!(bitspkg::build::build(&t, None, None).unwrap_err().contains("byte 0xc3"));
    fs::remove_file(t.join("caf\u{e9}")).unwrap();
    let spec = fs::read_to_string(t.join("package.toml")).unwrap();
    fs::write(t.join("package.toml"), format!("{spec}\n[[file]]\npath = \"x\"\n")).unwrap();
    assert!(bitspkg::build::build(&t, None, None).unwrap_err().contains("made by build"));
    // exclude keeps paths of the tree out (Brave's cron/ and its setuid chrome-sandbox, PACKAGES.md section 7).
    fs::write(t.join("package.toml"), spec.replace("program = ", "exclude = [\"share\", \"lib/libshared.so.1\"]\nprogram = ")).unwrap();
    let m = bitspkg::build::build(&t, None, Some(&d.join("excl"))).unwrap();
    assert_eq!(m.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(), vec!["bin/demo", "lib/libshared.so.1.0"]);
    assert!(m.symlinks.is_empty());
    // Every refused entry is listed in one message with a hint; setuid is refused, not silently dropped.
    std::os::unix::fs::symlink("/usr/lib/libc.so.6", t.join("lib/libc.so.6")).unwrap();
    write(&t.join("caf\u{e9}"), "x", 0o644);
    write(&t.join("bin/helper"), "#!/bin/sh\n", 0o755);
    fs::set_permissions(t.join("bin/helper"), fs::Permissions::from_mode(0o4755)).unwrap();
    let e = bitspkg::build::build(&t, None, None).unwrap_err();
    for want in ["3 entries", "bin/helper: setuid", "lib/libc.so.6: target", "byte 0xc3", "hint: list them in package.toml"] {
        assert!(e.contains(want), "{want}: {e}");
    }
    fs::write(t.join("package.toml"), spec.replace("program = ", "exclude = [\"bin/helper\", \"lib/libc.so.6\"]\nprogram = ")).unwrap();
    let e = bitspkg::build::build(&t, None, None).unwrap_err();
    assert!(e.contains("1 entry") && e.contains("byte 0xc3") && !e.contains("setuid"), "{e}");
    fs::remove_file(t.join("caf\u{e9}")).unwrap();
    fs::remove_file(t.join("lib/libc.so.6")).unwrap();
    fs::remove_file(t.join("bin/helper")).unwrap();
    fs::write(t.join("package.toml"), spec).unwrap();
    // In place: the manifest lands in the tree, and building again ignores it.
    let m = bitspkg::build::build(&t, None, None).unwrap();
    assert_eq!(bitspkg::build::build(&t, None, None).unwrap().to_text(), m.to_text());
    let st = Store::new(&d.join("root"));
    install::install(&st, &t).unwrap();
    assert!(verify::verify(&st, None, true).unwrap().is_empty());
}

#[test]
fn second_command_waits_for_the_lock() {
    let d = tmp("lock");
    let root = d.join("root");
    let st = Store::new(&root);
    let p1 = package(&d, "1.0");
    let held = st.lock().unwrap();
    let r2 = root.clone();
    let h = std::thread::spawn(move || run(&r2, &["install", p1.to_str().unwrap()], None));
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(!root.join("apps/demo/current").exists(), "installed while the lock was held");
    drop(held);
    let (code, out, err) = h.join().unwrap();
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("installed: demo 1.0"), "{out}");
}

#[test]
fn cli_round_trip() {
    let d = tmp("cli");
    let root = d.join("root");
    let t = tree(&d, "1.0");
    let pkg = d.join("pkg");
    let ok = |args: &[&str]| {
        let (code, out, err) = run(&root, args, None);
        assert_eq!(code, 0, "{args:?}: {out}{err}");
        out
    };
    assert!(ok(&["build", t.to_str().unwrap(), "--out", pkg.to_str().unwrap()]).contains("built demo 1.0: 3 files, 1 symlinks"));
    assert!(ok(&["check", pkg.to_str().unwrap()]).contains("ok demo 1.0"));
    ok(&["install", pkg.to_str().unwrap()]);
    let p2 = package(&d, "2.0");
    ok(&["install", p2.to_str().unwrap()]);
    assert_eq!(ok(&["list"]).trim(), "demo current 2.0 previous 1.0 id 100");
    let info = ok(&["info", "demo"]);
    for want in ["wants: net_connect proto=\"tcp\" port=443 why=\"fetches pages\"", "action: app.demo.open_url (change)", "setting: app.demo.homepage (text)"] {
        assert!(info.contains(want), "{info}");
    }
    assert!(ok(&["rollback", "demo"]).contains("back from 2.0 to 1.0"));
    assert!(ok(&["switch", "demo", "2.0"]).contains("current 2.0"));
    assert!(ok(&["verify", "--store"]).contains("all good"));
    ok(&["gc"]);
    assert!(ok(&["remove", "demo"]).contains("data folder removed"));
    assert_eq!(run(&root, &["frobnicate"], None).0, 2);
    assert_eq!(run(&root, &["remove", "../etc"], None).0, 1);
    // No command-line name or version reaches outside apps/<name>/.
    for args in [&["switch", "../x", "1.0"][..], &["switch", "demo", "../../x"], &["rollback", "a/b"], &["info", "../x"], &["info", "demo", "../y"]] {
        let (code, out, err) = run(&root, args, None);
        assert_eq!(code, 1, "{args:?}: {out}{err}");
        assert!(err.contains("bad package name") || err.contains("bad version"), "{args:?}: {err}");
    }
}

#[test]
fn a_bad_store_file_is_never_shared() {
    let d = tmp("badstore");
    let root = d.join("root");
    install_cli(&root, &package(&d, "1.0"));
    // The shared library's store file replaced by a new inode of the same size (bit rot, an owner's chmod and
    // write); version 1.0 keeps its own good inode.
    let lib = fs::read_dir(root.join("apps/.store")).unwrap().flatten().map(|e| e.path()).find(|p| {
        fs::read(p).map(|b| b == b"shared library bytes\n").unwrap_or(false)
    });
    let lib = lib.expect("store file of libshared");
    fs::remove_file(&lib).unwrap();
    write(&lib, "SHARED LIBRARY BYTES\n", 0o444);
    let (code, out, err) = run(&root, &["install", package(&d, "2.0").to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("event=store.bad"), "{err}");
    assert_eq!(fs::read_to_string(root.join("apps/demo/current/lib/libshared.so.1.0")).unwrap(), "shared library bytes\n");
    let (code, out, err) = run(&root, &["verify", "--store"], None);
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn clean_up_after_the_commit_point_never_fails_the_command() {
    let d = tmp("aftercommit");
    let root = d.join("root");
    install_cli(&root, &package(&d, "1.0"));
    // Another package's manifest is unreadable garbage: the store gc cannot know what it uses.
    write(&root.join("apps/zzz/1.0/manifest.toml"), "this is not toml [[[\n", 0o444);
    let (code, out, err) = run(&root, &["install", package(&d, "2.0").to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("event=gc.partial"), "{err}");
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    // Linked store files are all kept, so demo stays whole; verify names only the broken package.
    let (_, out, _) = run(&root, &["verify", "demo", "--store"], None);
    assert!(out.contains("all good"), "{out}");
    let (code, out, err) = run(&root, &["remove", "demo"], None);
    assert_eq!(code, 0, "{out}{err}");
}

fn install_cli(root: &std::path::Path, pkg: &std::path::Path) {
    let (code, out, err) = run(root, &["install", pkg.to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
}

#[test]
fn selftest_passes_here() {
    let d = tmp("selftest");
    let (code, out, err) = run(&d.join("root-unused"), &["selftest", d.to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.ends_with("selftest ok\n"), "{out}");
    assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0, "selftest leaves its folder empty");
}
