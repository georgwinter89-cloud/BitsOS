//! GT: the GPU capture's register-trace checks (`host/gaming-pc/gx-trace-check.sh`, used by the Gaming-PC capture
//! `host/gaming-pc/gpu-messung.sh` and the Mini-PC kit) against a fake tracefs. The checks are the shell script
//! `host/tests/gx/30-trace-kit.sh`; this host test runs it, so it belongs to every gate (no root, no GPU, seconds).

use std::path::PathBuf;
use std::process::{Command, Stdio};

#[test]
fn the_capture_never_hands_back_an_empty_trace_silently() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let script = root.join("host/tests/gx/30-trace-kit.sh");
    assert!(script.is_file(), "{} missing", script.display());
    let run = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gx-trace-kit");
    let _ = std::fs::remove_dir_all(&run);
    std::fs::create_dir_all(&run).expect("the run folder");
    let out = Command::new("bash")
        .arg(&script)
        .env("BITSOS_ROOT", &root)
        .env("BITSOS_RUN_DIR", &run)
        .stdin(Stdio::null())
        .output()
        .expect("bash missing (install bash)");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success() && text.contains("PASS: trace kit checks"), "{text}");
}
