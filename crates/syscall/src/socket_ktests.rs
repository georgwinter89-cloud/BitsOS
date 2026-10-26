//! NW-5 ktests that need processes: the socket programs of `user/abi-tests` (built into every image with the
//! musl sysroot) run through the real syscall path and must end and print exactly what Linux did
//! (`host/tests/nw/sock-linux.txt`, made on CachyOS); the Wayland-like pair of processes passes a shared buffer
//! over a path socket; with CONFIG `net_test_url` a program fetches from the host over the card. Safe and
//! rerunnable: the programs name their files with their pid and remove them.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;
const LINUX: &str = include_str!("../../../host/tests/nw/sock-linux.txt");

/// The blocks of sock-linux.txt: (argv, end line, stdout lines).
fn blocks() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    let mut v: Vec<(&str, &str, Vec<&str>)> = Vec::new();
    for l in LINUX.lines().filter(|l| !l.starts_with('#')) {
        if let Some(m) = l.strip_prefix("=== ") {
            v.push((m, "", Vec::new()));
        } else if let Some(b) = v.last_mut() {
            if b.1.is_empty() {
                b.1 = l;
            } else {
                b.2.push(l);
            }
        }
    }
    v
}

fn end_of(p: &proc::Process) -> Option<String> {
    match p.status() {
        Some(proc::ExitStatus::Exited(c)) => Some(format!("end: exit {c}")),
        Some(proc::ExitStatus::Signaled(s)) => Some(format!("end: signal {s}")),
        None => None,
    }
}

fn start(argv: &[&str]) -> Result<(Arc<proc::Process>, Arc<proc::stdio::Capture>), String> {
    let out = proc::stdio::Capture::new(1 << 18);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let mut o = proc::SpawnOptions::new(argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let path = format!("/bin/{}", argv[0]);
    let p = proc::spawn_with(&path, &o).map_err(|e| format!("spawn {path}: {e}"))?;
    Ok((p, out))
}

/// Runs a program to its end (60 s at most): (end line, output).
fn run(argv: &[&str]) -> Result<(String, String), String> {
    let (p, out) = start(argv)?;
    p.wait(Some(60 * SEC));
    let Some(end) = end_of(&p) else {
        p.kill(9);
        return Err(format!("{argv:?}: still running after 60 s"));
    };
    Ok((end, String::from_utf8_lossy(&out.contents()).into_owned()))
}

fn have(prog: &str) -> bool {
    vfs::root_ns().lookup(&format!("/bin/{prog}")).is_ok()
}

/// Every block of `prog` must end and print as on Linux.
fn same_as_linux(prog: &str) -> TestResult {
    if !have(prog) {
        return ktest::skip("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    let sockets = net::sock::alive();
    let mut n = 0;
    for (args, end, lines) in blocks().into_iter().filter(|b| b.0.split(' ').next() == Some(prog)) {
        let argv: Vec<&str> = args.split(' ').collect();
        let (got_end, text) = run(&argv)?;
        let got: Vec<&str> = text.lines().collect();
        if got_end != end || got != lines {
            let first = got.iter().zip(lines.iter()).position(|(a, b)| a != b).unwrap_or(got.len().min(lines.len()));
            klog::event!(Warn, "net", "sock.abi_diff", case = args, line = first, v2 = got.get(first).copied().unwrap_or("<none>"),
                linux = lines.get(first).copied().unwrap_or("<none>"));
            return Err(format!("{args}: {got_end} vs Linux {end}; first different line {first}: {:?} vs {:?}", got.get(first), lines.get(first)));
        }
        n += 1;
    }
    kassert!(n > 0, "no {prog} blocks in sock-linux.txt");
    // Every socket the programs made went with them (the collector took the cycles abi-unix leaves behind).
    for _ in 0..50 {
        if net::sock::alive() <= sockets {
            break;
        }
        net::sock::unix_gc::collect();
        sched::sleep_ns(SEC / 50);
    }
    kassert!(net::sock::alive() <= sockets, "sockets left behind: {} before, {} after ({})", sockets, net::sock::alive(), net::sock::summary());
    Ok(())
}

fn abi_unix() -> TestResult {
    same_as_linux("abi-unix")
}
ktest!("nw.sock.abi_unix", Safe, abi_unix);

fn abi_inet() -> TestResult {
    same_as_linux("abi-inet")
}
ktest!("nw.sock.abi_inet", Safe, abi_inet);

/// NW-6: rtnetlink dumps, getifaddrs, SO_BINDTODEVICE and the interface ioctls, as on Linux.
fn abi_netlink() -> TestResult {
    same_as_linux("abi-netlink")
}
ktest!("nw.sock.abi_netlink", Safe, abi_netlink);

fn abi_fdpass() -> TestResult {
    same_as_linux("abi-fdpass")
}
ktest!("nw.sock.abi_fdpass", Safe, abi_fdpass);

/// The Wayland pattern between two processes: a compositor listens on a path socket, a client connects, makes
/// a shared buffer, passes its fd (SCM_RIGHTS) and draws into it; the compositor maps what arrived and sees both
/// frames, and SO_PEERCRED names the other process.
fn wayland_pair() -> TestResult {
    if !have("abi-fdpass") {
        return ktest::skip("abi-fdpass is not in this image (no musl sysroot when it was built)");
    }
    let path = format!("/tmp/nw-wayland-{}", ktest_seq());
    let (srv, sout) = start(&["abi-fdpass", "server", &path])?;
    // The client waits for the path itself (up to 5 s), so no ordering is needed here.
    let (cli, cout) = start(&["abi-fdpass", "client", &path])?;
    cli.wait(Some(60 * SEC));
    srv.wait(Some(60 * SEC));
    let (se, ce) = (end_of(&srv), end_of(&cli));
    if se.is_none() {
        srv.kill(9);
    }
    if ce.is_none() {
        cli.kill(9);
    }
    let s = String::from_utf8_lossy(&sout.contents()).into_owned();
    let c = String::from_utf8_lossy(&cout.contents()).into_owned();
    kassert!(se.as_deref() == Some("end: exit 0") && ce.as_deref() == Some("end: exit 0"), "server {se:?}: {s:?}; client {ce:?}: {c:?}");
    for want in [
        "server: client accepted: yes, its pid differs from mine: yes, same uid: yes",
        "compositor: request 'create_pool', fd arrived: yes",
        "compositor: pool size 16384, cloexec yes",
        "compositor: first frame right: yes",
        "compositor: second frame (written after the hand-over) right: yes",
    ] {
        kassert!(s.lines().any(|l| l == want), "server output lacks {want:?}: {s:?}");
    }
    kassert!(c.lines().any(|l| l == "client: compositor says 'frame2'"), "client output: {c:?}");
    Ok(())
}
ktest!("nw.sock.wayland_pair", Safe, wayland_pair);

fn ktest_seq() -> u64 {
    static N: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, core::sync::atomic::Ordering::Relaxed) ^ (time::now_ns() & 0xffff_0000)
}

/// With CONFIG `net_test_url` (host/tests/nw/stack_server.py on the host, 30-stack.sh and 60-sockets.sh set
/// it): a program fetches the test file by length and chunked over the card and checks the SHA-256, echoes a
/// datagram (port + 1) and streams 3 x 4 MiB through the TCP echo (port + 2) at once; its connections are
/// recorded with the program as owner (B3).
fn fetch_over_the_card() -> TestResult {
    if !have("abi-net") {
        return ktest::skip("abi-net is not in this image (no musl sysroot when it was built)");
    }
    let Some(base) = net::config("net_test_url") else {
        return ktest::skip("no net_test_url in CONFIG (a host server: host/tests/nw/60-sockets.sh)");
    };
    let Some(stack) = net::stack::kernel() else { return ktest::skip("no network stack") };
    if stack.wait_configured(20 * SEC).is_none() {
        return ktest::skip("no address on any card within 20 s");
    }
    let hostport = base.trim_start_matches("http://").trim_end_matches('/');
    let Some((host, port)) = hostport.split_once(':') else { return Err(format!("net_test_url {base}: want http://IP:PORT")) };
    let port: u16 = port.parse().map_err(|_| format!("net_test_url {base}: bad port"))?;
    let (end, out) = run(&["abi-net", "fetchcheck", &base])?;
    kassert!(end == "end: exit 0" && out.matches("sha256 matches").count() == 2, "fetchcheck: {end} {out:?}");
    let (end, out) = run(&["abi-net", "udpecho", host, &format!("{}", port + 1)])?;
    kassert!(end == "end: exit 0" && out.contains("echo came back"), "udpecho: {end} {out:?}");
    let (end, out) = run(&["abi-net", "echo", host, &format!("{}", port + 2), "3", "4"])?;
    kassert!(end == "end: exit 0" && out.matches(" intact").count() == 3, "echo: {end} {out:?}");
    let recs = policy::recent_connections(64);
    kassert!(
        recs.iter().any(|r| r.subject.pid != 0 && r.proto == policy::NetProto::Tcp && r.remote_port == port),
        "no TCP connection record owned by the program towards port {port}"
    );
    Ok(())
}
ktest!("nw.sock.fetch", Safe, fetch_over_the_card);

/// With CONFIG `nw_k3_peer` (host/tests/nw/k3_lan.py on a second card, 60-sockets.sh): V1's k3a/k3b fakes for
/// program sockets: a refused and a silent connect, a peer that ignores the first segment (the resend comes from
/// the stack while the program only waits), a host whose ARP answer is late, and a lossy web server (lost,
/// duplicated and reordered segments) whose files must arrive intact.
fn k3_lan() -> TestResult {
    if !have("abi-net") {
        return ktest::skip("abi-net is not in this image (no musl sysroot when it was built)");
    }
    let Some(peer) = net::config("nw_k3_peer") else {
        return ktest::skip("no nw_k3_peer in CONFIG (the fake LAN of host/tests/nw/60-sockets.sh)");
    };
    let Some(stack) = net::stack::kernel() else { return ktest::skip("no network stack") };
    let on_lan = || stack.interfaces().iter().any(|i| i.info.addr.is_some_and(|(a, _)| a.0[..3] == [192, 168, 77]));
    let t0 = time::now_ns();
    while !on_lan() && time::now_ns() - t0 < 60 * SEC {
        sched::sleep_ns(SEC / 10);
    }
    kassert!(on_lan(), "no address on the fake LAN within 60 s");
    let (_, out) = run(&["abi-net", "connect", &peer, "7003", "3000"])?;
    kassert!(out.contains("ECONNREFUSED"), "reset peer: {out:?}");
    let (_, out) = run(&["abi-net", "connect", &peer, "7002", "1500"])?;
    kassert!(out.contains("still connecting"), "silent peer: {out:?}");
    let (end, out) = run(&["abi-net", "send", &peer, "7001", "3000"])?;
    kassert!(end == "end: exit 0" && out.contains("sent 3000, peer answered 9 bytes, end EOF"), "hold peer: {end} {out:?}");
    let (_, out) = run(&["abi-net", "connect", "192.168.77.50", "7001", "3000"])?;
    kassert!(out.contains(": connected"), "late ARP host: {out:?}");
    let (end, out) = run(&["abi-net", "fetchcheck", &format!("http://{peer}:7080")])?;
    kassert!(end == "end: exit 0" && out.matches("sha256 matches").count() == 2, "lossy web server: {end} {out:?}");
    Ok(())
}
ktest!("nw.sock.k3_lan", Safe, k3_lan);

/// NW-7 fixer: with CONFIG `net_test_url` (stack_server.py with `/part/N/`): 64 programs run `abi-net fetchcheck`
/// at once, each fetching 1 MiB by length and chunked through the syscall socket path and checking its SHA-256,
/// while a kernel probe opens a fresh connection for `/ping` every 50 ms (the way MCP is asked). All program
/// waiters share `inet::READY`; this is the program side of NW-7's 64 downloads (`nw.stack.fetch_parallel`).
fn fetch_parallel_programs() -> TestResult {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    const PROGRAMS: usize = 64;
    const PART: usize = 1 << 20;
    if !have("abi-net") {
        return ktest::skip("abi-net is not in this image (no musl sysroot when it was built)");
    }
    let Some(base) = net::config("net_test_url") else {
        return ktest::skip("no net_test_url in CONFIG (a host server: host/tests/nw/60-sockets.sh)");
    };
    let Some(stack) = net::stack::kernel() else { return ktest::skip("no network stack") };
    if stack.wait_configured(20 * SEC).is_none() {
        return ktest::skip("no address on any card within 20 s");
    }
    let opts = net::http::Options { timeout_ns: 10 * SEC, idle_ns: 10 * SEC, ..net::http::Options::new(200) };
    match net::http::get(stack, &format!("{base}/part/{PART}/file.sha256"), &opts) {
        Ok(_) => {}
        Err(net::http::HttpError::Status(404)) => return ktest::skip("the test server has no /part/N/ (stack_server.py before NW-7's fixer)"),
        Err(x) => return Err(format!("GET /part/{PART}/file.sha256: {x:?}")),
    }
    let no_memory = net::stack::NO_MEMORY.get();
    let stop = Arc::new(AtomicBool::new(false));
    let (answered, failed, worst_us) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (a, f, w, halt, ping) = (answered.clone(), failed.clone(), worst_us.clone(), stop.clone(), format!("{base}/ping"));
    let prober = sched::spawn("ktest-probe", sched::Class::Normal, move || {
        while !halt.load(Ordering::Acquire) {
            let t = time::now_ns();
            let o = net::http::Options { timeout_ns: 10 * SEC, idle_ns: 10 * SEC, ..net::http::Options::new(100) };
            if matches!(net::http::get(stack, &ping, &o), Ok((_, ref b)) if b == b"pong\n") {
                a.fetch_add(1, Ordering::Relaxed);
                w.fetch_max((time::now_ns() - t) / 1000, Ordering::Relaxed);
            } else {
                f.fetch_add(1, Ordering::Relaxed);
            }
            sched::sleep_ns(SEC / 20);
        }
    })
    .map_err(|x| format!("probe thread: {x:?}"))?;
    let part = format!("{base}/part/{PART}");
    let t0 = time::now_ns();
    let mut progs = Vec::new();
    let mut spawn_err = None;
    for i in 0..PROGRAMS {
        match start(&["abi-net", "fetchcheck", &part]) {
            Ok(p) => progs.push(p),
            Err(x) => {
                spawn_err = Some(format!("program {i}: {x}"));
                break;
            }
        }
    }
    // B4: bounded (64 x 2 MiB; generous for a slow card).
    let end = t0 + 240 * SEC;
    let mut bad = Vec::new();
    for (i, (p, out)) in progs.iter().enumerate() {
        p.wait(Some(end.saturating_sub(time::now_ns()).max(1)));
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        match end_of(p) {
            Some(e) if e == "end: exit 0" && text.matches("sha256 matches").count() == 2 => {}
            e => {
                if e.is_none() {
                    p.kill(9);
                }
                bad.push(format!("program {i}: {e:?} {text:?}"));
            }
        }
    }
    let ms = (time::now_ns() - t0) / 1_000_000;
    stop.store(true, Ordering::Release);
    let probe_done = prober.join_timeout(20 * SEC);
    sched::reap();
    let worst_ms = worst_us.load(Ordering::Relaxed) / 1000;
    klog::event!(Info, "net", "ktest.fetch_parallel_programs", programs = progs.len(), ms = ms,
        mb_per_s = (progs.len() * 2 * PART) as u64 / 1000 / ms.max(1), probes = answered.load(Ordering::Relaxed),
        probe_failed = failed.load(Ordering::Relaxed), probe_worst_ms = worst_ms);
    if let Some(x) = spawn_err {
        return Err(x);
    }
    kassert!(bad.is_empty(), "{} of {PROGRAMS} programs failed, first: {}", bad.len(), bad[0]);
    kassert!(probe_done, "the probe still running 20 s after the programs");
    kassert!(answered.load(Ordering::Relaxed) >= 1 && failed.load(Ordering::Relaxed) == 0, "the probe was not answered every time ({} ok, {} failed)", answered.load(Ordering::Relaxed), failed.load(Ordering::Relaxed));
    kassert!(net::stack::NO_MEMORY.get() == no_memory, "real socket buffer refusals during the downloads: {}", net::stack::NO_MEMORY.get() - no_memory);
    Ok(())
}
ktest!("nw.sock.fetch_parallel", Safe, fetch_parallel_programs);
