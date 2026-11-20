//! Tests of trust.rs on a made-up host in a temporary folder: packages packed by the host's bsdtar and a
//! stand-in verifier script (the real one runs in every image build). Without sh or bsdtar they fail.

use super::*;

const ARCH: &str = "1:20260909-1";
const CACHY: &str = "20240331-1";
const GOOD: &str = "echo \"$@\" >> \"$(dirname \"$0\")/args\"; echo 'gut ABCD Test Key <t@example.org>'";

/// Written by a child process: a file this process still holds open for writing in a forked sibling cannot be run.
fn script(at: &Path, body: &str) {
    let st = Command::new("sh").arg("-c").arg("printf '#!/bin/sh\\n%s\\n' \"$2\" > \"$1\" && chmod 755 \"$1\"").arg("sh").arg(at).arg(body).status().unwrap();
    assert!(st.success());
}

/// A host on which every check passes: versions/, host/ (the keyring folder), cache/ and the verifier `verify`.
fn fixture(test: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xt-trust-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["versions", "host", "cache"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    for ((package, files), version) in RINGS.iter().zip([ARCH, CACHY]) {
        std::fs::write(d.join("versions").join(package), version).unwrap();
        let tree = d.join("tree").join(package);
        std::fs::create_dir_all(tree.join(KEYRING_IN_PKG)).unwrap();
        for f in files {
            let body = format!("bytes of {f}");
            std::fs::write(tree.join(KEYRING_IN_PKG).join(f), &body).unwrap();
            std::fs::write(d.join("host").join(f), &body).unwrap();
        }
        let pkg = d.join("cache").join(pkg_file(package, version).unwrap());
        let st = Command::new("bsdtar").arg("-cf").arg(&pkg).arg("-C").arg(&tree).arg("usr").status().unwrap();
        assert!(st.success());
        std::fs::write(format!("{}.sig", pkg.display()), b"sig").unwrap();
    }
    script(&d.join("verify"), GOOD);
    d
}

fn run_with(d: &Path, bsdtar: &str) -> Result<Vec<Checked>, Fail> {
    let version = |n: &str| std::fs::read_to_string(d.join("versions").join(n)).ok();
    keyrings(&Host { keyrings: &d.join("host"), cache: &d.join("cache"), last: &d.join("last"), verifier: &d.join("verify"), bsdtar, version: &version })
}

fn run(d: &Path) -> Result<Vec<Checked>, Fail> {
    run_with(d, "bsdtar")
}

fn arch_pkg(d: &Path) -> PathBuf {
    d.join("cache").join(pkg_file("archlinux-keyring", ARCH).unwrap())
}

#[test]
fn versions_and_cache_names() {
    assert_eq!(q_version("archlinux-keyring 1:20260909-1\n", "archlinux-keyring").as_deref(), Some(ARCH));
    assert_eq!(q_version("cachyos-keyring 20240331-1\n", "archlinux-keyring"), None);
    assert_eq!(q_version("", "archlinux-keyring"), None);
    assert_eq!(pkg_file("archlinux-keyring", ARCH).as_deref(), Some("archlinux-keyring-1:20260909-1-any.pkg.tar.zst"));
    assert_eq!(pkg_file("cachyos-keyring", CACHY).as_deref(), Some("cachyos-keyring-20240331-1-any.pkg.tar.zst"));
    for bad in ["", "1/../x", "1 2", "1\n"] {
        assert_eq!(pkg_file("cachyos-keyring", bad), None, "{bad:?}");
    }
}

#[test]
fn only_active_plain_servers_are_copied() {
    let host = "# STARTED AT: 2026-11-06 02:29:52 +01:00\n# ARGS: rate-mirrors --save /tmp/tmp.x cachyos\n\
                Server = https://a.example/repo/$arch/$repo\n#Server = https://off.example/$repo\n  Server=http://b.example/$repo # 12 ms\n\
                Server = https://a.example/repo/$arch/$repo\nServer = file:///home/someone/mirror\nServer = https://me:secret@c.example/$repo\n\
                Server = https://d.example/x?key=1\nServer =\nInclude = /etc/pacman.d/other\n";
    let (keep, left_out) = servers(host);
    assert_eq!(keep, ["https://a.example/repo/$arch/$repo", "http://b.example/$repo"]);
    assert_eq!(left_out, 4);
    let text = list_text(&keep);
    assert_eq!(text.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>(), ["Server = https://a.example/repo/$arch/$repo", "Server = http://b.example/$repo"]);
    for word in ["STARTED", "rate-mirrors", "12 ms", "someone", "secret", "off.example"] {
        assert!(!text.contains(word), "{word} went into the image's list");
    }
    // What the image gets reads back as the same servers.
    assert_eq!(servers(&text), (keep, 0));
}

#[test]
fn mirror_list_refusals() {
    let d = std::env::temp_dir().join(format!("xt-trust-{}-lists", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    assert!(matches!(mirror_list(&d.join("none")), Err(Fail::Absent(_))));
    std::fs::write(d.join("off"), "# Server = https://a.example/$repo\nServer = file:///srv/mirror\n").unwrap();
    assert!(matches!(mirror_list(&d.join("off")), Err(Fail::Bad(_))));
    std::fs::write(d.join("on"), "# ranked today\nServer = https://a.example/$repo\n").unwrap();
    assert!(mirror_list(&d.join("on")).unwrap().ends_with("\nServer = https://a.example/$repo\n"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_good_host_gives_six_files_and_the_next_build_judges_by_them() {
    let d = fixture("good");
    let got = run(&d).unwrap();
    let names: Vec<&str> = got.iter().map(|c| c.file).collect();
    assert_eq!(names, ["archlinux.gpg", "archlinux-trusted", "archlinux-revoked", "cachyos.gpg", "cachyos-trusted", "cachyos-revoked"]);
    assert!(got.iter().all(|c| c.data == format!("bytes of {}", c.file).into_bytes()));
    assert_eq!((got[0].package, got[0].version.as_str(), got[5].package, got[5].version.as_str()), ("archlinux-keyring", ARCH, "cachyos-keyring", CACHY));
    // First bootstrap: `pgp verify PKG PKG.sig --keyring <host folder>`, once per package.
    let args = std::fs::read_to_string(d.join("args")).unwrap();
    let pkg = arch_pkg(&d);
    assert_eq!(args.lines().next().unwrap(), format!("pgp verify {} {}.sig --keyring {}", pkg.display(), pkg.display(), d.join("host").display()));
    assert_eq!(args.lines().count(), 2);
    remember(&d.join("last"), &got).unwrap();
    assert_eq!(std::fs::read_dir(d.join("last")).unwrap().count(), 6);
    assert_eq!(std::fs::read(d.join("last/cachyos-trusted")).unwrap(), b"bytes of cachyos-trusted");
    assert_eq!(std::fs::read_dir(d.join("last").parent().unwrap()).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".tmp")).count(), 0);
    assert_eq!(run(&d).unwrap(), got);
    let args = std::fs::read_to_string(d.join("args")).unwrap();
    assert!(args.lines().nth(2).unwrap().ends_with(&format!("--keyring {}", d.join("last").display())), "{args}");
    // A refusal against the last image's keyring names it.
    script(&d.join("verify"), "echo 'abgelehnt x: Schlüssel unbekannt'; exit 1");
    assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("last image's keyring") && m.contains("Schlüssel unbekannt")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_host_without_the_parts_is_absent() {
    let d = fixture("absent-version");
    std::fs::remove_file(d.join("versions/cachyos-keyring")).unwrap();
    assert_eq!(run(&d), Err(Fail::Absent("pacman -Q names no package cachyos-keyring".into())));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("absent-pkg");
    std::fs::remove_file(arch_pkg(&d)).unwrap();
    assert!(matches!(run(&d), Err(Fail::Absent(m)) if m.ends_with("any.pkg.tar.zst is missing")));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("absent-sig");
    std::fs::remove_file(format!("{}.sig", arch_pkg(&d).display())).unwrap();
    assert!(matches!(run(&d), Err(Fail::Absent(m)) if m.ends_with(".sig is missing")));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("absent-file");
    std::fs::remove_file(d.join("host/cachyos-revoked")).unwrap();
    assert!(matches!(run(&d), Err(Fail::Absent(m)) if m.contains("cachyos-revoked")));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("absent-bsdtar");
    assert!(matches!(run_with(&d, "xt-no-such-bsdtar"), Err(Fail::Absent(m)) if m.contains("xt-no-such-bsdtar")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_failed_check_is_bad() {
    // (b): only exit 0 together with `gut` counts.
    for (case, body) in [("refused", "echo 'abgelehnt x: Signatur passt nicht'; exit 1"), ("gut-exit1", "echo 'gut ABCD T'; exit 1"), ("exit0-not-gut", "echo 'abgelehnt x'"), ("silent", "exit 0")] {
        let d = fixture(case);
        script(&d.join("verify"), body);
        assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("does not say gut")), "{case}");
        let _ = std::fs::remove_dir_all(&d);
    }
    let d = fixture("no-verifier");
    std::fs::remove_file(d.join("verify")).unwrap();
    assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("cannot start")));
    let _ = std::fs::remove_dir_all(&d);

    // (c): one changed byte in the host's folder, or a file the package does not carry.
    let d = fixture("differs");
    std::fs::write(d.join("host/archlinux-trusted"), "bytes of archlinux-trusteD").unwrap();
    assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("archlinux-trusted differs from usr/share/pacman/keyrings/archlinux-trusted")));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("not-in-pkg");
    let tree = d.join("tree/cachyos-keyring");
    std::fs::remove_file(tree.join(KEYRING_IN_PKG).join("cachyos-revoked")).unwrap();
    let pkg = d.join("cache").join(pkg_file("cachyos-keyring", CACHY).unwrap());
    assert!(Command::new("bsdtar").arg("-cf").arg(&pkg).arg("-C").arg(&tree).arg("usr").status().unwrap().success());
    assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("no usr/share/pacman/keyrings/cachyos-revoked")));
    let _ = std::fs::remove_dir_all(&d);

    let d = fixture("bad-version");
    std::fs::write(d.join("versions/archlinux-keyring"), "1/../x").unwrap();
    assert!(matches!(run(&d), Err(Fail::Bad(m)) if m.contains("not a package version")));
    let _ = std::fs::remove_dir_all(&d);
}
