//! PK-0 under a scratch root (`--once`): the line init writes to /run/bitsos/apps-state for each data-state the
//! kernel can publish, /data/apps made, the bind itself skipped (only the system's init mounts).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(root: &Path) -> String {
    run_env(root, &[])
}

fn run_env(root: &Path, env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_init"));
    cmd.args(["--once", "--net-file", "/nonexistent-pk0", "--root"]).arg(root).env_remove("INIT_TEST_APPS_WAIT_S");
    let out = cmd.envs(env.iter().copied()).output().expect("spawn init");
    assert_eq!(out.status.code(), Some(0));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fresh(name: &str, data_state: Option<&str>) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("init-apps-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("run/bitsos")).unwrap();
    if let Some(t) = data_state {
        fs::write(root.join("run/bitsos/data-state"), t).unwrap();
    }
    root
}

fn state(root: &Path) -> String {
    fs::read_to_string(root.join("run/bitsos/apps-state")).unwrap_or_default()
}

#[test]
fn apps_state_lines() {
    let r = fresh("none", None);
    let out = run(&r);
    assert_eq!(state(&r), "waiting: data volume not announced yet\n", "{out}");
    assert!(!r.join("data/apps").exists());

    let r = fresh("img", Some("mounted pool rw\n"));
    fs::create_dir_all(r.join("boot/BITSOS2")).unwrap();
    fs::File::create(r.join("boot/BITSOS2/data.img")).unwrap().set_len(3 << 20).unwrap();
    let out = run(&r);
    assert_eq!(state(&r), "data: stick data.img 3 MB\n", "{out}");
    assert!(out.contains("/apps: bind mount of /data/apps skipped under a test root"), "{out}");
    assert!(out.contains("init: /data/apps: made (mode 755)"), "{out}");
    assert_eq!(fs::metadata(r.join("data/apps")).unwrap().permissions().mode() & 0o7777, 0o755);
    assert!(r.join("apps").is_dir());
    // A second boot finds it and writes nothing new.
    let out = run(&r);
    assert!(!out.contains("made (mode 755)") && !out.contains("apps-state:"), "{out}");

    // blocks= times 512 B: the assumed block size (an open item in plan/notes/desk-offen.md, group G3, makes it exact).
    let r = fresh("lexar", Some("mounted pool rw\n"));
    fs::create_dir_all(r.join("proc/bitsos")).unwrap();
    fs::write(r.join("proc/bitsos/data-window"), "window=armed disk=nvme0n1 part=nvme0n1p5 first=2048 blocks=1048576000\n").unwrap();
    let out = run(&r);
    assert_eq!(state(&r), "data: lexar partition 512000 MB\n", "{out}");

    let r = fresh("off", Some("skipped data_img_mb=0\n"));
    let out = run(&r);
    assert_eq!(state(&r), "tmpfs: no data volume (data_img_mb=0)\n", "{out}");
    assert!(out.contains("/apps: stays the initramfs tmpfs (no data volume (data_img_mb=0))"), "{out}");
    assert!(!r.join("data/apps").exists());

    let r = fresh("ro", Some("mounted pool ro\n"));
    run(&r);
    assert_eq!(state(&r), "tmpfs: data volume read-only\n");

    let r = fresh("plain", Some("mounted plain rw\n"));
    let out = run(&r);
    assert_eq!(state(&r), "data: unknown volume, size unknown\n", "{out}");
    assert!(r.join("data/apps").is_dir());

    let r = fresh("failed", Some("failed the image holds no pool\n"));
    run(&r);
    assert_eq!(state(&r), "tmpfs: data volume failed (the image holds no pool)\n");

    let r = fresh("late", Some("waiting\n"));
    let out = run(&r);
    assert_eq!(state(&r), "waiting: data volume waiting\n", "{out}");
    let out = run_env(&r, &[("INIT_TEST_APPS_WAIT_S", "0")]);
    assert_eq!(state(&r), "tmpfs: data volume still waiting after 0 s\n", "{out}");
    assert!(!r.join("data/apps").exists());

    // B3: a group- or world-writable /data/apps is set back to 755 before the bind.
    let r = fresh("mode", Some("mounted pool rw\n"));
    fs::create_dir_all(r.join("data/apps")).unwrap();
    fs::set_permissions(r.join("data/apps"), fs::Permissions::from_mode(0o777)).unwrap();
    let out = run(&r);
    assert!(out.contains("init: /data/apps: mode 777 set back to 755"), "{out}");
    assert_eq!(fs::metadata(r.join("data/apps")).unwrap().permissions().mode() & 0o7777, 0o755);

    // A link at /data/apps never carries /apps elsewhere.
    let r = fresh("link", Some("mounted pool rw\n"));
    fs::create_dir_all(r.join("data")).unwrap();
    std::os::unix::fs::symlink("/", r.join("data/apps")).unwrap();
    run(&r);
    assert_eq!(state(&r), "tmpfs: /data/apps is a symlink, not a directory\n");
    for n in ["none", "img", "lexar", "off", "ro", "plain", "failed", "late", "mode", "link"] {
        let _ = fs::remove_dir_all(Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("init-apps-{n}-{}", std::process::id())));
    }
}
