//! init on CachyOS under a scratch root: `--once` writes /etc from a DHCP view file, makes the directories,
//! skips mounts, starts no shell; a second pass rewrites nothing; a changed view rewrites resolv.conf and hosts.

use std::path::Path;
use std::process::Command;

fn run(root: &Path, net: &Path) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_init"))
        .args(["--once", "--root"])
        .arg(root)
        .arg("--net-file")
        .arg(net)
        .output()
        .expect("spawn init");
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn once_under_a_scratch_root() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("init-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let net = base.join("dhcp");
    std::fs::write(&net, "iface=eth0 mac=52:54:00:12:34:56 link=up dhcp=yes addr=10.0.2.15/24 router=10.0.2.2 dns=10.0.2.3 default=yes\n").unwrap();

    let (code, out) = run(&root, &net);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("init: BitsOS init starting"), "{out}");
    assert!(out.contains("/proc: not a mount point; mount skipped under a test root"), "{out}");
    assert!(out.contains("init: network: eth0 up 10.0.2.15/24 via 10.0.2.2 dns 10.0.2.3 default"), "{out}");
    assert!(out.contains("init: wrote /etc/resolv.conf (nameserver 10.0.2.3)"), "{out}");
    assert!(out.contains("no console shell"), "{out}");
    assert!(out.contains("one round done (--once, reaped 0)"), "{out}");
    let rd = |p: &str| std::fs::read_to_string(root.join(p)).unwrap_or_default();
    assert_eq!(rd("etc/resolv.conf"), "# written by BitsOS init from the DHCP lease of eth0\nnameserver 10.0.2.3\n");
    assert!(rd("etc/hosts").contains("\n10.0.2.15\t"), "{}", rd("etc/hosts"));
    assert!(!rd("etc/hostname").trim().is_empty());
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &str| std::fs::metadata(root.join(p)).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0);
    assert_eq!(mode("tmp"), 0o1777);
    assert_eq!(mode("dev/shm"), 0o1777);
    assert_eq!(mode("run"), 0o755);
    assert_eq!(mode("root"), 0o700);
    assert_eq!(mode("var/tmp"), 0o1777);
    assert_eq!(mode("var/spool/cron/crontabs"), 0o700);
    for (link, target) in [("usr/bin", "../bin"), ("usr/sbin", "../bin"), ("sbin", "bin"), ("var/run", "../run")] {
        assert_eq!(std::fs::read_link(root.join(link)).ok(), Some(target.into()), "{link}");
        assert!(out.contains(&format!("/{link}: link to {target}")), "{out}");
    }
    assert_eq!(mode("etc/resolv.conf"), 0o644);
    assert!(rd("etc/passwd").starts_with("root:x:0:0:root:/root:/bin/sh\n"), "{}", rd("etc/passwd"));
    assert!(rd("etc/group").starts_with("root:x:0:\n"), "{}", rd("etc/group"));
    // No temporary file left behind (named `<file>.init-new.<pid>`).
    let leftovers: Vec<_> = std::fs::read_dir(root.join("etc"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".init-new"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Nothing changed: nothing written.
    let (code, out) = run(&root, &net);
    assert_eq!(code, 0);
    assert!(!out.contains("wrote "), "{out}");
    assert!(out.contains("directories: all present"), "{out}");

    // A directory with another mode than the table's: said, not changed.
    std::fs::set_permissions(root.join("var/tmp"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let (code, out) = run(&root, &net);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("/var/tmp: exists with mode 755 instead of 1777; left alone"), "{out}");
    assert!(!out.contains("directories: all present"), "{out}");
    assert_eq!(mode("var/tmp"), 0o755);
    std::fs::set_permissions(root.join("var/tmp"), std::fs::Permissions::from_mode(0o1777)).unwrap();

    // A new lease: resolv.conf and hosts follow, hostname stays.
    std::fs::write(&net, "iface=eth0 link=up addr=192.168.100.113/24 router=192.168.100.1 dns=192.168.100.1 default=yes\n").unwrap();
    let (_, out) = run(&root, &net);
    assert!(out.contains("wrote /etc/resolv.conf (nameserver 192.168.100.1)"), "{out}");
    assert!(out.contains("wrote /etc/hosts"), "{out}");
    assert!(!out.contains("wrote /etc/hostname"), "{out}");

    // The view vanishes: init says so and writes the no-DNS resolv.conf.
    std::fs::remove_file(&net).unwrap();
    let (code, out) = run(&root, &net);
    assert_eq!(code, 0);
    assert!(out.contains("no DHCP view"), "{out}");
    assert!(rd("etc/resolv.conf").contains("no DNS server known yet"));
    let _ = std::fs::remove_dir_all(&base);
}
