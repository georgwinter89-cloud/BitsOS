//! FS-17 ktest: `fs.unixwake.program` runs `/bin/abi-unixwake` (user/abi-tests/c/unixwake.c): a compositor
//! process with its client sockets in four inner epolls inside one outer epoll (edge, level, one-shot, and level
//! inside edge, as wayland-backend inside calloop) plus a second compositor thread with one client in the
//! tightest shape, client processes answering pings over AF_UNIX stream pairs, descriptors passed now and then,
//! and busy processes on every CPU. Every ping must come back; a pong waiting longer than 5 s is a lost wake-up
//! (the program says on which side the data sat). The gate runs it a few seconds; CONFIG `fs17_secs=<n>` makes
//! it run n seconds (the desk's long run on the Mini-PC), `fs17_clients` (8) and `fs17_busy` (3) set the process
//! counts. Timing is not judged here: pongs later than 1 s are only counted (klog).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;
const PROG: &str = "/bin/abi-unixwake";

fn knob(key: &str, default: u64, max: u64) -> u64 {
    net::config(key).and_then(|v| v.trim().parse::<u64>().ok()).map_or(default, |v| v.clamp(1, max))
}

fn program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::fail("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    let secs = knob("fs17_secs", 4, 24 * 3600);
    let clients = knob("fs17_clients", 8, 256);
    let busy = knob("fs17_busy", 3, 256);
    let (a, b, c) = (format!("{secs}"), format!("{clients}"), format!("{busy}"));
    let argv = ["abi-unixwake", a.as_str(), b.as_str(), c.as_str()];
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
    // The run, its quiet end (up to 10 s for pongs still out) and the clients' ends.
    let limit = (secs + 40) * SEC;
    let ended = p.wait(Some(limit)).is_some();
    let status = p.status();
    if !ended || status.is_none() {
        p.kill(9);
        p.wait(Some(5 * SEC));
    }
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    let counts = text.lines().find(|l| l.starts_with("counts: ")).unwrap_or("counts: none");
    klog::event!(Info, "fs", "unixwake.done", secs = secs, clients = clients, busy = busy, counts = counts;
        "compositor-like ping/pong over AF_UNIX stream sockets in nested epolls");
    kassert!(ended && status.is_some(), "{PROG}: still running after {} s: {text}", limit / SEC);
    for l in text.lines().filter(|l| l.starts_with("stall: ")) {
        klog::event!(Warn, "fs", "unixwake.stall", line = l; "a pong waited longer than 5 s (a lost wake-up)");
    }
    kassert!(
        status == Some(proc::ExitStatus::Exited(0)) && !text.contains(": no"),
        "{PROG} ended {status:?}: {text}"
    );
    Ok(())
}
ktest!("fs.unixwake.program", Safe, program);
