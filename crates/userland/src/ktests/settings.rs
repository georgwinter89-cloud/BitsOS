//! CL-1 change request 4: settingsd and its client on V2. A private settingsd (uid 0, scratch stick, fallback,
//! /sys and socket, no MCP bridge) serves a uid 1000 client: set, get and undo theme.accent, refuse an R8 switch
//! name. Rerunnable: fresh scratch directories each run, never the real \BITSOS2\settings.

use super::{MS, SEC, SIGKILL, finish, need, scratch, start_as, text, wait_for};
use alloc::format;
use alloc::string::String;
use ktest::{TestResult, kassert, ktest};

fn client(sock: &str, args: &[&str]) -> Result<(i32, String), String> {
    let mut argv = alloc::vec!["settings", "--socket", sock];
    argv.extend_from_slice(args);
    let s = start_as("/bin/settings", &argv, None, false, Some(1000))?;
    let code = finish(&s, 20 * SEC)?;
    Ok((code, s.both()))
}

fn settings() -> TestResult {
    need("/bin/settingsd")?;
    need("/bin/settings")?;
    let dir = scratch("settings");
    let ns = vfs::root_ns();
    for d in ["", "/stick", "/fb", "/sys"] {
        ns.mkdir_p_bytes(format!("{dir}{d}").as_bytes(), 0o755).map_err(|e| format!("mkdir {dir}{d}: {e}"))?;
    }
    let sock = format!("{dir}/s.sock");
    let (stick, fb, sys) = (format!("{dir}/stick"), format!("{dir}/fb"), format!("{dir}/sys"));
    let argv = ["settingsd", "serve", "--stick", &stick, "--fallback", &fb, "--sys", &sys, "--socket", &sock, "--no-mcp"];
    let d = start_as("/bin/settingsd", &argv, None, false, Some(0))?;
    let r = (|| -> TestResult {
        kassert!(wait_for(10 * SEC, || ns.lookup(&sock).is_ok()), "no socket {sock}:\n{}", d.both());
        let (c, out) = client(&sock, &["set", "theme.accent", "#123456"])?;
        kassert!(c == 0, "set exited {c}:\n{out}");
        let (c, out) = client(&sock, &["get", "theme.accent"])?;
        kassert!(c == 0 && out.starts_with("#123456"), "get after set exited {c}:\n{out}");
        let (c, out) = client(&sock, &["undo"])?;
        kassert!(c == 0, "undo exited {c}:\n{out}");
        let (c, out) = client(&sock, &["get", "theme.accent"])?;
        kassert!(c == 0 && out.starts_with("#00e5ff"), "get after undo exited {c}:\n{out}");
        let (c, out) = client(&sock, &["set", "wlan", "on"])?;
        kassert!(c == 1, "set wlan on exited {c} (want 1, an R8 switch is no key):\n{out}");
        kassert!(!d.p.has_exited(), "settingsd ended: {:?}\n{}", d.p.status(), d.both());
        klog::event!(Info, "ul", "test.settings", daemon_lines = text(&d.err).lines().count());
        Ok(())
    })();
    d.p.kill(SIGKILL);
    let _ = d.p.wait(Some(10 * SEC));
    sched::sleep_ns(10 * MS);
    r
}
ktest!("cl.settings", Safe, settings);
