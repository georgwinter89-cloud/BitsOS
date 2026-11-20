//! The bitsrun command on the host (no view is made: `--plan` and refusals only): names, versions, non-UTF-8
//! input, the data folder's owner, the ProgramId file, HOME.

use bitspkg::store::Store;
use bitsrun::facts;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitsrun-cli-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// The store's version folders are read-only (B2): writable again, then removed.
fn clean(d: &Path) {
    let _ = Command::new("chmod").arg("-R").arg("u+w").arg(d).status();
    fs::remove_dir_all(d).unwrap();
}

fn write(p: &Path, text: &str, mode: u32) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

/// A store under `d/store` with `probe` 1 installed (a per-program root tree root/usr + root/etc).
fn store(d: &Path) -> PathBuf {
    let t = d.join("tree");
    write(
        &t.join("package.toml"),
        "format = 1\nname = \"probe\"\nversion = \"1\"\nen = \"PK-3 host test\"\nprogram = \"root/usr/bin/probe\"\n",
        0o644,
    );
    write(&t.join("root/usr/bin/probe"), "#!/bin/sh\n", 0o755);
    write(&t.join("root/etc/probe.conf"), "x\n", 0o644);
    // A second program of the tree (a .desktop file's Exec), started with --exec.
    write(&t.join("root/usr/bin/probe-two"), "#!/bin/sh\n", 0o755);
    write(&t.join("root/opt/probe/run"), "#!/bin/sh\n", 0o755);
    let pkg = d.join("pkg");
    bitspkg::build::build(&t, None, Some(&pkg)).unwrap();
    let root = d.join("store");
    fs::create_dir_all(&root).unwrap();
    bitspkg::install::install(&Store::new(&root), &pkg).unwrap();
    root
}

fn run<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(args: I, env: &[(&str, &OsStr)]) -> (i32, String, String) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_bitsrun"));
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

fn as_root() -> bool {
    usys::geteuid() == Ok(0)
}

#[test]
fn plan_names_versions_and_non_utf8_input() {
    let d = tmp("names");
    let st = store(&d);
    let s = st.to_str().unwrap();
    if as_root() {
        let (code, _, err) = run(["--store", s, "--plan", "probe"], &[]);
        assert!(code == 1 && err.contains("keine Programme als root") && err.contains("event=pkg.run.refused"), "{err}");
        clean(&d);
        return;
    }
    let (code, out, err) = run(["--store", s, "--plan", "probe", "a"], &[]);
    assert_eq!(code, 0, "{err}");
    let usr = format!("ro-bind {}/apps/probe/1/root/usr -> /usr", st.canonicalize().unwrap().display());
    assert!(out.contains(&usr) && out.contains("program_id 100 ") && out.contains("run /usr/bin/probe a"), "{out}");
    for (args, want) in [
        (vec!["../x"], "Programmname"),
        (vec!["a/b"], "Programmname"),
        (vec![""], "Programmname"),
        (vec!["nosuch"], "nicht installiert"),
        (vec!["--version", "../1", "probe"], "Version"),
        (vec!["--version", "current", "probe"], "Version"),
        (vec!["--version", "2", "probe"], "probe 2 ist nicht installiert"),
        (vec!["--version", "1:2.3-1", "probe"], "Epoche"),
        (vec!["--bogus", "probe"], "unbekannte Option"),
    ] {
        let mut a = vec!["--store", s, "--plan"];
        a.extend(args.iter().copied());
        let (code, _, err) = run(&a, &[]);
        assert!(code != 0 && err.contains(want), "{args:?}: {code} {err}");
        // PE-1: a refusal's line for the desktop's card carries the mark; usage errors stay plain.
        if code == 1 {
            assert!(err.lines().any(|l| l.starts_with("» ") && l.contains(want)), "{args:?}: no marked line: {err}");
        }
    }
    // A file name that is no UTF-8 reaches the program untouched; NAME must be UTF-8; a non-UTF-8 environment
    // entry is left out with a note, never a panic.
    let bad = OsStr::from_bytes(b"f\xffile");
    let args: Vec<OsString> = ["--store", s, "--plan", "probe"].iter().map(OsString::from).chain([bad.to_owned()]).collect();
    let (code, out, err) = run(&args, &[("LANG", OsStr::from_bytes(b"de\xff"))]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("run /usr/bin/probe f\u{fffd}ile") && out.contains("note Umgebung: Einträge ohne UTF-8 weggelassen: LANG"), "{out}");
    let args: Vec<OsString> = ["--store", s, "--plan"].iter().map(OsString::from).chain([bad.to_owned()]).collect();
    let (code, _, err) = run(&args, &[]);
    assert!(code == 1 && err.contains("kein UTF-8") && !err.contains("panicked"), "{err}");
    clean(&d);
}

#[test]
fn exec_starts_another_program_of_the_tree() {
    let d = tmp("exec");
    let st = store(&d);
    let s = st.to_str().unwrap();
    if as_root() {
        clean(&d);
        return;
    }
    // The desktop's start of a .desktop entry: the same view as NAME's program, only the program differs.
    for (exec, want) in [("probe-two", "run /usr/bin/probe-two --x"), ("/usr/bin/probe-two", "run /usr/bin/probe-two --x"), ("/opt/probe/run", "run /opt/probe/run --x")] {
        let (code, out, err) = run(["--store", s, "--exec", exec, "--plan", "probe", "--x"], &[]);
        assert_eq!(code, 0, "{exec}: {err}");
        assert!(out.contains(want) && out.contains("/apps/probe/1/root/usr -> /usr"), "{exec}: {out}");
    }
    for (exec, want) in [
        ("../../bin/sh", "--exec"),
        ("bin/probe", "--exec"),
        ("/usr/../etc/probe.conf", "--exec"),
        ("/etc/probe.conf", "--exec"),
        ("nosuch", "fehlt im Baum"),
    ] {
        let (code, _, err) = run(["--store", s, "--exec", exec, "--plan", "probe"], &[]);
        assert!(code != 0 && err.contains(want), "{exec}: {code} {err}");
    }
    let tree = d.join("tree/root");
    let (code, _, err) = run([OsStr::new("--root"), tree.as_os_str(), "--exec".as_ref(), "probe".as_ref(), "--plan".as_ref(), "--".as_ref(), "probe".as_ref()], &[]);
    assert!(code != 0 && err.contains("nicht mit --root"), "{err}");
    clean(&d);
}

#[test]
fn program_id_file_and_data_folder_owner() {
    let d = tmp("id");
    let st = store(&d);
    let s = st.to_str().unwrap();
    if as_root() {
        let (code, _, err) = run(["--store", s, "--plan", "probe"], &[]);
        assert!(code == 1 && err.contains("keine Programme als root"), "{err}");
        clean(&d);
        return;
    }
    let id = st.join("apps/probe/id");
    fs::remove_file(&id).unwrap();
    let (code, out, err) = run(["--store", s, "--plan", "probe"], &[]);
    assert!(code == 0 && out.contains("program_id - "), "{out}{err}");
    write(&id, "kaputt\n", 0o444);
    let (code, out, _) = run(["--store", s, "--plan", "probe"], &[]);
    assert!(code == 0 && out.contains("program_id - "), "{out}");
    // A data folder of another uid (here root's /) is refused with the line to type.
    let tree = st.join("apps/probe/1/root");
    let (code, _, err) = run([OsStr::new("--root"), tree.as_os_str(), "--name".as_ref(), "probe".as_ref(), "--data".as_ref(), "/".as_ref(), "--plan".as_ref(), "--".as_ref(), "probe".as_ref()], &[]);
    assert!(code == 1 && err.contains("gehört uid 0") && err.contains("chown") && err.contains("event=pkg.run.refused"), "{err}");
    let (code, _, err) = run([OsStr::new("--root"), tree.as_os_str(), "--data".as_ref(), "/".as_ref(), "--".as_ref(), "probe".as_ref()], &[]);
    assert!(code == 1 && err.contains("--data braucht --name"), "{err}");
    clean(&d);
}

#[test]
fn home_falls_back_to_passwd() {
    let d = tmp("home");
    let pw = ("u".to_string(), "/pw/home".to_string());
    let real = d.to_str().unwrap();
    assert_eq!(facts::home(Some(&format!("{real}/")), Some(&pw), 1000), real);
    for h in ["relative", "/", "/no/such/folder", &format!("{real}/../x")] {
        assert_eq!(facts::home(Some(h), Some(&pw), 1000), "/pw/home", "{h}");
    }
    assert_eq!(facts::home(None, Some(&pw), 1000), "/pw/home");
    assert_eq!(facts::home(None, None, 1000), "/data/home/1000");
    write(&d.join("passwd"), "root:x:0:0::/root:/bin/sh\nbad line\nuser:x:1000:1000::/data/home/1000:/bin/sh\nrel:x:7:7::home:/bin/sh\n", 0o644);
    assert_eq!(facts::passwd(&d.join("passwd"), 1000), Some(("user".into(), "/data/home/1000".into())));
    assert_eq!(facts::passwd(&d.join("passwd"), 7), None);
    fs::remove_dir_all(&d).unwrap();
}
