//! Linux is the oracle: runs abi-a/b/c on this machine (CachyOS) the way the kernel's `ul.abi.*` ktests run
//! them on BitsOS (stdin an empty pipe, stdout a pipe, cwd "/", env HOME=/ PATH=/bin) and compares with
//! `expected/wave-*.txt`. `UL_ABI_RECORD=1` (or a missing file) writes the expected file from this run instead
//! (do that only on CachyOS, and look at the diff before committing).

use std::io::Write;
use std::process::{Command, Stdio};

fn run(bin: &str) -> (String, Option<i32>, String) {
    let mut c = Command::new(bin)
        .current_dir("/")
        .env_clear()
        .env("HOME", "/")
        .env("PATH", "/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(c.stdin.take());
    let out = c.wait_with_output().expect("wait");
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
}

fn check(wave: char, bin: &str) {
    let dir = env!("CARGO_MANIFEST_DIR");
    let exp_path = format!("{dir}/expected/wave-{}.txt", wave.to_ascii_lowercase());
    let (out, code, err) = run(bin);
    assert_eq!(code, Some(0), "abi-{wave} exit {code:?}, stderr: {err}\n{out}");
    assert!(err.is_empty(), "abi-{wave} wrote to stderr: {err}");
    if std::env::var("UL_ABI_RECORD").is_ok_and(|v| v == "1") || !std::path::Path::new(&exp_path).exists() {
        std::fs::File::create(&exp_path).and_then(|mut f| f.write_all(out.as_bytes())).expect("record");
        eprintln!("recorded {exp_path}");
        return;
    }
    let expected = std::fs::read_to_string(&exp_path).unwrap_or_else(|e| panic!("{exp_path}: {e} (record with UL_ABI_RECORD=1)"));
    let d = abitest::compare(&expected, &out, "");
    assert!(d.ok() && d.known.is_empty(), "abi-{wave} differs from {exp_path}: {}\n--- got ---\n{out}", d.summary());
    // Deterministic: a second run gives the same transcript.
    let (again, _, _) = run(bin);
    assert_eq!(again, out, "abi-{wave}: two runs differ");
}

#[test]
fn wave_a_equals_linux() {
    check('A', env!("CARGO_BIN_EXE_abi-a"));
}

#[test]
fn wave_b_equals_linux() {
    check('B', env!("CARGO_BIN_EXE_abi-b"));
}

#[test]
fn wave_c_equals_linux() {
    check('C', env!("CARGO_BIN_EXE_abi-c"));
}
