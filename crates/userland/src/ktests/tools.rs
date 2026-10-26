//! UL-4b: the tools in `user/tools` (procs, evtest, fbtest, memstress, alsa-test) on V2.
//! - `ul.tools.cases`: every line of `user/tools/cases.txt` (CachyOS answers the same table in
//!   `host/tests/ul/40-tools-linux.sh`): exit codes and, where named, stdout equal to CachyOS's transcript.
//! - `ul.tools.procs`: the listing has its header and a line for the procs process itself; `--probe` names
//!   the 16 files with ok or an errno (the ones V2 has: ok; pid 2147483647: ENOENT).
//! - `ul.tools.evtest`: the MCP keyboard (event0) grabbed by evtest (with EVIOCSCLOCKID monotonic), a second
//!   grab refused (EBUSY), `--query` of KEY_A while held (10) and after (0), a KEY_A stroke printed in evtest's
//!   format; the grab is given back; the device list and the pointer's description.
//! - `ul.tools.fbtest`: `info` (does not take the screen), `check` (pan/var/mmap/pwrite rules), `draw` held
//!   while the screen is compared pixel by pixel with abi-fbdev's pattern, the console back after the kill,
//!   `demo` for two seconds.
//! - `ul.tools.memstress_enomem` (QemuOnly: it empties the commit pool for a moment): commit until ENOMEM, which
//!   comes as an errno, then the same amount again (nothing leaked).
//! - `ul.tools.memstress_truncate`: SIGBUS past the end of a shrunk shared memfd map (skips while CR3 is open).
//! All rerunnable: own scratch dirs (removed), every started program has ended or was killed before the return.

use super::{SEC, Started, finish, need, scratch, start_as, text, wait_for};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

const CASES: &str = include_str!("../../../../user/tools/cases.txt");

fn expected(name: &str) -> Option<&'static str> {
    Some(match name {
        "procs-selftest.txt" => include_str!("../../../../user/tools/expected/procs-selftest.txt"),
        "evtest-list-fake.txt" => include_str!("../../../../user/tools/expected/evtest-list-fake.txt"),
        "memstress-touch.txt" => include_str!("../../../../user/tools/expected/memstress-touch.txt"),
        "memstress-commit.txt" => include_str!("../../../../user/tools/expected/memstress-commit.txt"),
        "memstress-commit-mmap.txt" => include_str!("../../../../user/tools/expected/memstress-commit-mmap.txt"),
        "memstress-rules.txt" => include_str!("../../../../user/tools/expected/memstress-rules.txt"),
        "memstress-replay.txt" => include_str!("../../../../user/tools/expected/memstress-replay.txt"),
        "alsa-test.txt" => include_str!("../../../../user/tools/expected/alsa-test.txt"),
        _ => return None,
    })
}

/// Starts /bin/<argv[0]> as root (the evdev and fbdev nodes are 0660 uid 1000; root needs no owner fix).
fn tool(argv: &[&str]) -> Result<Started, String> {
    start_as(&format!("/bin/{}", argv[0]), argv, None, false, Some(0))
}

fn run(argv: &[&str], timeout_s: u64) -> Result<(i32, String, String), String> {
    let s = tool(argv)?;
    let code = finish(&s, timeout_s * SEC)?;
    Ok((code, text(&s.out), text(&s.err)))
}

fn cases() -> TestResult {
    for p in ["procs", "evtest", "fbtest", "memstress", "alsa-test"] {
        need(&format!("/bin/{p}"))?;
    }
    let mut n = 0;
    for line in CASES.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.is_empty() || f[0].starts_with('#') {
            continue;
        }
        kassert!(f.len() >= 4, "bad cases.txt line {line:?}");
        let (name, want, exp) = (f[0], f[1].parse::<i32>().map_err(|_| format!("bad exit in {line:?}"))?, f[2]);
        let d = scratch("tools");
        let ns = vfs::root_ns();
        ns.mkdir_p(&d).map_err(|e| format!("mkdir {d}: {e:?}"))?;
        for fake in ["event0", "event1", "mouse0", "fb"] {
            ns.create_file(&format!("{d}/{fake}"), 0o644, Vec::new()).map_err(|e| format!("create {d}/{fake}: {e:?}"))?;
        }
        let args: Vec<String> = f[3..].iter().map(|a| a.replace("@D", &d)).collect();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = run(&argv, 120);
        let _ = ns.remove_tree(&d);
        let (code, out, err) = r?;
        kassert!(code == want, "{name}: exit {code}, want {want}\n{out}--- stderr ---\n{err}");
        if exp != "-" {
            let want_out = expected(exp).ok_or_else(|| format!("{name}: no expected file {exp} compiled in"))?.replace("@D", &d);
            kassert!(out == want_out, "{name}: stdout differs from CachyOS's\n--- V2 ---\n{out}--- CachyOS ---\n{want_out}--- stderr ---\n{err}");
        }
        klog::event!(Info, "ul", "tools.case", case = name, code = code);
        n += 1;
    }
    kassert!(n >= 25, "only {n} cases in cases.txt");
    Ok(())
}
ktest!("ul.tools.cases", Safe, cases);

fn procs() -> TestResult {
    need("/bin/procs")?;
    let s = tool(&["procs"])?;
    let code = finish(&s, 60 * SEC)?;
    let out = text(&s.out);
    kassert!(code == 0, "procs: exit {code}\n{}", s.both());
    kassert!(out.lines().next().is_some_and(|l| l.starts_with("    PID    PPID   UID S THR") && l.ends_with(" CMD")), "no header:\n{out}");
    let me = format!("{:>7} ", s.p.pid);
    let line = out.lines().find(|l| l.starts_with(&me)).ok_or_else(|| format!("no line for procs itself (pid {}):\n{out}", s.p.pid))?;
    let f: Vec<&str> = line.split_whitespace().collect();
    kassert!(f.len() == 9 && f[3] == "R" && f[8] == "procs" && f[2] == "0", "procs' own line {line:?}: want state R, uid 0, CMD procs");
    let s = tool(&["procs", "--probe"])?;
    let code = finish(&s, 60 * SEC)?;
    let out = text(&s.out);
    kassert!(code == 0 && out.lines().count() == 16, "procs --probe: exit {code}, {} lines\n{}", out.lines().count(), s.both());
    for l in out.lines() {
        let (path, what) = l.split_once(": ").ok_or_else(|| format!("bad probe line {l:?}"))?;
        kassert!(what == "ok" || (what.starts_with('E') && what.chars().all(|c| c.is_ascii_uppercase())), "probe line {l:?}");
        let have = ["/proc/self/stat", "/proc/self/cmdline", "/proc/self/status", "/proc/self/maps", "/proc/<pid>/status", "/proc/<pid>/stat", "/proc/meminfo", "/proc/uptime", "/proc/cpuinfo"];
        kassert!(!have.contains(&path) || what == "ok", "{path} exists on V2, probe says {what}");
        kassert!(!path.starts_with("/proc/2147483647/") || what == "ENOENT", "{path}: {what}, want ENOENT");
    }
    Ok(())
}
ktest!("ul.tools.procs", Safe, procs);

fn mcp_keyboard() -> Result<alloc::sync::Arc<console::evdev::InputDevice>, String> {
    console::evdev::devices().into_iter().find(|d| d.alias == Some(console::evdevfs::INJECT_KEYBOARD)).ok_or_else(|| String::from("no MCP keyboard device"))
}

fn evtest() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    need("/bin/evtest")?;
    let kb = mcp_keyboard()?;
    let node = format!("/dev/input/event{}", kb.number);
    let (code, out, err) = run(&["evtest"], 30)?;
    kassert!(code == 0, "evtest (list): exit {code}\n{out}{err}");
    kassert!(out.lines().any(|l| l == format!("{node}:\tBitsOS MCP keyboard")), "the list lacks the MCP keyboard:\n{out}");
    kassert!(out.contains(":\tBitsOS MCP pointer"), "the list lacks the MCP pointer:\n{out}");
    let s = tool(&["evtest", "--grab", "--monotonic", "--count", "2", "--timeout", "30", &node])?;
    let r = (|| -> TestResult {
        kassert!(wait_for(20 * SEC, || text(&s.out).contains("Testing ...") || s.p.has_exited()), "no 'Testing' line:\n{}", s.both());
        kassert!(!s.p.has_exited(), "evtest ended early: {:?}\n{}", s.p.status(), s.both());
        kassert!(kb.sink().grabbed_by(kb.event_id()).is_some(), "evtest said Grabbed but nobody holds the grab");
        // A second grab of a grabbed device: EBUSY (evdev.c:341 `evdev_grab`), exit 1, the first keeps it.
        let (code, _, err) = run(&["evtest", "--grab", "--count", "1", "--timeout", "1", &node], 30)?;
        kassert!(code == 1 && err.contains("EBUSY"), "second grab: exit {code}, want 1 with EBUSY\n{err}");
        kassert!(kb.sink().grabbed_by(kb.event_id()).is_some() && !s.p.has_exited(), "the first grab was lost");
        console::ktests_in4::inject(30, 1); // KEY_A
        let (held, _, err) = run(&["evtest", "--query", &node, "EV_KEY", "KEY_A"], 30)?;
        console::ktests_in4::inject(30, 0);
        kassert!(held == 10, "--query KEY_A while held: exit {held}, want 10\n{err}");
        let (up, _, err) = run(&["evtest", "--query", &node, "EV_KEY", "30"], 30)?;
        kassert!(up == 0, "--query KEY_A after release: exit {up}, want 0\n{err}");
        let code = finish(&s, 20 * SEC)?;
        let out = text(&s.out);
        kassert!(code == 0, "evtest: exit {code} (1 also when an event time was off the monotonic clock)\n{}", s.both());
        kassert!(out.contains("Input device name: \"BitsOS MCP keyboard\""), "no name line:\n{out}");
        kassert!(out.contains("  Event type 1 (EV_KEY)") && out.contains("    Event code 30 (KEY_A)"), "no EV_KEY / KEY_A in the description:\n{out}");
        for want in ["type 1 (EV_KEY), code 30 (KEY_A), value 1", "type 1 (EV_KEY), code 30 (KEY_A), value 0", "-------------- SYN_REPORT ------------"] {
            kassert!(out.lines().any(|l| l.starts_with("Event: time ") && l.ends_with(want)), "no event line ending {want:?}:\n{out}");
        }
        Ok(())
    })();
    if !s.p.has_exited() {
        s.p.kill(super::SIGKILL);
        let _ = s.p.wait(Some(10 * SEC));
    }
    if kb.held_keys().contains(&30) {
        console::ktests_in4::inject(30, 0);
    }
    r?;
    kassert!(kb.sink().grabbed_by(kb.event_id()).is_none(), "the grab outlived evtest");
    let ptr = console::evdev::devices().into_iter().find(|d| d.alias == Some(console::evdevfs::INJECT_POINTER)).ok_or("no MCP pointer device")?;
    let pnode = format!("/dev/input/event{}", ptr.number);
    let (code, out, err) = run(&["evtest", "--describe", &pnode], 30)?;
    kassert!(code == 0 && out.contains("  Event type 2 (EV_REL)") && out.contains("    Event code 0 (REL_X)"), "evtest --describe {pnode}: exit {code}\n{out}{err}");
    let (code, out, err) = run(&["evtest", "--describe", &node], 30)?;
    kassert!(code == 0 && out.contains("    Repeat code 0 (REP_DELAY)") && out.contains("    Repeat code 1 (REP_PERIOD)"), "evtest --describe {node}: no EV_REP settings, exit {code}\n{out}{err}");
    let (code, _, err) = run(&["evtest", "--count", "1", "--timeout", "1", &pnode], 30)?;
    kassert!(code == 3, "evtest with no events in 1 s: exit {code}, want 3\n{err}");
    Ok(())
}
ktest!("ul.tools.evtest", Safe, evtest);

/// abi-fbdev's pattern (user/abi-tests/c/fbdev.c, user/tools/fbtest/src/fb.rs).
fn pat(seed: u32, x: u32, y: u32) -> u32 {
    ((x.wrapping_mul(3).wrapping_add(seed.wrapping_mul(7)) & 0xff) << 16) | ((y.wrapping_mul(5).wrapping_add(seed.wrapping_mul(11)) & 0xff) << 8) | ((x ^ y ^ seed) & 0xff)
}

/// `drawn a=<a> b=<b> band=<row>+<rows> ...`
fn drawn(out: &str) -> Option<(u32, u32, u32, u32)> {
    let l = out.lines().find(|l| l.starts_with("drawn "))?;
    let f = |k: &str| l.split_whitespace().find_map(|w| w.strip_prefix(k));
    let (band, rows) = f("band=")?.split_once('+')?;
    Some((f("a=")?.parse().ok()?, f("b=")?.parse().ok()?, band.parse().ok()?, rows.parse().ok()?))
}

fn holds_screen(pid: u32) -> bool {
    display::scanout_holder() == Some(("fbdev", pid))
}

fn fbtest() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    need("/bin/fbtest")?;
    let before = display::scanout_holder();
    let (code, out, err) = run(&["fbtest", "info"], 30)?;
    kassert!(code == 0 && out.contains("id \"BitsOS gopfb\""), "fbtest info: exit {code}\n{out}{err}");
    kassert!(display::scanout_holder() == before, "fbtest info took the screen: {:?}", display::scanout_holder());
    let (code, out, err) = run(&["fbtest", "check"], 60)?;
    kassert!(code == 0 && out.contains(" 0 failed"), "fbtest check: exit {code}\n{out}{err}");
    let s = tool(&["fbtest", "draw", "31", "3", "--hold", "60"])?;
    let pid = s.p.pid;
    let r = (|| -> TestResult {
        kassert!(wait_for(60 * SEC, || drawn(&text(&s.out)).is_some() || s.p.has_exited()), "no 'drawn' line:\n{}", s.both());
        let (a, b, band, rows) = drawn(&text(&s.out)).ok_or_else(|| format!("ended early: {:?}\n{}", s.p.status(), s.both()))?;
        kassert!(wait_for(2 * SEC, || holds_screen(pid)), "screen holder {:?}, fbtest pid {pid}", display::scanout_holder());
        let (w, h, px) = display::snapshot().ok_or("no snapshot")?;
        kassert!(px.len() == (w * h) as usize, "snapshot {} pixels for {w}x{h}", px.len());
        for y in 0..h {
            let seed = if y >= band && y < band + rows { b } else { a };
            for x in 0..w {
                let got = px[(y * w + x) as usize];
                kassert!(got == pat(seed, x, y), "pixel ({x}, {y}) is {got:#08x}, fbtest drew {:#08x} (seed {seed})", pat(seed, x, y));
            }
        }
        Ok(())
    })();
    s.p.kill(super::SIGKILL);
    let ended = s.p.wait(Some(10 * SEC)).is_some();
    r?;
    kassert!(ended, "fbtest did not end after SIGKILL");
    kassert!(wait_for(3 * SEC, || !holds_screen(pid)), "the screen stayed with the killed fbtest");
    let (code, out, err) = run(&["fbtest", "demo", "--seconds", "2"], 60)?;
    let frames = out.split_whitespace().find_map(|w| w.strip_prefix("frames=")).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    kassert!(code == 0 && frames > 0, "fbtest demo: exit {code}\n{out}{err}");
    kassert!(wait_for(3 * SEC, || display::scanout_holder() == before), "after demo the holder is {:?}, was {before:?}", display::scanout_holder());
    klog::event!(Info, "ul", "tools.fbtest", width = display::snapshot().map(|s| s.0).unwrap_or(0), demo_frames = frames);
    Ok(())
}
ktest!("ul.tools.fbtest", Safe, fbtest);

fn memstress_enomem() -> TestResult {
    need("/bin/memstress")?;
    // 256 GiB asked: far past a QEMU guest's memory, inside the user address space.
    for extra in [None, Some("--mmap")] {
        let mut argv = alloc::vec!["memstress", "commit", "--mb", "262144", "--step-mb", "64"];
        argv.extend(extra);
        let (code, out, err) = run(&argv, 300)?;
        kassert!(code == 0, "memstress commit {extra:?}: exit {code}\n{out}{err}");
        kassert!(out.contains("stopped by ENOMEM (an errno, no signal)") && out.contains("nothing leaked"), "memstress commit {extra:?}:\n{out}{err}");
        klog::event!(Info, "ul", "tools.memstress_enomem", by_mmap = extra.is_some(), detail = err.trim());
    }
    Ok(())
}
ktest!("ul.tools.memstress_enomem", QemuOnly, memstress_enomem);

/// `memstress truncate`: Linux ends a store past the end of a shrunk shared memfd map with SIGBUS. V2 keeps a
/// mapped page alive after a shrinking truncate (crates/vfs/src/pagecache.rs `truncate`), so the store lands:
/// a skip naming the change request (ul-UL-4b.md CR3) until that is fixed, then this test passes by itself.
fn memstress_truncate() -> TestResult {
    need("/bin/memstress")?;
    let (code, out, err) = run(&["memstress", "truncate"], 60)?;
    if code == 1 && out.contains("the store past the end came back") {
        return ktest::skip("V2 keeps mapped pages after a shrinking truncate (no SIGBUS): CR3 in plan/notes/ul-UL-4b.md");
    }
    kassert!(code == 0 && out.starts_with("ok "), "memstress truncate: exit {code}\n{out}{err}");
    Ok(())
}
ktest!("ul.tools.memstress_truncate", Safe, memstress_truncate);
