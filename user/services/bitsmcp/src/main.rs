//! The `bitsmcp` command (MC-8a): `bitsmcp serve` is what init starts under CONFIG.TXT mcp=both|service.
//!
//! Setup, in this order, while the process has one thread: /dev/kmsg, the gate's sessions (root-only node;
//! `hello` names the mode, the port and the connection count), the listening socket, the TLS keys on the stick
//! (MC-8b, `bitsmcp::keys`: made at the first start), a private mount view if the kernel has one, then root is
//! given up for the service's own user, no_new_privs and the seccomp filter (`bitsmcp::filter`). From there it
//! only serves.

mod sys;

use bitsmcp::link::{DevLink, Link, Pool};
use bitsmcp::{filter, keys, log, serve, tls};
use mcp_wire::gate::{self, Mode};
use std::process::exit;

const USAGE: &str = "usage: bitsmcp serve [--user UID[:GID]] [--node PATH] [--keys DIR]
  serve      the MCP service (init starts it under CONFIG.TXT mcp=both or mcp=service)
  --user     the user it runs as after the setup (default 961:961; 0 keeps root, for a test only)
  --node     the gate (default /dev/bitsos/mcp)
  --keys     the TLS keys: the server's, made at the first start, and the paired clients' (default
             /boot/BITSOS2/mcp, the stick's \\BITSOS2\\mcp\\)
  mode       prints what /proc/bitsos/mcp says
  filter-probe exec|fork
             the service's setup (root given up when it has it, no_new_privs, the filter), then calls the filter
             refuses (open, socket: EPERM), a thread (allowed) and execve or fork, which ends it with SIGSYS";

fn parse_user(s: &str) -> Option<(u32, u32)> {
    match s.split_once(':') {
        Some((u, g)) => Some((u.parse().ok()?, g.parse().ok()?)),
        None => s.parse().ok().map(|u| (u, u)),
    }
}

fn fail(msg: &str) -> ! {
    log::warn(msg);
    exit(1)
}

fn serve(args: &[String]) -> ! {
    let mut user = bitsmcp::SERVICE_ID;
    let mut node = String::from(gate::NODE);
    let mut key_dir = String::from(keys::DIR);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match (a.as_str(), it.next()) {
            ("--user", Some(v)) => user = parse_user(v).unwrap_or_else(|| fail(&format!("--user {v}: want UID or UID:GID"))),
            ("--node", Some(v)) => node = v.clone(),
            ("--keys", Some(v)) => key_dir = v.clone(),
            _ => {
                eprintln!("{USAGE}");
                exit(2)
            }
        }
    }
    log::open();
    let mut first = DevLink::open(&node).unwrap_or_else(|e| fail(&format!("{node}: {e} (ENXIO: CONFIG.TXT says mcp=kernel, the kernel serves MCP itself)")));
    let hello = first.command(&gate::encode_hello()).ok().and_then(|t| gate::decode_hello_answer(&t)).unwrap_or_else(|| fail(&format!("{node}: no hello answer")));
    if hello.mode == Mode::Kernel {
        log::info("mcp=kernel: the kernel serves MCP; nothing to do");
        exit(0);
    }
    // One session per connection served at once, one spare. All are opened now (the filter forbids open later);
    // when the process runs out of descriptors or memory (a huge mcp_conns), it serves with what it has.
    let mut links: Vec<Box<dyn Link>> = vec![Box::new(first)];
    for i in 0..hello.conns {
        match DevLink::open(&node) {
            Ok(l) => links.push(Box::new(l)),
            Err(e) => {
                log::warn(&format!("{node}: session {} of {}: {e}; serving with {}", i + 2, hello.conns + 1, links.len()));
                break;
            }
        }
    }
    let conns = hello.conns.min(links.len());
    let listener = std::net::TcpListener::bind(("0.0.0.0", hello.port)).unwrap_or_else(|e| fail(&format!("listen on port {}: {e}", hello.port)));
    // The keys before the filter (it forbids open). No TLS this start: plain HTTP with the token (as MC-8a) only
    // when no client can be paired on the stick, the state before the pairing anyway; when one may be (the stick
    // not mounted in time, a paired file unreadable, the server key unusable), no plain fallback: once paired,
    // the service's plain path stays off. The service ends then and init starts it again with its back-off; the
    // kernel's own server (mcp=both/kernel) and the console mirror are untouched.
    let kd = std::path::Path::new(&key_dir);
    let opened = if key_dir == keys::DIR { keys::open_on_stick(kd, std::path::Path::new(keys::MOUNT), keys::STICK_WAIT) } else { keys::open(kd) };
    let (tls, tls_line) = match opened.and_then(|k| {
        let made = k.made;
        let unpaired = k.paired.is_empty();
        tls::Tls::new(k).map(|t| (t, made)).map_err(|text| keys::OpenError { text, unpaired })
    }) {
        Ok((t, made)) => {
            let state = match (t.on(), hello.mode) {
                (true, _) => format!("{} paired client(s), TLS only", t.paired()),
                (false, Mode::Service) => String::from("no client paired yet: plain HTTP only for the pairing, which Georg confirms at the keyboard (host/mcp/pair.mjs)"),
                (false, _) => String::from("no client paired yet: plain HTTP with the token until the one-time pairing (host/mcp/pair.mjs)"),
            };
            let line = format!("TLS server key sha256 {}{}, {state}", t.fingerprint(), if made { " (made now)" } else { "" });
            (Some(std::sync::Arc::new(t)), line)
        }
        // MC-10: under mcp=service there is no plain fallback at all (a token alone opens nothing).
        Err(e) if hello.mode == Mode::Service => fail(&format!(
            "no TLS this start ({key_dir}: {}); under mcp=service the service serves TLS only: ending (init starts it again; mcp=kernel on the stick is the rescue)",
            e.text
        )),
        Err(e) if e.unpaired => {
            log::warn(&format!("no TLS this start ({key_dir}: {}); no client is paired, so plain HTTP with the token", e.text));
            (None, String::from("no TLS"))
        }
        Err(e) => fail(&format!(
            "no TLS this start ({key_dir}: {}) and a client may be paired: not serving plain HTTP instead; ending (init starts the service again)",
            e.text
        )),
    };
    let view = match sys::private_mounts() {
        Ok(()) => String::from("own mount view"),
        Err(e) => format!("no own mount view ({e}, SX-2)"),
    };
    if user.0 != 0
        && let Err(e) = sys::drop_to(user.0, user.1)
    {
        fail(&format!("giving root up for {}:{}: {e}", user.0, user.1));
    }
    if let Err(e) = sys::seccomp(&filter::program()) {
        fail(&format!("the seccomp filter: {e}"));
    }
    // Two lines: the kernel keeps a kmsg line to about 240 bytes.
    log::info(&tls_line);
    log::info(&format!(
        "serving MCP on port {} (mcp={}), uid {}, seccomp on, {} gate sessions, {view}",
        hello.port,
        hello.mode.name(),
        user.0,
        links.len()
    ));
    let svc = serve::Service::with_tls(Pool::new(links), serve::Limits::new(conns), tls);
    svc.set_strict(hello.mode == Mode::Service);
    serve::run(listener, svc);
    exit(0)
}

/// `bitsmcp filter-probe exec|fork`: proves the filter on the running kernel (V2 through MCP `run`, Linux in
/// the host test). One line per call; the last call must end the process with SIGSYS (KILL_PROCESS).
fn filter_probe(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let last = args.first().map(String::as_str).unwrap_or("exec");
    if !matches!(last, "exec" | "fork") || args.len() > 1 {
        eprintln!("{USAGE}");
        exit(2)
    }
    if sys::euid() == 0 {
        let (u, g) = bitsmcp::SERVICE_ID;
        if let Err(e) = sys::drop_to(u, g) {
            println!("drop: {e}");
            exit(1)
        }
        println!("drop: uid {u}");
    }
    if let Err(e) = sys::seccomp(&filter::program()) {
        println!("filter: {e}");
        exit(1)
    }
    println!("filter: on");
    fn said<T>(r: std::io::Result<T>) -> String {
        match r {
            Ok(_) => String::from("ok"),
            Err(e) => format!("errno {}", e.raw_os_error().unwrap_or(0)),
        }
    }
    println!("open: {}", said(std::fs::File::open("/proc/self/status")));
    println!("socket: {}", said(std::net::TcpListener::bind(("127.0.0.1", 0))));
    println!("thread: {}", if std::thread::spawn(|| 7).join().ok() == Some(7) { "ok" } else { "failed" });
    if last == "exec" {
        let e = std::process::Command::new("/bin/true").exec();
        println!("exec: returned ({e})");
    } else {
        println!("fork: returned {}", sys::raw_fork());
    }
    exit(1)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("filter-probe") => filter_probe(&args[1..]),
        Some("mode") => match std::fs::read_to_string(gate::PROC_FILE) {
            Ok(t) => print!("{t}"),
            Err(e) => {
                eprintln!("bitsmcp: {}: {e}", gate::PROC_FILE);
                exit(1)
            }
        },
        Some("--help" | "-h" | "help") => println!("{USAGE}"),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    }
}
