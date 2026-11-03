//! The service's seccomp filter on the real kernel of the host (Linux): `bitsmcp filter-probe` installs it as
//! the service does and must see open and socket refused with EPERM, a thread allowed, and execve and fork end
//! the process with SIGSYS. The QEMU scenario host/tests/mcp/60-service.sh runs the same probe on V2.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

const SIGSYS: i32 = 31;

fn probe(last: &str) -> (String, Option<i32>) {
    let out = Command::new(env!("CARGO_BIN_EXE_bitsmcp")).args(["filter-probe", last]).output().unwrap();
    (String::from_utf8_lossy(&out.stdout).into_owned(), out.status.signal())
}

#[test]
fn the_filter_refuses_and_kills_on_linux() {
    for last in ["exec", "fork"] {
        let (out, sig) = probe(last);
        if out.contains("filter: seccomp") || out.contains("filter: prctl") {
            eprintln!("skip: no seccomp on this host: {out}");
            return;
        }
        for want in ["filter: on", "open: errno 1", "socket: errno 1", "thread: ok"] {
            assert!(out.lines().any(|l| l == want), "{last}: no '{want}' in:\n{out}");
        }
        assert!(!out.contains("returned"), "{last} came back:\n{out}");
        assert_eq!(sig, Some(SIGSYS), "{last}: not ended by SIGSYS:\n{out}");
    }
}
