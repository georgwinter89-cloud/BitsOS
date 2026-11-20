//! BR-9 on the host: Brave from the repositories (a fake brave-bin tree) is planned as the stick's launcher starts
//! Brave (`--plan`, no view is made); the stick's profile is taken over once; the theme rule; the flags equal the
//! launcher's.

use bitspkg::store::Store;
use bitsrun::browser::{self, FLAGS};
use bitsrun::browser_run::{MOVED_NOTE, take_over};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitsrun-browser-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn clean(d: &Path) {
    let _ = Command::new("chmod").arg("-R").arg("u+w").arg(d).status();
    fs::remove_dir_all(d).unwrap();
}

fn write(p: &Path, text: &str, mode: u32) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();
}

/// A store with a fake `brave-bin` (Arch's layout: the binary in opt/, the start script in usr/bin) and `probe`.
fn store(d: &Path) -> PathBuf {
    let root = d.join("store");
    fs::create_dir_all(&root).unwrap();
    for (name, files) in [
        ("brave-bin", vec!["root/opt/brave-bin/brave", "root/usr/bin/brave"]),
        ("probe", vec!["root/usr/bin/probe"]),
    ] {
        let t = d.join(format!("tree-{name}"));
        let prog = files.last().unwrap();
        write(&t.join("package.toml"), &format!("format = 1\nname = \"{name}\"\nversion = \"1\"\nen = \"BR-9 host test\"\nprogram = \"{prog}\"\n"), 0o644);
        for f in files {
            write(&t.join(f), "#!/bin/sh\n", 0o755);
        }
        write(&t.join("root/etc/x.conf"), "x\n", 0o644);
        let pkg = d.join(format!("pkg-{name}"));
        bitspkg::build::build(&t, None, Some(&pkg)).unwrap();
        bitspkg::install::install(&Store::new(&root), &pkg).unwrap();
    }
    root
}

fn plan(args: &[&OsStr], home: &Path) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_bitsrun")).args(args).env("HOME", home).env_remove("XCURSOR_SIZE").output().unwrap();
    (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
}

/// bitsrun refuses root, so a run as root could check nothing: that is a failure, never a pass.
fn not_root() {
    assert!(usys::geteuid() != Ok(0), "the BR-9 host tests run as a user (bitsrun refuses root): run cargo test as uid 1000");
}

/// A process that looks like the stick's Brave on profile `dir` (a script named brave), until its stdin closes.
fn fake_brave(d: &Path, dir: &Path) -> std::process::Child {
    let b = d.join("bin/brave");
    write(&b, "#!/bin/sh\nread x\n", 0o755);
    let c = Command::new(&b).arg("--no-first-run").arg(format!("--user-data-dir={}", dir.display())).stdin(std::process::Stdio::piped()).spawn().unwrap();
    // The script runs once its interpreter is in /proc with the arguments.
    let want = format!("--user-data-dir={}", dir.display());
    for _ in 0..200 {
        if fs::read(format!("/proc/{}/cmdline", c.id())).is_ok_and(|t| String::from_utf8_lossy(&t).contains(&want)) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    c
}

fn end(mut c: std::process::Child) {
    drop(c.stdin.take());
    c.wait().unwrap();
}

#[test]
fn flags_equal_the_stick_launchers() {
    let l = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ports/brave-launcher/bitsos/brave")).unwrap();
    let block = l.split_once("\nFLAGS=\"").unwrap().1.split_once('"').unwrap().0;
    let theirs: Vec<&str> = block.split_whitespace().collect();
    assert_eq!(theirs, FLAGS, "user/ports/brave-launcher/bitsos/brave's FLAGS and bitsrun's browser::FLAGS differ");
}

#[test]
fn theme_rule_as_the_launcher() {
    use browser::dark_now as d;
    assert!(d("dark", None, None, None) && !d("light", None, None, None) && !d("", None, None, None) && !d("bogus", Some("3"), None, None));
    // auto: light from 7 to 19 unless set.
    assert!(d("auto", Some("06"), None, None) && !d("auto", Some("07"), None, None) && !d("auto", Some("18"), None, None) && d("auto", Some("19"), None, None));
    // across midnight: light from 20 to 6.
    assert!(!d("auto", Some("23"), Some("20"), Some("6")) && !d("auto", Some("2"), Some("20"), Some("6")) && d("auto", Some("12"), Some("20"), Some("6")));
    // bad hours take the defaults, big ones clamp to 23 (light from 23 to 19: dark at 22, light at 23); an unreadable clock is hour 0 (dark with the defaults).
    assert!(d("auto", Some("x"), None, None) && d("auto", None, Some("-1"), Some("")) && d("auto", Some("22"), Some("99999999999999"), None) && !d("auto", Some("23"), Some("99999999999999"), None));
}

#[test]
fn brave_bin_starts_as_from_the_stick() {
    not_root();
    let d = tmp("plan");
    let st = store(&d);
    let s = st.as_os_str();
    let home = d.join("home");
    fs::create_dir_all(&home).unwrap();
    // The library's start of brave-browser.desktop, with the sofa's extra arguments.
    let (code, out, err) = plan(&[OsStr::new("--store"), s, "--exec".as_ref(), "/usr/bin/brave".as_ref(), "--plan".as_ref(), "brave-bin".as_ref(), "https://a.test/".as_ref(), "--".as_ref(), "--start-fullscreen".as_ref()], &home);
    assert_eq!(code, 0, "{err}");
    let run = out.lines().find(|l| l.starts_with("run ")).unwrap();
    assert_eq!(run, format!("run /opt/brave-bin/brave {} https://a.test/ --start-fullscreen", FLAGS.join(" ")), "{out}");
    for e in ["env GTK_USE_PORTAL=0", "env DBUS_SESSION_BUS_ADDRESS=disabled:", "env PULSE_SERVER=none", "env GSETTINGS_BACKEND=memory", "env XCURSOR_SIZE=32", "env NO_AT_BRIDGE=1"] {
        assert!(out.lines().any(|l| l == e), "{e}: {out}");
    }
    assert!(out.contains("note Brave: wie vom Stick gestartet") && !out.contains("--no-sandbox"), "{out}");
    let xdg = out.lines().find_map(|l| l.strip_prefix("env XDG_RUNTIME_DIR="));
    let tmpdir = out.lines().find_map(|l| l.strip_prefix("env TMPDIR="));
    assert_eq!(xdg, tmpdir, "TMPDIR is the runtime folder when there is one: {out}");
    assert!(out.lines().any(|l| l.starts_with("env XDG_CONFIG_HOME=") && l.ends_with("/.local/share/brave-bin/config")), "{out}");
    // NAME alone (the manifest's program, the start script) is the same browser; probe is untouched.
    let (code, out, err) = plan(&[OsStr::new("--store"), s, "--plan".as_ref(), "brave-bin".as_ref()], &home);
    assert!(code == 0 && out.contains("\nrun /opt/brave-bin/brave --ozone-platform=wayland"), "{out}{err}");
    let (code, out, err) = plan(&[OsStr::new("--store"), s, "--plan".as_ref(), "probe".as_ref(), "--".as_ref(), "x".as_ref()], &home);
    assert!(code == 0 && out.contains("\nrun /usr/bin/probe -- x\n") && !out.contains("TMPDIR") && !out.contains("Brave"), "{out}{err}");
    // The stick's profile: --plan only says it would be taken over; a running stick Brave refuses the start.
    let stick = st.join("data/appdata/brave");
    write(&stick.join("Local State"), "{}", 0o600);
    let (code, out, err) = plan(&[OsStr::new("--store"), s, "--plan".as_ref(), "brave-bin".as_ref()], &home);
    assert!(code == 0 && out.contains("wird beim Start nach") && stick.join("Local State").exists(), "{out}{err}");
    // A lock whose pid runs but is no Brave on that profile (a restart gave the pid to another program) is stale.
    std::os::unix::fs::symlink(format!("host-{}", std::process::id()), stick.join("SingletonLock")).unwrap();
    let (code, out, err) = plan(&[OsStr::new("--store"), s, "--plan".as_ref(), "brave-bin".as_ref()], &home);
    assert!(code == 0 && out.contains("wird beim Start nach"), "{out}{err}");
    let b = fake_brave(&d, &stick);
    fs::remove_file(stick.join("SingletonLock")).unwrap();
    std::os::unix::fs::symlink(format!("host-{}", b.id()), stick.join("SingletonLock")).unwrap();
    let (code, _, err) = plan(&[OsStr::new("--store"), s, "--plan".as_ref(), "brave-bin".as_ref()], &home);
    end(b);
    assert!(code == 1 && err.contains("Brave läuft noch mit dem alten Profil. Bitte dieses Brave schließen") && err.contains("event=pkg.run.profile.in_use"), "{err}");
    assert!(err.lines().filter(|l| !l.contains("event=")).all(|l| !l.contains("pid") && !l.contains('/')), "Georg's line names no pid or path: {err}");
    clean(&d);
}

#[test]
fn stick_profile_is_taken_over_once() {
    not_root();
    let d = tmp("take");
    let stick = d.join("appdata/brave");
    let target = d.join("appdata/brave-bin/config/BraveSoftware/Brave-Browser");
    // Nothing to take: no stick profile, or one without Local State.
    assert_eq!(take_over(&stick, &target, false), Ok(None));
    write(&stick.join("Default/Bookmarks"), "lesezeichen", 0o600);
    assert_eq!(take_over(&stick, &target, false), Ok(None));
    write(&stick.join("Local State"), "{}", 0o600);
    // A stale lock (a running pid that is no Brave: this test) and its socket go; the dry run moves nothing.
    std::os::unix::fs::symlink(format!("host-{}", std::process::id()), stick.join("SingletonLock")).unwrap();
    std::os::unix::fs::symlink("/tmp/x/SingletonSocket", stick.join("SingletonSocket")).unwrap();
    assert!(take_over(&stick, &target, true).unwrap().unwrap().contains("übernommen"));
    assert!(!target.exists());
    // A start that never finished left a folder: it is kept aside.
    write(&target.join("Default/half"), "", 0o600);
    let note = take_over(&stick, &target, false).unwrap().unwrap();
    assert!(note.contains("2 Einträge") && note.contains("before-takeover"), "{note}");
    assert_eq!(fs::read_to_string(target.join("Default/Bookmarks")).unwrap(), "lesezeichen");
    assert!(target.join("Local State").is_file() && d.join("appdata/brave-bin/config/BraveSoftware/Brave-Browser.before-takeover/Default/half").is_file());
    assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o700);
    let left: Vec<_> = fs::read_dir(&stick).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    assert_eq!(left, vec![MOVED_NOTE.to_string()], "only the note stays");
    assert!(fs::read_to_string(stick.join(MOVED_NOTE)).unwrap().contains("Brave-Browser"));
    // Once: the program's profile has its Local State now.
    write(&stick.join("Local State"), "{}", 0o600);
    assert_eq!(take_over(&stick, &target, false), Ok(None));
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_running_stick_brave_keeps_its_profile() {
    not_root();
    let d = tmp("busy");
    let stick = d.join("appdata/brave");
    let target = d.join("appdata/brave-bin/config/BraveSoftware/Brave-Browser");
    write(&stick.join("Local State"), "{}", 0o600);
    let b = fake_brave(&d, &stick);
    std::os::unix::fs::symlink(format!("host-{}", b.id()), stick.join("SingletonLock")).unwrap();
    let r = take_over(&stick, &target, false);
    end(b);
    assert_eq!(r, Err("Brave läuft noch mit dem alten Profil. Bitte dieses Brave schließen und Brave noch einmal öffnen.".into()));
    assert!(stick.join("Local State").is_file() && !target.exists());
    // The same pid after the Brave ended: stale, the profile moves.
    assert!(take_over(&stick, &target, false).unwrap().unwrap().contains("1 Einträge"));
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn lock_names_a_brave_on_that_profile_only() {
    use bitsrun::browser_run::brave_on;
    let p = Path::new("/data/appdata/brave");
    assert!(brave_on(b"/boot/BITSOS2/brave/brave\0--ozone-platform=wayland\0--user-data-dir=/data/appdata/brave\0", p));
    // Chromium's process title: one string with spaces.
    assert!(brave_on(b"/boot/BITSOS2/brave/brave --no-first-run --user-data-dir=/data/appdata/brave\0", p));
    assert!(!brave_on(b"/boot/BITSOS2/brave/brave\0--user-data-dir=/data/appdata/other\0", p));
    assert!(!brave_on(b"/bin/sleep\0--user-data-dir=/data/appdata/brave\0", p), "another program with that word");
    assert!(!brave_on(b"/opt/brave-bin/brave\0--type=renderer\0", p) && !brave_on(b"", p));
}

#[test]
fn two_starts_at_once_move_the_profile_once() {
    not_root();
    let d = tmp("race");
    let stick = d.join("appdata/brave");
    let target = d.join("appdata/brave-bin/config/BraveSoftware/Brave-Browser");
    write(&stick.join("Local State"), "{}", 0o600);
    write(&stick.join("Default/Bookmarks"), "lesezeichen", 0o600);
    // Another start holds the take-over and finishes it while this one waits.
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let held = fs::File::options().create(true).truncate(false).write(true).open(target.parent().unwrap().join(".takeover-lock")).unwrap();
    held.lock().unwrap();
    let (s2, t2) = (stick.clone(), target.clone());
    let waiting = std::thread::spawn(move || take_over(&s2, &t2, false));
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(!waiting.is_finished(), "the second start waits for the first one's take-over");
    write(&target.join("Local State"), "{}", 0o600);
    drop(held);
    assert_eq!(waiting.join().unwrap(), Ok(None), "the second start takes nothing over");
    assert_eq!(fs::read_to_string(stick.join("Default/Bookmarks")).unwrap(), "lesezeichen", "nothing moved twice");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_failed_move_is_undone() {
    not_root();
    let d = tmp("undo");
    let stick = d.join("appdata/brave");
    let target = d.join("appdata/brave-bin/config/BraveSoftware/Brave-Browser");
    write(&stick.join("Default/Bookmarks"), "lesezeichen", 0o600);
    write(&stick.join("Local State"), "{}", 0o600);
    // A folder moved to another parent needs write permission on itself ('..' changes,
    // linux-7.3.1/fs/namei.c:6120-6124): "Zz" comes last and cannot move.
    fs::create_dir_all(stick.join("Zz")).unwrap();
    fs::set_permissions(stick.join("Zz"), fs::Permissions::from_mode(0o500)).unwrap();
    let r = take_over(&stick, &target, false);
    fs::set_permissions(stick.join("Zz"), fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(r, Err("Das alte Brave-Profil lässt sich nicht übernehmen; es bleibt, wie es war. Brave startet nicht.".into()));
    let mut left: Vec<_> = fs::read_dir(&stick).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
    left.sort();
    assert_eq!(left, ["Default", "Local State", "Zz"], "the stick's profile is as it was");
    assert_eq!(fs::read_to_string(stick.join("Default/Bookmarks")).unwrap(), "lesezeichen");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0, "nothing stays in the program's profile");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn accessibility_bus_and_cursor_reach_brave() {
    use bitsrun::browser::{BRAVE, Facts, adjust};
    use bitsrun::plan::Plan;
    let base = || Plan {
        ops: Vec::new(),
        env: vec![("XDG_RUNTIME_DIR".into(), "/run/user/1000".into()), ("LANG".into(), "en_US.UTF-8".into())],
        chdir: "/".into(),
        argv: vec!["/usr/bin/brave".into(), "https://a.test/".into()],
        notes: Vec::new(),
    };
    let env = |p: &Plan, k: &str| p.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut p = base();
    adjust(&mut p, &BRAVE, &Facts { dark: true, a11y_bus: Some("/run/user/1000/at-spi/bus".into()), cursor_size: Some("48".into()) });
    let argv: Vec<String> = p.argv.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let mut want: Vec<String> = vec!["/opt/brave-bin/brave".into()];
    want.extend(FLAGS.iter().map(|f| f.to_string()));
    want.extend(["--force-dark-mode", "--force-renderer-accessibility", "https://a.test/"].map(String::from));
    assert_eq!(argv, want);
    assert_eq!(env(&p, "AT_SPI_BUS_ADDRESS").as_deref(), Some("unix:path=/run/user/1000/at-spi/bus"));
    assert_eq!(env(&p, "ACCESSIBILITY_ENABLED").as_deref(), Some("1"));
    assert_eq!(env(&p, "NO_AT_BRIDGE"), None);
    assert_eq!(env(&p, "XCURSOR_SIZE").as_deref(), Some("48"));
    assert_eq!(env(&p, "LANG").as_deref(), Some("en_US.UTF-8"), "the caller's language stays");
    assert_eq!(env(&p, "TMPDIR").as_deref(), Some("/run/user/1000"));
    // Without the bus: the bridge is off, no accessibility flag; without a runtime folder a note says so.
    let mut p = base();
    p.env.retain(|(k, _)| k != "XDG_RUNTIME_DIR");
    adjust(&mut p, &BRAVE, &Facts::default());
    assert!(!p.argv.iter().any(|a| a == "--force-renderer-accessibility" || a == "--force-dark-mode"));
    assert_eq!(env(&p, "NO_AT_BRIDGE").as_deref(), Some("1"));
    assert_eq!(env(&p, "AT_SPI_BUS_ADDRESS"), None);
    assert_eq!(env(&p, "XCURSOR_SIZE").as_deref(), Some("32"));
    assert!(env(&p, "TMPDIR").is_none() && p.notes.iter().any(|n| n.contains("kein Laufzeitordner")), "{:?}", p.notes);
}
