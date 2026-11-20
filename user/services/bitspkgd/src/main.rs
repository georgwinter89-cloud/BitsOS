//! `bitspkgd serve` (init starts it after settingsd) and `bitspkgd call JSON` (one request, for root's console
//! and the tests).
#![forbid(unsafe_code)]

use bitspkgd::Ctx;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "usage: bitspkgd serve [--root DIR] [--socket PATH] [--settings SOCKET|none] [--sources FILE] [--no-mcp] [--no-timer] [--stderr]
       bitspkgd call JSON [--socket PATH] [--wait-s N]";

fn take(a: &mut Vec<String>, k: &str) -> Option<String> {
    let i = a.iter().position(|x| x == k)?;
    a.remove(i);
    if i < a.len() {
        Some(a.remove(i))
    } else {
        eprintln!("{USAGE}");
        exit(2)
    }
}

fn flag(a: &mut Vec<String>, k: &str) -> bool {
    let n = a.len();
    a.retain(|x| x != k);
    a.len() != n
}

/// stderr (bitspkg's event lines) into /dev/kmsg, the journal; init gives the service no terminal.
fn to_kmsg() {
    use std::os::fd::AsRawFd;
    match std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        Ok(f) => {
            if usys::dup2(f.as_raw_fd() as usize, 2).is_ok() {
                // fd 2 holds its own reference now.
                drop(f);
            }
        }
        Err(e) => eprintln!("bitspkgd: /dev/kmsg: {e}; events stay on stderr"),
    }
}

fn serve(mut a: Vec<String>) -> ! {
    let root = take(&mut a, "--root").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    let socket = take(&mut a, "--socket").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(bitspkgd::SOCKET));
    let settings = take(&mut a, "--settings").map(|s| (s != "none").then(|| PathBuf::from(s)));
    let sources = take(&mut a, "--sources").map(PathBuf::from);
    let (no_mcp, no_timer, stderr) = (flag(&mut a, "--no-mcp"), flag(&mut a, "--no-timer"), flag(&mut a, "--stderr"));
    if !a.is_empty() {
        eprintln!("{USAGE}");
        exit(2);
    }
    if !stderr {
        to_kmsg();
    }
    // The cut tests end the process at a named step (bitspkg's cut.rs): they belong to the command line, never to
    // the service everyone's changes go through.
    for v in [bitspkg::cut::ENV, bitspkg::cut::ENV_POINT] {
        if std::env::var_os(v).is_some() {
            eprintln!("bitspkgd: {v} is set: the cut tests run on bitspkg's command line, not in the service");
            exit(2);
        }
    }
    let l = bitspkgd::server::bind(&socket).unwrap_or_else(|e| {
        bitspkgd::event("pkg.daemon.bind_failed", &[("reason", &e)]);
        exit(1)
    });
    let cx = Arc::new(Ctx::new(root, settings, sources));
    bitspkgd::event("pkg.daemon.start", &[("socket", &socket.display().to_string()), ("root", &cx.root.display().to_string()), ("mcp", if no_mcp { "no" } else { "yes" }), ("timer", if no_timer { "no" } else { "yes" })]);
    // settingsd may start just before: the keys (pkg.sources.*, pkg.cpu_level, pkg.space.max_gb, pkg.updates.*,
    // pkg.optional.ask) are registered as soon as it answers, so einstellungen/programme shows them from the boot.
    // init starts settingsd just before bitspkgd, so the first tries come fast (100 ms, doubling up to 15 s): a
    // fixed 15 s left the keys unknown for that long, and a `settings set pkg.*` in that window failed
    // (INT-PK-B, 96-bitspkgd.sh). At most 10 min in all.
    let c = cx.clone();
    let _ = std::thread::Builder::new().name("pkg-keys".into()).spawn(move || {
        let start = std::time::Instant::now();
        let mut wait = Duration::from_millis(100);
        while start.elapsed() < Duration::from_secs(600) {
            let st = c.store();
            match st.hooks.settings_socket().map(|s| bitspkg::repo::sources::from_settings(&bitspkg::settings::Settings { socket: s.to_path_buf() })) {
                None | Some(Ok(_)) => {
                    bitspkgd::event("pkg.daemon.keys", &[("registered", if st.hooks.settings_socket().is_some() { "yes" } else { "no settingsd" })]);
                    return;
                }
                Some(Err(_)) => {
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(15));
                }
            }
        }
        bitspkgd::event("pkg.daemon.keys", &[("registered", "no"), ("reason", "settingsd answered no registration in 10 min")]);
    });
    if !no_mcp {
        bitspkgd::mcp::spawn(cx.clone());
    }
    if !no_timer {
        bitspkgd::timer::spawn(cx.clone());
    }
    println!("bitspkgd: serving on {}", socket.display());
    bitspkgd::server::serve(l, cx);
    exit(1)
}

fn call(mut a: Vec<String>) -> ! {
    let socket = take(&mut a, "--socket").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(bitspkgd::SOCKET));
    let wait = take(&mut a, "--wait-s").map(|v| v.parse::<u64>().unwrap_or_else(|_| exit(2))).unwrap_or(3700);
    if a.len() != 1 {
        eprintln!("{USAGE}");
        exit(2);
    }
    match bitspkgd::client::call(&socket, &a[0], Duration::from_secs(wait), &mut |l| println!("{l}")) {
        Ok(true) => exit(0),
        Ok(false) => exit(1),
        Err(e) => {
            eprintln!("bitspkgd: {e}");
            exit(3)
        }
    }
}

fn main() {
    let mut a: Vec<String> = std::env::args().skip(1).collect();
    if a.is_empty() {
        eprintln!("{USAGE}");
        exit(2);
    }
    match a.remove(0).as_str() {
        "serve" => serve(a),
        "call" => call(a),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    }
}
