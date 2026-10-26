//! PK-5: verify over every hash of every version (read-only, inode-once, waits for a changing command), the space
//! report, and the B1 link (a program's version and data in a snapshot tree; VS-3 mounts the real one).

mod common;

use bitspkg::history::{self, SnapRoot, Step, Then};
use bitspkg::install;
use bitspkg::store::Store;
use bitspkg::{remove, snapshot, space, verify};
use common::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn other_package(d: &Path, version: &str) -> std::path::PathBuf {
    let t = d.join(format!("other-tree-{version}"));
    write(&t.join("package.toml"), &format!("format = 1\nname = \"other\"\nversion = \"{version}\"\nprogram = \"bin/other\"\n"), 0o644);
    write(&t.join("bin/other"), "#!/bin/sh\necho other\n", 0o755);
    // The same bytes as demo's lib/libshared.so.1.0: one store file for both programs.
    write(&t.join("lib/libshared.so.1.0"), "shared library bytes\n", 0o644);
    let out = d.join(format!("other-pkg-{version}"));
    bitspkg::build::build(&t, None, Some(&out)).unwrap();
    out
}

/// Distinct regular-file inodes below `dirs` and their apparent bytes.
fn unique_bytes(dirs: &[&Path]) -> (usize, u64) {
    let mut seen = HashSet::new();
    let mut bytes = 0;
    for d in dirs {
        for e in tree_list(d) {
            let Some(rel) = e.strip_prefix("f ") else { continue };
            let md = fs::symlink_metadata(d.join(rel)).unwrap();
            if seen.insert((md.dev(), md.ino())) {
                bytes += md.len();
            }
        }
    }
    (seen.len(), bytes)
}

fn chmod_w(p: &Path) {
    fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

#[test]
fn verify_reads_every_version_once_and_changes_nothing() {
    let d = tmp("pk5-verify");
    let root = d.join("root");
    let st = Store::new(&root);
    for v in ["1.0", "2.0", "3.0"] {
        install::install(&st, &package(&d, v)).unwrap();
    }
    assert_eq!(st.versions("demo").unwrap(), ["2.0", "3.0"]);
    let (bad, c) = verify::verify_counted(&st, None, true).unwrap();
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!((c.packages, c.versions, c.files), (1, 2, 6));
    // Each inode read once: the shared library and every version file is also the store file.
    let (_, want) = unique_bytes(&[&root.join("apps/demo/2.0"), &root.join("apps/demo/3.0"), &root.join("apps/.store")]);
    let manifests: u64 = ["2.0", "3.0"].iter().map(|v| fs::metadata(root.join(format!("apps/demo/{v}/manifest.toml"))).unwrap().len()).sum();
    assert_eq!(c.bytes_hashed, want - manifests, "manifest.toml is parsed, not hashed");
    assert_eq!(c.store_files, 5);

    // Damage in the previous (not current) version and a byte flip in a shared store file.
    let before = snapshot::take(&root).unwrap();
    let prev = root.join("apps/demo/2.0");
    make_writable(&prev);
    let notes = prev.join("share/notes.txt");
    fs::set_permissions(&notes, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    fs::write(&notes, "notes for 2.1\n").unwrap();
    fs::set_permissions(&notes, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let lib = root.join("apps/demo/3.0/lib/libshared.so.1.0");
    let store_dir = root.join("apps/.store");
    chmod_w(&store_dir);
    let mut bytes = fs::read(&lib).unwrap();
    bytes[0] ^= 1;
    // In place through the store name: every version linked to that inode sees it.
    let sha = fs::read_dir(&store_dir)
        .unwrap()
        .flatten()
        .find(|e| fs::metadata(e.path()).unwrap().ino() == fs::metadata(&lib).unwrap().ino())
        .unwrap()
        .path();
    fs::set_permissions(&sha, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    fs::write(&sha, &bytes).unwrap();
    fs::set_permissions(&sha, std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let damaged = snapshot::take(&root).unwrap();
    let bad = verify::verify(&st, None, true).unwrap().join("\n");
    for want in ["demo 2.0: share/notes.txt changed", "demo 2.0: lib/libshared.so.1.0 changed", "demo 3.0: lib/libshared.so.1.0 changed", "holds content"] {
        assert!(bad.contains(want), "verify missed {want:?}:\n{bad}");
    }
    assert!(!bad.contains("demo 3.0: share/notes.txt"), "{bad}");
    assert_eq!(snapshot::take(&root).unwrap(), damaged, "verify must not repair or write anything");
    assert_ne!(before, damaged);

    // A store copy that is gone is named (the version file itself is still whole).
    let notes3 = fs::metadata(root.join("apps/demo/3.0/share/notes.txt")).unwrap().ino();
    for e in fs::read_dir(&store_dir).unwrap().flatten() {
        if fs::metadata(e.path()).unwrap().ino() == notes3 {
            fs::remove_file(e.path()).unwrap();
        }
    }
    let bad = verify::verify(&st, Some("demo"), true).unwrap().join("\n");
    assert!(bad.contains("demo 3.0: share/notes.txt: store copy") && bad.contains("is missing"), "{bad}");
    assert!(verify::verify(&st, Some("demo"), false).unwrap().iter().all(|l| !l.contains("store copy")));

    let (code, out, err) = run(&root, &["verify", "--store"], None);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("BAD demo 2.0: share/notes.txt changed") && out.contains("problems (1 packages, 2 versions"), "{out}");
    assert!(err.contains("event=verify.bad"), "{err}");
}

#[test]
fn verify_reads_without_the_lock_and_rechecks_suspects_under_it() {
    let d = tmp("pk5-lock");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    let held = File::options().write(true).open(root.join("apps/.lock")).unwrap();
    held.lock().unwrap();
    // A clean store is read without the lock: a changing command holding it does not delay verify, and a long
    // verify never keeps install/register waiting.
    let (bad, c) = verify::verify_wait(&st, None, true, Duration::from_millis(200)).unwrap();
    assert!(bad.is_empty() && c.packages == 1 && c.files == 3, "{bad:?} {c:?}");

    // Half-built work of the lock holder: verify waits for the lock, then checks again and sees it finished.
    let vdir = root.join("apps/demo/1.0");
    chmod_w(&vdir);
    write(&vdir.join(".tmp-half"), "half", 0o644);
    let r2 = root.clone();
    let h = std::thread::spawn(move || verify::verify(&Store::new(&r2), None, true));
    std::thread::sleep(Duration::from_millis(400));
    assert!(!h.is_finished(), "verify reported half work instead of waiting");
    fs::remove_file(vdir.join(".tmp-half")).unwrap();
    held.unlock().unwrap();
    assert!(h.join().unwrap().unwrap().is_empty());

    // Real damage while the lock stays held beyond the wait: an error, not a guess.
    write(&vdir.join(".tmp-half"), "half", 0o644);
    held.lock().unwrap();
    let e = verify::verify_wait(&st, None, true, Duration::from_millis(300)).unwrap_err();
    assert!(e.contains("another bitspkg is still working"), "{e}");
    // A package removed while verify waited is no damage; `verify NAME` then says it is not installed.
    let r2 = root.clone();
    let all = std::thread::spawn(move || verify::verify_wait(&Store::new(&r2), None, true, Duration::from_secs(20)));
    let r3 = root.clone();
    let one = std::thread::spawn(move || verify::verify_wait(&Store::new(&r3), Some("demo"), false, Duration::from_secs(20)));
    std::thread::sleep(Duration::from_millis(400));
    let gone = d.join("gone");
    fs::rename(root.join("apps/demo"), &gone).unwrap();
    held.unlock().unwrap();
    let (bad, c) = all.join().unwrap().unwrap();
    assert!(bad.is_empty() && c.packages == 0, "{bad:?} {c:?}");
    let e = one.join().unwrap().unwrap_err();
    assert!(e.contains("demo is not installed"), "{e}");
    fs::rename(&gone, root.join("apps/demo")).unwrap();

    // No lock file (a copy of the store without it): the re-check runs unlocked and names the damage.
    fs::remove_file(root.join("apps/.lock")).unwrap();
    let bad = verify::verify(&st, None, true).unwrap();
    assert_eq!(bad, ["demo 1.0: .tmp-half is not part of the package"]);
    let (code, out, _) = run(&root, &["verify"], None);
    assert!(code == 1 && out.contains("BAD demo 1.0: .tmp-half"), "{out}");
    fs::remove_file(vdir.join(".tmp-half")).unwrap();
    let (code, out, _) = run(&root, &["verify"], None);
    assert_eq!(code, 0);
    assert!(out.starts_with("verify: all good (1 packages, 1 versions, 3 files"), "{out}");
}

#[test]
fn install_is_never_kept_waiting_by_verify_runs() {
    let d = tmp("pk5-busy");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runs: Vec<_> = (0..2)
        .map(|_| {
            let (r, stop) = (root.clone(), stop.clone());
            std::thread::spawn(move || {
                let st = Store::new(&r);
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    verify::verify(&st, None, true).unwrap();
                }
            })
        })
        .collect();
    let t = std::time::Instant::now();
    install::install(&st, &package(&d, "2.0")).unwrap();
    install::install(&st, &package(&d, "3.0")).unwrap();
    assert!(t.elapsed() < Duration::from_secs(10), "install waited {:?} for overlapping verifies", t.elapsed());
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for r in runs {
        r.join().unwrap();
    }
    assert!(verify::verify(&st, None, true).unwrap().is_empty());
}

#[test]
fn space_report_counts_shared_content_once() {
    let d = tmp("pk5-space");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    install::install(&st, &package(&d, "2.0")).unwrap();
    install::install(&st, &other_package(&d, "1")).unwrap();
    let data = root.join("data/appdata/demo");
    write(&data.join("profile"), "mine", 0o600);
    write(&data.join("Default/Cache/blob"), &"x".repeat(100), 0o600);
    write(&data.join("fontcache/f"), &"y".repeat(10), 0o600);
    // A hard link inside the data folder counts once.
    fs::hard_link(data.join("profile"), data.join("profile.bak")).unwrap();

    let (progs, s) = space::report(&st, None).unwrap();
    let demo = progs.iter().find(|p| p.name == "demo").unwrap();
    let other = progs.iter().find(|p| p.name == "other").unwrap();
    let lib = "shared library bytes\n".len() as u64;
    assert_eq!(demo.shared.bytes, lib);
    assert_eq!(other.shared.bytes, lib);
    assert_eq!(demo.own.bytes + demo.shared.bytes, demo.package.bytes);
    assert_eq!(demo.own.disk + demo.shared.disk, demo.package.disk);
    assert_eq!(demo.copies, 0);
    let roles: Vec<(&str, &str)> = demo.versions.iter().map(|v| (v.version.as_str(), v.role)).collect();
    assert_eq!(roles, [("1.0", "previous"), ("2.0", "current")]);
    // Version 1.0 alone: everything but the library (shared with 2.0 and with other).
    let v1 = &demo.versions[0];
    assert_eq!(v1.size.bytes - v1.alone.bytes, lib);
    // The package: both versions with the library once.
    assert_eq!(demo.package.bytes, demo.versions[0].size.bytes + demo.versions[1].size.bytes - lib);

    let dd = demo.data.as_ref().unwrap();
    let dirlen = |p: &str| fs::symlink_metadata(data.join(p)).unwrap().len();
    assert_eq!(dd.cache_dirs, ["Default/Cache", "fontcache"]);
    assert_eq!(dd.cache.bytes, 110 + dirlen("Default/Cache") + dirlen("fontcache"));
    assert_eq!(dd.size.bytes, 114 + dirlen("Default") + dirlen("Default/Cache") + dirlen("fontcache"));
    assert_eq!(dd.unreadable, 0);
    assert!(space::is_cache("GPUCache") && space::is_cache(".cache") && space::is_cache("Code Cache") && !space::is_cache("Cached Data x"));

    assert_eq!((s.unused_files, s.unused.bytes), (0, 0));
    // A store file nobody links to (what gc frees), and data kept after a removal.
    remove::remove(&st, "other", true).unwrap();
    chmod_w(&root.join("apps/.store"));
    fs::write(root.join("apps/.store").join("0".repeat(64)), "orphan").unwrap();
    let (progs, s) = space::report(&st, None).unwrap();
    assert_eq!(progs.len(), 1);
    assert_eq!(progs[0].shared.bytes, 0, "the library is demo's alone now");
    assert_eq!((s.unused_files, s.unused.bytes), (1, 6));
    assert_eq!(s.orphan_data.iter().map(|o| o.0.as_str()).collect::<Vec<_>>(), ["other"]);

    let (code, out, _) = run(&root, &["space"], None);
    assert_eq!(code, 0);
    for want in ["demo: package ", "  version 2.0 current: ", "  cache 1", " in Default/Cache, fontcache", "store unused: 1 files, 6 bytes", "data without program: other"] {
        assert!(out.contains(want), "space missed {want:?}:\n{out}");
    }
    let (code, out, _) = run(&root, &["space", "demo"], None);
    assert!(code == 0 && !out.contains("store:"), "{out}");
    let (code, _, err) = run(&root, &["space", "nope"], None);
    assert!(code == 1 && err.contains("nope is not installed"));

    // Symlinks are never followed: one to a big folder outside the root, a loop, and a data folder that is a
    // symlink itself change nothing but the link's own size.
    let big = d.join("outside");
    write(&big.join("huge"), &"z".repeat(100_000), 0o644);
    let before = space::report(&st, Some("demo")).unwrap().0[0].data.clone().unwrap();
    let (d0, c0) = (dirlen("Default"), dirlen("Default/Cache"));
    std::os::unix::fs::symlink(&big, data.join("Default/Cache/out")).unwrap();
    std::os::unix::fs::symlink("loop", data.join("loop")).unwrap();
    std::os::unix::fs::symlink(".", data.join("Default/self")).unwrap();
    let after = space::report(&st, Some("demo")).unwrap().0[0].data.clone().unwrap();
    let links: u64 = ["Default/Cache/out", "loop", "Default/self"].iter().map(|p| fs::symlink_metadata(data.join(p)).unwrap().len()).sum();
    // Folder sizes grow with their entries on some file systems (btrfs, tmpfs): that growth is counted too.
    let (dg, cg) = (dirlen("Default") - d0, dirlen("Default/Cache") - c0);
    assert_eq!(after.size.bytes, before.size.bytes + links + dg + cg);
    assert_eq!(after.cache.bytes, before.cache.bytes + fs::symlink_metadata(data.join("Default/Cache/out")).unwrap().len() + cg);
    assert_eq!(after.cache_dirs, before.cache_dirs);
    install::install(&st, &other_package(&d, "2")).unwrap();
    let od = root.join("data/appdata/other");
    if od.exists() {
        fs::remove_dir_all(&od).unwrap();
    }
    std::os::unix::fs::symlink(&big, &od).unwrap();
    let (progs, _) = space::report(&st, None).unwrap();
    assert!(progs.iter().find(|p| p.name == "other").unwrap().data.is_none(), "a symlinked data folder is not counted");
    // A package folder that is a symlink is not a package (as the listing says).
    std::os::unix::fs::symlink(root.join("apps/demo"), root.join("apps/fake")).unwrap();
    let (code, _, err) = run(&root, &["space", "fake"], None);
    assert!(code == 1 && err.contains("fake is not installed"), "{err}");
    // st_blocks 0 for a non-empty file (a file system that does not fill it) is counted by its length.
    let md = fs::symlink_metadata(data.join("profile")).unwrap();
    assert!(space::disk_of(&md) >= md.len());
}

/// A snapshot as VS-3 mounts it: the /data volume with apps/ and appdata/ (cp -a keeps links and modes).
fn snap_of(root: &Path, snap: &Path) {
    fs::create_dir_all(snap).unwrap();
    for (from, to) in [("apps", "apps"), ("data/appdata", "appdata")] {
        let ok = Command::new("cp").arg("-a").arg(root.join(from)).arg(snap.join(to)).status().unwrap().success();
        assert!(ok);
    }
}

#[test]
fn b1_link_finds_the_version_and_data_of_a_snapshot_and_brings_it_back() {
    let d = tmp("pk5-b1");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    install::install(&st, &package(&d, "2.0")).unwrap();
    fs::write(root.join("data/appdata/demo/profile"), "at 15:00").unwrap();
    let snap = d.join("snap");
    snap_of(&root, &snap);
    let sr = SnapRoot::volume(&snap);
    install::install(&st, &package(&d, "3.0")).unwrap();
    assert_eq!(st.versions("demo").unwrap(), ["2.0", "3.0"]);

    let at = history::program_at(&sr, &st, "demo").unwrap();
    let Then::Installed { current, previous, versions, program_id, current_dir, problem } = &at.then else { panic!("{at:?}") };
    assert_eq!((current.as_deref(), previous.as_deref()), (Some("2.0"), Some("1.0")));
    assert_eq!(versions, &["1.0", "2.0"]);
    assert_eq!(*program_id, Some(bitspkg::store::FIRST_ID));
    assert_eq!(current_dir.as_deref(), Some(snap.join("apps/demo/2.0").as_path()));
    assert!(problem.is_none() && !at.id_changed());
    assert_eq!(at.data_then.as_deref(), Some(snap.join("appdata/demo").as_path()));
    assert_eq!(at.data_now, root.join("data/appdata/demo"));
    assert_eq!(at.current_now.as_deref(), Some("3.0"));
    assert_eq!(at.step, Step::Switch("2.0".into()));
    // An answer that went stale (the live store changed after program_at): bring_back works the step out again
    // under the lock, sees the difference and does nothing.
    install::rollback(&st, "demo").unwrap();
    let e = history::bring_back(&st, &at).unwrap_err();
    assert!(e.contains("changed since"), "{e}");
    assert_eq!(st.current("demo").as_deref(), Some("2.0"));
    install::rollback(&st, "demo").unwrap();
    let before_snap = snapshot::take(&snap).unwrap();
    history::bring_back(&st, &at).unwrap();
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 2.0"));
    assert_eq!(st.previous("demo").as_deref(), Some("3.0"));
    assert_eq!(history::program_at(&sr, &st, "demo").unwrap().step, Step::Nothing);
    install::rollback(&st, "demo").unwrap();
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 3.0"), "rollback undoes it");

    // The snapshot's current is a version gone live: installed from the snapshot, linked into the live store.
    let snap_pkg = snap.join("apps/demo");
    chmod_w(&snap_pkg);
    fs::remove_file(snap_pkg.join("current")).unwrap();
    std::os::unix::fs::symlink("1.0", snap_pkg.join("current")).unwrap();
    let at = history::program_at(&sr, &st, "demo").unwrap();
    assert_eq!(at.step, Step::Install(snap.join("apps/demo/1.0")));
    let (code, out, _) = run(&root, &["at", snap.to_str().unwrap(), "demo"], None);
    assert_eq!(code, 0);
    for want in ["then installed", "then.current 1.0", "then.previous 1.0", "now.current 3.0", "step install ", "then.data "] {
        assert!(out.contains(want), "at missed {want:?}:\n{out}");
    }
    let (code, out, err) = run(&root, &["at", snap.to_str().unwrap(), "demo", "--apply"], None);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("done demo: installed 1.0 from the snapshot (was 3.0)"), "{out}");
    assert_eq!(current_program(&root).as_deref(), Some("echo demo 1.0"));
    assert!(verify::verify(&st, None, true).unwrap().is_empty());
    let ino = |v: &str| fs::metadata(root.join(format!("apps/demo/{v}/lib/libshared.so.1.0"))).unwrap().ino();
    assert_eq!(ino("1.0"), ino("3.0"), "the restored version shares the live store again");
    // The snapshot itself is never written.
    let mut now_snap = snapshot::take(&snap).unwrap();
    now_snap.remove("apps/demo/current");
    let mut was = before_snap.clone();
    was.remove("apps/demo/current");
    assert_eq!(now_snap, was);
}

#[test]
fn b1_link_unknown_absent_and_hostile_snapshots() {
    let d = tmp("pk5-b1-edge");
    let root = d.join("root");
    let st = Store::new(&root);
    install::install(&st, &package(&d, "1.0")).unwrap();
    let snap = d.join("snap");
    snap_of(&root, &snap);
    let sr = SnapRoot::volume(&snap);

    // apps/ not on the snapshotted volume: unknown, not an error and not a guess; data still found.
    let only_data = SnapRoot { apps: d.join("no-such/apps"), data: snap.join("appdata") };
    let at = history::program_at(&only_data, &st, "demo").unwrap();
    assert!(matches!(at.then, Then::Unknown(_)) && matches!(at.step, Step::Blocked(_)));
    assert!(at.data_then.is_some());
    assert!(history::bring_back(&st, &at).is_err());
    let (code, out, _) = run(&root, &["at", d.join("empty").to_str().unwrap(), "demo", "--snap-data", snap.join("appdata").to_str().unwrap()], None);
    assert!(code == 0 && out.contains(&format!("then.data {}", snap.join("appdata/demo").display())), "{out}");
    let (code, out, _) = run(&root, &["at", d.join("empty").to_str().unwrap(), "demo"], None);
    assert!(code == 0 && out.contains("then unknown ") && out.contains("then.data -") && out.contains("step blocked"), "{out}");

    // Not installed then: installed now -> remove (refused by bring_back), neither -> absent.
    let at = history::program_at(&sr, &st, "zed").unwrap();
    assert_eq!((at.then.clone(), at.step.clone()), (Then::NotInstalled, Step::Absent));
    install::install(&st, &other(&d)).unwrap();
    let at = history::program_at(&sr, &st, "zed").unwrap();
    assert_eq!(at.step, Step::Remove);
    assert!(history::bring_back(&st, &at).is_err());
    assert!(history::program_at(&sr, &st, "../demo").is_err());

    // Hostile snapshot: current leaves the folder, a program folder that is a symlink, a version folder holding
    // another program, a reused version name with other content, a FIFO as id.
    let pkg = snap.join("apps/demo");
    chmod_w(&pkg);
    fs::remove_file(pkg.join("current")).unwrap();
    std::os::unix::fs::symlink("../../../etc", pkg.join("current")).unwrap();
    let at = history::program_at(&sr, &st, "demo").unwrap();
    assert!(matches!(&at.then, Then::Installed { current: None, .. }) && matches!(at.step, Step::Blocked(_)), "{at:?}");
    std::os::unix::fs::symlink(&pkg, snap.join("apps/evil")).unwrap();
    assert_eq!(history::program_at(&sr, &st, "evil").unwrap().then, Then::NotInstalled);

    fs::remove_file(pkg.join("current")).unwrap();
    std::os::unix::fs::symlink("1.0", pkg.join("current")).unwrap();
    fs::remove_file(pkg.join("id")).unwrap();
    assert!(Command::new("mkfifo").arg(pkg.join("id")).status().unwrap().success());
    let v = pkg.join("1.0");
    make_writable(&v);
    let m = v.join("manifest.toml");
    fs::set_permissions(&m, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    let text = fs::read_to_string(&m).unwrap();
    fs::write(&m, format!("{text}\n")).unwrap();
    let at = history::program_at(&sr, &st, "demo").unwrap();
    assert!(matches!(&at.then, Then::Installed { program_id: None, .. }), "a FIFO id is not read");
    assert!(matches!(&at.step, Step::Blocked(w) if w.contains("other content")), "{at:?}");

    // Gone live and the snapshot folder holds another version: refused before anything is written.
    remove::remove(&st, "demo", true).unwrap();
    fs::write(&m, text.replace("version = \"1.0\"", "version = \"9.0\"")).unwrap();
    let at = history::program_at(&sr, &st, "demo").unwrap();
    assert_eq!(at.step, Step::Install(v.clone()));
    let live = snapshot::take(&root).unwrap();
    let e = history::bring_back(&st, &at).unwrap_err();
    assert!(e.contains("holds demo 9.0"), "{e}");
    assert_eq!(snapshot::take(&root).unwrap(), live);
}

fn other(d: &Path) -> std::path::PathBuf {
    let t = d.join("zed-tree");
    write(&t.join("package.toml"), "format = 1\nname = \"zed\"\nversion = \"1\"\nprogram = \"bin/zed\"\n", 0o644);
    write(&t.join("bin/zed"), "#!/bin/sh\n", 0o755);
    let out = d.join("zed-pkg");
    bitspkg::build::build(&t, None, Some(&out)).unwrap();
    out
}
