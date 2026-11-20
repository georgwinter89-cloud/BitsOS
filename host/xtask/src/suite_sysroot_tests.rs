//! L7-RESTE-AB 4: the gate's step order in a fresh worktree: the sysroot comes before the first step whose image needs it.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static BUILDS: AtomicUsize = AtomicUsize::new(0);

fn fake_build(_: &Path) -> Result<(), String> {
    BUILDS.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[test]
fn sysroot_runs_once_before_the_first_v2_script() {
    let root = std::env::temp_dir().join(format!("xtask-gate-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("host/tests/wy")).unwrap();
    std::fs::write(root.join("host/tests/wy/1-host.sh"), "cargo test\n").unwrap();
    std::fs::write(root.join("host/tests/wy/2-v2.sh"), "cargo xtask image --name x\n").unwrap();
    std::fs::write(root.join("host/tests/wy/3-v2.sh"), "cargo xtask qemu --name x\n").unwrap();
    let mut gate = Gate { root: &root, res: Vec::new(), interim: None, sysroot_done: false, build_sysroot: fake_build };
    // A fresh tree: nothing built yet; `true` stands in for the scripts.
    for (name, script) in [("host", "host/tests/wy/1-host.sh"), ("v2 a", "host/tests/wy/2-v2.sh"), ("v2 b", "host/tests/wy/3-v2.sh")] {
        gate.script(name, &mut Command::new("true"), script);
    }
    gate.ensure_sysroot();
    let order: Vec<&str> = gate.res.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(order, ["host", "sysroot", "v2 a", "v2 b"]);
    assert_eq!(BUILDS.load(Ordering::SeqCst), 1, "built once, the ktest's ensure finds it done");
    let _ = std::fs::remove_dir_all(&root);
}

/// Every script the gate runs goes through `Gate::script` (a plain `gate.step` with `run_status` would skip the
/// sysroot check), and the ktest boots ask for it through `ensure_sysroot`.
#[test]
fn the_gate_runs_its_scripts_through_the_sysroot_check() {
    let src = include_str!("suite.rs");
    let body = &src[src.find("fn run_test_scripts").unwrap()..src.find("/// The script of a run name").unwrap()];
    assert!(!body.contains("run_status(&mut w"), "a script step bypasses Gate::script");
    assert!(body.contains("gate.ensure_sysroot();"));
    assert!(!body.contains("gate.step(\"sysroot\""));
    let scripts = body.matches("gate.script(").count();
    assert!(scripts >= 4, "{scripts} script steps");
}
