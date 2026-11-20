//! PK-0: bitspkg and init's /run/bitsos/apps-state. While init has not settled /apps (`waiting: ...`) every
//! changing command is refused before it touches the store; `space` names the volume /apps lives on.

mod common;
use common::*;
use std::fs;

#[test]
fn waits_for_init_and_names_the_volume() {
    let dir = tmp("apps-state");
    let root = dir.join("root");
    let pkg = package(&dir, "1.0");
    let state = root.join("run/bitsos/apps-state");
    write(&state, "waiting: data volume being created\n", 0o644);

    let (code, _, err) = run(&root, &["install", pkg.to_str().unwrap()], None);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("/apps ist noch nicht bereit") && err.contains("waiting: data volume being created"), "{err}");
    assert!(err.contains("event=store.not_ready"), "{err}");
    assert!(!root.join("apps").exists(), "nothing written into the tmpfs the bind will cover");
    let (code, _, err) = run(&root, &["register"], None);
    assert!(code == 1 && err.contains("noch nicht bereit"), "{err}");
    // Reading is fine while waiting.
    let (code, out, _) = run(&root, &["list"], None);
    assert!(code == 0 && out.is_empty(), "{out}");

    write(&state, "data: stick data.img 64 MB\n", 0o644);
    let (code, out, err) = run(&root, &["install", pkg.to_str().unwrap()], None);
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, _) = run(&root, &["space"], None);
    assert!(code == 0 && out.contains("\nvolume data: stick data.img 64 MB\nstore: "), "{out}");

    fs::remove_file(&state).unwrap();
    let (_, out, _) = run(&root, &["space"], None);
    assert!(out.contains("volume unknown (no ") && out.contains("run/bitsos/apps-state)"), "{out}");
    let (code, out, _) = run(&root, &["list"], None);
    assert!(code == 0 && out.starts_with("demo current 1.0 "), "{out}");
}
