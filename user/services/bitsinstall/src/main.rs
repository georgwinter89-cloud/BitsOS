//! The `bitsinstall` command (MC-9a). `bitsinstall serve` is what init starts under CONFIG.TXT mcp=both|service.
//!
//! Setup, while the process has one thread: /dev/kmsg, `/proc/bitsos/mcp` (mode; `confirmer=test` from
//! CONFIG.TXT `mcp_confirmer=test`), the gate session and its `claim` (the kernel says whether this is the
//! installer), not dumpable, the confirmer socket, no_new_privs and the seccomp deny list (`filter`); then the
//! grants from the stick (waiting for its mount, bounded) and the loop: `next`, decide, `decide`. With no
//! confirmer connected the text console asks (MC-11, `tty`), except under mcp_confirmer=test.

mod sys;
mod tty;

use bitsinstall::access::{self as full, Access};
use bitsinstall::confirm::{self, Input};
use bitsinstall::console::Fallback;
use bitsinstall::decide::{Keep, State};
use bitsinstall::hub::{Hub, PeerCheck, unix_now};
use bitsinstall::gate_loop;
use bitsinstall::link::{self, DevLink};
use bitsinstall::store::{self, FileKeep};
use bitsinstall::{filter, log};
use mcp_wire::gate;
use mcp_wire::installer::{DIALOG_DEFAULT_MS, DIALOG_MAX_MS};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::exit;
use std::sync::Arc;
use std::time::{Duration, Instant};

const USAGE: &str = "usage: bitsinstall serve [--node PATH] [--socket PATH] [--grants DIR] [--dialog-ms N]
  serve           the installer (init starts it under CONFIG.TXT mcp=both or mcp=service)
  --node          the gate (default /dev/bitsos/mcp)
  --socket        where the confirmer (bitscomp) connects (default /run/bitsos/install.sock)
  --grants        where standing grants are kept (default /boot/BITSOS2/mcp, the stick's \\BITSOS2\\mcp\\)
  --dialog-ms     how long a dialog waits for its answer (default 60000; 5000 under mcp_confirmer=test)
bitsinstall claim-probe [--node PATH]
                  opens the gate and asks for the installer's role once (prints and logs what the kernel said)
bitsinstall test-confirmer [--socket PATH] [--answers LIST] [--grant KIND:SECONDS] [--hold-ms N]
                  a confirmer for QEMU scenarios (taken only under CONFIG.TXT mcp_confirmer=test): answers the
                  dialogs in turn from LIST (yes, no, silent, injected; for the access dialog access:H[:nvme],
                  access-injected:H, revoke), sets a grant first if asked
bitsinstall filter-probe exec|fork|console
                  no_new_privs and the filter, then open (allowed), a thread (allowed) and execve or fork (SIGSYS)";

/// The dialog time with the test confirmer (it answers at once or never).
const TEST_DIALOG_MS: u64 = 5_000;
/// How long the service waits for the stick before it reads the grants.
const STICK_WAIT: Duration = Duration::from_secs(8);

fn fail(msg: &str) -> ! {
    log::warn(msg);
    exit(1)
}

fn flags(args: &[String], known: &[&str]) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match it.next() {
            Some(v) if known.contains(&a.as_str()) => {
                m.insert(a.clone(), v.clone());
            }
            _ => {
                eprintln!("{USAGE}");
                exit(2)
            }
        }
    }
    m
}

/// What `/proc/bitsos/mcp` says: (mode, test confirmer).
fn mode() -> (String, bool) {
    let t = std::fs::read_to_string(gate::PROC_FILE).unwrap_or_default();
    let word = |k: &str| t.split_whitespace().find_map(|w| w.strip_prefix(k)).map(String::from);
    (word("mode=").unwrap_or_else(|| String::from("kernel")), word("confirmer=").as_deref() == Some("test"))
}

/// The real check: SO_PEERCRED, a confirmer's uid, its executable, the compositor grant (`bitsinstall::judge_peer`).
fn peer_check(exes: &'static [&'static str]) -> PeerCheck {
    Box::new(move |s: &UnixStream| {
        let (pid, uid, _) = sys::peer_cred(s)?;
        let got = std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|e| format!("pid {pid}: /proc/{pid}/exe: {e}"))?;
        let holder = match std::fs::read_to_string(bitsinstall::COMPOSITOR_HOLDER) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", bitsinstall::COMPOSITOR_HOLDER)),
        };
        bitsinstall::judge_peer(exes, pid, uid, &got, holder.as_deref())
    })
}

fn serve(args: &[String]) -> ! {
    let f = flags(args, &["--node", "--socket", "--grants", "--dialog-ms"]);
    let node = f.get("--node").cloned().unwrap_or_else(|| String::from(gate::NODE));
    let sock = f.get("--socket").cloned().unwrap_or_else(|| String::from(confirm::SOCKET));
    let grants = f.get("--grants").cloned().unwrap_or_else(|| String::from(store::DIR));
    log::open();
    let (m, test) = mode();
    if m == "kernel" {
        log::info("mcp=kernel: the kernel serves MCP itself; no installer needed");
        exit(0);
    }
    let dialog_ms = match f.get("--dialog-ms") {
        Some(v) => v.parse::<u64>().ok().filter(|ms| (1..=DIALOG_MAX_MS).contains(ms)).unwrap_or_else(|| fail(&format!("--dialog-ms {v}: 1 to {DIALOG_MAX_MS}"))),
        None if test => TEST_DIALOG_MS,
        None => DIALOG_DEFAULT_MS,
    };
    let mut gate_link = DevLink::open(&node).unwrap_or_else(|e| fail(&format!("{node}: {e}")));
    match link::claim(&mut gate_link) {
        Ok(pid) => log::info(&format!("the kernel gave pid {pid} the installer's role")),
        Err(link::EBUSY) => fail("another installer holds the role (EBUSY)"),
        Err(e) => fail(&format!("the kernel refused the installer's role (errno {e}): not the boot's /bin/bitsinstall, or not root")),
    }
    if let Err(e) = sys::not_dumpable() {
        log::warn(&e);
    }
    let _ = std::fs::remove_file(&sock);
    if let Some(dir) = Path::new(&sock).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let listener = UnixListener::bind(&sock).unwrap_or_else(|e| fail(&format!("{sock}: {e}")));
    // Anyone may connect; who is taken is `peer_check`'s question (bitscomp runs as the desktop user).
    if let Err(e) = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)) {
        log::warn(&format!("{sock}: chmod: {e}"));
    }
    sys::seccomp(&filter::program()).unwrap_or_else(|e| fail(&e));
    const TEST: [&str; 1] = [bitsinstall::TEST_CONFIRMER];
    let exe: &'static [&'static str] = if test { &TEST } else { &bitsinstall::CONFIRMERS };
    if test {
        log::warn("CONFIG.TXT mcp_confirmer=test: confirmations come from `bitsinstall test-confirmer`, not from a person (QEMU only)");
    }
    let keep = FileKeep::on_stick(Path::new(&grants), Path::new(store::MOUNT));
    let kept = keep.load(STICK_WAIT).unwrap_or_else(|e| {
        log::warn(&format!("no grants read ({e}); starting with none"));
        Default::default()
    });
    // MC-10: full access, kept on the data volume (root's own, checked), judged from now; the kernel gets the
    // table first. A revoke the volume did not take is noted on the stick (`tomb`); the wall clock's high-water
    // mark in the file ends every grant when the clock was set back across the start.
    let akeep = FileKeep::on_volume(Path::new(full::DIR), Path::new(full::MOUNT)).owned_by(0);
    let tomb = FileKeep::on_stick(Path::new(&grants), Path::new(store::MOUNT)).named(full::TOMB_NAME);
    let revoked = tomb.load_text(Duration::ZERO).ok().flatten().as_deref().and_then(full::revoked_at);
    let alist = match akeep.load_text(DATA_WAIT) {
        Ok(Some(t)) => {
            let (v, bad) = full::from_text(&t);
            if bad > 0 {
                log::warn(&format!("{}/grants: {bad} line(s) not taken", full::DIR));
            }
            full::restore(v, full::high_water(&t), revoked, unix_now())
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            log::warn(&format!("no full-access grants read ({e}); starting with none"));
            Vec::new()
        }
    };
    let rescan = || {
        let ok = full::confirmed_in(&confirmed_keep().load_text(Duration::ZERO).ok().flatten().unwrap_or_default());
        full::clients_in(Path::new(full::PAIRED)).into_iter().filter(|c| ok.contains(&c.fp)).collect()
    };
    let record = |fp: &str| {
        let k = confirmed_keep();
        let t = k.load_text(Duration::ZERO)?.unwrap_or_default();
        k.save(&full::with_confirmed(&t, fp))
    };
    let access = Arc::new(
        Access::new(alist, Some(Box::new(akeep) as Box<dyn Keep>), Box::new(clock), Box::new(rescan))
            .with_tombstone(Box::new(tomb), revoked.is_some())
            .with_pairing_record(Box::new(record)),
    );
    let state = Arc::new(State::new(kept, Some(Box::new(keep) as Box<dyn Keep>), Duration::from_millis(dialog_ms)).with_access(access.clone()));
    state.sweep(unix_now());
    let hub = Hub::new(state.clone(), peer_check(exe), unix_now);
    hub.serve(listener).unwrap_or_else(|e| fail(&format!("confirmer thread: {e}")));
    log::info(&format!(
        "serving installer requests; confirmer {} on {sock}; dialogs wait {} s; {} grant(s), {} full-access grant(s), {} paired client(s)",
        exe.join(" or "),
        dialog_ms / 1000,
        state.grants().len(),
        access.table().len(),
        access.clients().len()
    ));
    // MC-11: with no confirmer connected the text console asks (real keyboard only); not under
    // mcp_confirmer=test, whose scenarios script every answer and type into the root console themselves.
    let console = (!test).then(|| Box::new(tty::TextConsole::new()) as Box<dyn bitsinstall::console::Console>);
    let base = Instant::now();
    let confirmer = Arc::new(Fallback::new(hub, console, Box::new(move || base.elapsed())));
    if test && let Some(d) = test_grants_delay() {
        // Fixer (MC-11 review): QEMU only (mcp_confirmer=test): holds the kernel's first table back, so a scenario
        // sees the "grants not loaded yet" window end to end; fewer grants for a while, never more.
        log::warn(&format!("CONFIG.TXT mcp_test_grants_delay_s: the kernel gets the grants {} s late (QEMU only)", d.as_secs()));
        std::thread::sleep(d);
    }
    let never = std::sync::atomic::AtomicBool::new(false);
    match gate_loop::run(&mut gate_link, state, access, confirmer, unix_now, &never) {
        Err(e) => fail(&format!("the gate answered errno {e}; the installer's role is lost")),
        Ok(()) => exit(0),
    }
}

/// MC-10: the keys Georg paired at the keyboard (the stick, next to bitsmcp's pairings).
fn confirmed_keep() -> FileKeep {
    FileKeep::on_stick(Path::new(full::CONFIRMED_DIR), Path::new(store::MOUNT)).named(full::CONFIRMED_NAME)
}

/// MC-11 fixer, QEMU only: CONFIG.TXT `mcp_test_grants_delay_s=N` (at most 30) under mcp_confirmer=test.
fn test_grants_delay() -> Option<Duration> {
    let t = std::fs::read_to_string(Path::new(store::MOUNT).join("BITSOS2/CONFIG.TXT")).ok()?;
    let v = t.lines().find_map(|l| l.trim().strip_prefix("mcp_test_grants_delay_s="))?;
    let s: u64 = v.trim().parse().ok()?;
    (s > 0).then(|| Duration::from_secs(s.min(30)))
}

/// How long the service waits for the data volume before it reads the full-access grants.
const DATA_WAIT: Duration = Duration::from_secs(20);

/// MC-10: (Unix seconds, monotonic seconds since the start).
fn clock() -> (i64, i64) {
    static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    (unix_now(), BASE.get_or_init(Instant::now).elapsed().as_secs() as i64)
}

fn claim_probe(args: &[String]) -> ! {
    let f = flags(args, &["--node"]);
    let node = f.get("--node").cloned().unwrap_or_else(|| String::from(gate::NODE));
    log::open();
    let line = match DevLink::open(&node) {
        Err(e) => format!("claim-probe: open {node}: errno {}", e.raw_os_error().unwrap_or(0)),
        Ok(mut l) => match link::claim(&mut l) {
            Ok(pid) => format!("claim-probe: claimed (pid {pid})"),
            Err(e) => format!("claim-probe: claim: errno {e}"),
        },
    };
    println!("{line}");
    log::info(&line);
    exit(0)
}

/// The QEMU scenarios' confirmer: no screen, no input; it answers what it is told.
fn test_confirmer(args: &[String]) -> ! {
    let f = flags(args, &["--socket", "--answers", "--grant", "--hold-ms"]);
    let sock = f.get("--socket").cloned().unwrap_or_else(|| String::from(confirm::SOCKET));
    let mut answers: std::collections::VecDeque<String> = f.get("--answers").map(|a| a.split(',').map(String::from).collect()).unwrap_or_default();
    let hold = Duration::from_millis(f.get("--hold-ms").and_then(|v| v.parse().ok()).unwrap_or(600_000));
    log::open();
    let say = |m: &str| {
        println!("test-confirmer: {m}");
        log::info(&format!("test-confirmer: {m}"));
    };
    let until = Instant::now() + Duration::from_secs(10);
    let mut s = loop {
        match UnixStream::connect(&sock) {
            Ok(s) => break s,
            Err(e) if Instant::now() >= until => fail(&format!("test-confirmer: {sock}: {e}")),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
    let mut r = BufReader::new(s.try_clone().unwrap_or_else(|e| fail(&format!("test-confirmer: {e}"))));
    if let Some(g) = f.get("--grant") {
        let (kind, secs) = g.split_once(':').and_then(|(k, s)| Some((k, s.parse::<i64>().ok()?))).unwrap_or_else(|| fail("--grant KIND:SECONDS"));
        let _ = s.write_all(confirm::grant(kind, unix_now() + secs, Input::Physical).as_bytes());
        say(&format!("grant {kind} for {secs} s sent"));
    }
    let end = Instant::now() + hold;
    let mut line = String::new();
    while Instant::now() < end {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) => {
                say("the installer closed the connection");
                exit(0)
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => fail(&format!("test-confirmer: {e}")),
        }
        let t = line.trim_end();
        if let Some((id, kind, title, _, _)) = confirm::parse_ask(t) {
            let what = answers.pop_front().unwrap_or_else(|| String::from("silent"));
            say(&format!("ask {id} ({kind}, \"{title}\"): {what}"));
            let client = mcp_wire::Json::parse(t).ok().and_then(|j| j.get("client").and_then(mcp_wire::Json::as_str).map(String::from)).unwrap_or_default();
            let reply = match what.as_str() {
                "yes" => Some(confirm::answer(id, true, Input::Physical)),
                "no" => Some(confirm::answer(id, false, Input::Physical)),
                "injected" => Some(confirm::answer(id, true, Input::Injected)),
                // MC-10: access:H[:nvme] chooses H hours in the access dialog; access-injected the same from
                // injected input; revoke ends full access at the keyboard and says no to the question.
                "revoke" => {
                    let _ = s.write_all(confirm::revoke("access").as_bytes());
                    Some(confirm::answer(id, false, Input::Physical))
                }
                w if w.starts_with("access") => {
                    let mut p = w.split(':').skip(1);
                    let hours = p.next().and_then(|h| h.parse::<i64>().ok()).unwrap_or(mcp_wire::access::DEFAULT_H);
                    let nvme = p.next() == Some("nvme");
                    let input = if w.starts_with("access-injected") { Input::Injected } else { Input::Physical };
                    Some(confirm::access_answer(Some(id), &client, hours, nvme, input))
                }
                _ => None,
            };
            if let Some(reply) = reply {
                let _ = s.write_all(reply.as_bytes());
            }
        } else {
            say(&format!("got {}", t.chars().take(160).collect::<String>()));
        }
    }
    say("hold time over");
    exit(0)
}

fn filter_probe(what: &str) -> ! {
    sys::seccomp(&filter::program()).unwrap_or_else(|e| fail(&e));
    println!("filter: on (euid {})", sys::euid());
    match std::fs::File::open("/proc/self/status") {
        Ok(_) => println!("open: ok"),
        Err(e) => println!("open: errno {}", e.raw_os_error().unwrap_or(0)),
    }
    let t = std::thread::spawn(|| 7).join();
    println!("thread: {}", if matches!(t, Ok(7)) { "ok" } else { "failed" });
    let _ = std::io::stdout().flush();
    match what {
        "console" => {
            // Fixer (MC-11 review): the text console's calls pass the filter (the deny list must not grow
            // over them): open+write, the evdev/VT ioctls (on /dev/null: ENOTTY, never EPERM or SIGSYS), poll,
            // getdents, /dev/urandom, a sleep.
            let null = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null");
            let mut bad = Vec::new();
            match null {
                Err(e) => bad.push(format!("open /dev/null: {e}")),
                Ok(mut f) => {
                    if let Err(e) = f.write_all(b"x") {
                        bad.push(format!("write: {e}"));
                    }
                    let mut b = [0u8; 96];
                    for (what, r) in [
                        ("ioctl_read", sys::ioctl_read(&f, 0x4B3B, &mut b[..4]).map(|_| ())),
                        ("ioctl_read EVIOCGKEY", sys::ioctl_read(&f, (2 << 30) | (96 << 16) | (0x45 << 8) | 0x18, &mut b).map(|_| ())),
                        ("ioctl_int", sys::ioctl_int(&f, (1 << 30) | (4 << 16) | (0x45 << 8) | 0xb0, 1)),
                        ("ioctl_val", sys::ioctl_val(&f, (1 << 30) | (4 << 16) | (0x45 << 8) | 0x90, 1)),
                    ] {
                        if let Err(e) = r
                            && (e == libc::EPERM || e == libc::EACCES)
                        {
                            bad.push(format!("{what}: errno {e}"));
                        }
                    }
                    if sys::poll_in(&[std::os::fd::AsRawFd::as_raw_fd(&f)], 10) != [true] {
                        bad.push(String::from("poll: /dev/null not readable"));
                    }
                }
            }
            if std::fs::read_dir("/dev").map(|d| d.count()).unwrap_or(0) == 0 {
                bad.push(String::from("read_dir /dev"));
            }
            if bitsinstall::console::random().is_none() {
                bad.push(String::from("/dev/urandom"));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            println!("console: {}", if bad.is_empty() { String::from("ok") } else { bad.join("; ") });
            let _ = std::io::stdout().flush();
            exit(0)
        }
        "exec" => {
            let e = std::process::Command::new("/bin/true").status();
            println!("exec returned: {e:?}");
        }
        _ => println!("fork returned: {}", sys::raw_fork()),
    }
    exit(3)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("claim-probe") => claim_probe(&args[1..]),
        Some("test-confirmer") => test_confirmer(&args[1..]),
        Some("filter-probe") if args.len() == 2 && ["exec", "fork", "console"].contains(&args[1].as_str()) => filter_probe(&args[1]),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    }
}
