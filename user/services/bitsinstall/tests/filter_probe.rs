//! The filter on the host's real Linux kernel (the same program V2 gets): open and a thread pass, execve and a
//! fork end the process with SIGSYS; the text console's calls (MC-11) all pass.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

#[test]
fn exec_and_fork_end_the_probe() {
    for what in ["exec", "fork"] {
        let out = Command::new(env!("CARGO_BIN_EXE_bitsinstall")).args(["filter-probe", what]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("filter: on") && text.contains("open: ok") && text.contains("thread: ok"), "{what}: {text}");
        assert_eq!(out.status.signal(), Some(31), "{what}: {:?} {text}", out.status);
    }
}

#[test]
fn the_text_consoles_calls_pass_the_filter() {
    let out = Command::new(env!("CARGO_BIN_EXE_bitsinstall")).args(["filter-probe", "console"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("filter: on") && text.contains("console: ok"), "{text}");
    assert_eq!(out.status.code(), Some(0), "{:?} {text}", out.status);
}
