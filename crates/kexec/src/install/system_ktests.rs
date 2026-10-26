//! In-kernel tests of RB-9.
//!
//! Safe (nothing written; rerunnable; the Mini-PC runs it over MCP):
//! - `rb.install.system_list`: on the real boot stick, `system_list` names every generation with its state, the
//!   running one and the next start as bitsboot's rule says; `system_pin` dry runs (pin the newest confirmed
//!   generation; `now` into another confirmed one, staged from the stick and given back) leave the stick as it was.
//!
//! QemuOnly (writes the stick image of this run, and puts everything back):
//! - `rb.install.system_pin`: pin a confirmed generation (PIN on the stick, next start PINNED), the same again
//!   (nothing written), refusals for a missing and a NEW generation (stick unchanged), off (PIN gone, next start
//!   by the marker rule), and the console command's text; then the stick as it was.

use super::ktests::{e, initrd, kernel, mounted, snapshot};
use super::plan;
use super::system::Want;
use super::system_tool::{list_text, pin_step};
use super::{FatStick, LOCK, Opts, Stick, install_on};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use fs_fat::sysapi::Marker;
use ktest::{TestResult, kassert, ktest};

const SEC: u64 = 1_000_000_000;

fn call(args: &str) -> Result<String, String> {
    call_tool("system_pin", args, 120 * SEC)
}

fn call_tool(name: &str, args: &str, within_ns: u64) -> Result<String, String> {
    let t = mcp::find(name).ok_or_else(|| format!("{name} is not registered"))?;
    let a = mcp::Json::parse(args).map_err(e)?;
    let ctx = mcp::ToolContext::detached(time::now_ns() + within_ns);
    match (t.handler)(&ctx, &a) {
        Ok(out) => Ok(out
            .iter()
            .filter_map(|o| match o {
                mcp::ToolOutput::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")),
        Err(x) => Err(format!("{x:?}")),
    }
}

fn rb_install_system_list() -> TestResult {
    if let Err(r) = mounted() {
        return r;
    }
    kassert!(mcp::find("system_list").is_some() && mcp::find("install_system").is_some(), "tools not registered");
    let before = snapshot()?;
    let t = list_text(30 * SEC)?;
    for g in &before.0 {
        kassert!(t.lines().any(|l| l.trim_start().starts_with(&format!("{} ", g.n)) && l.contains(&g.markers())), "generation {} missing: {t}", g.n);
    }
    let next = super::system::next_start(&before.0, before.1);
    if let Ok((r, _, _)) = &next {
        kassert!(t.contains(&format!("next start: generation {} ({}", r.number, bitsboot_pure::gens::reason_name(r.reason))), "{t}");
    }
    // RB-11: after a firmware start bitsboot's record names the kernel, in hw_info's words; after a Reload the
    // line says which kernel (the Mini-PC runs this after Reloads too).
    let (running, reload) = crate::boot::boot_info().map_or((None, 0), |(bi, _)| (bi.generation(), bi.reload().map_or(0, |r| r.counter)));
    let line = t.lines().find(|l| l.starts_with("running: ")).ok_or_else(|| format!("no running line: {t}"))?;
    match running {
        Some(r) if reload == 0 => {
            let ok = before.0.iter().any(|g| g.n == r.number && g.ok);
            let words = mcp::running_reason_with(&r, ok);
            kassert!(line.starts_with(&format!("running: generation {} ({words}; bitsboot: ", r.number)), "{line}");
            kassert!(!(r.reason == bootinfo::gen_reason::NEWEST_TRY && ok) || line.contains("confirmed (OK) in this boot"), "{line}");
        }
        _ if reload > 0 => kassert!(line.contains(&format!("after Reload {reload}")), "{line}"),
        _ => {}
    }
    klog::event!(Info, "kexec", "ktest.system_list", lines = t.lines().count(), pin = before.1.unwrap_or(0));
    let oks: Vec<u32> = before.0.iter().filter(|g| g.ok && g.kernel && g.initrd).map(|g| g.n).collect();
    if let Some(&newest) = oks.iter().max() {
        let a = call(&format!("{{\"n\":{newest},\"dry_run\":true}}"))?;
        kassert!(a.contains(&format!("generation {newest}")) && a.contains("PINNED"), "{a}");
        // `now` into another confirmed generation: staged from the stick, then given back (dry run).
        if let Some(&other) = oks.iter().rev().find(|&&n| running.is_none_or(|r| r.number != n)) {
            match call(&format!("{{\"n\":{other},\"now\":true,\"dry_run\":true}}")) {
                Ok(a) => kassert!(a.contains(&format!("Reload into generation {other} (dry run")) && a.contains(&format!("SYS\\{other}\\KERNEL")), "{a}"),
                // A machine without the memory for it refuses the staging, which is no failure of this test.
                Err(x) if x.contains("does not fit") || x.contains("too big") => {}
                Err(x) => return Err(x),
            }
        }
    }
    // Hostile arguments: refused before the stick is touched (the handler's own checks, not the server's).
    for bad in ["{}", "[1]", r#"{"n":"x"}"#, r#"{"n":-1}"#, r#"{"n":1.5}"#, r#"{"n":0}"#, r#"{"n":100000000}"#, r#"{"n":"OFF"}"#,
        r#"{"n":1,"now":"yes"}"#, r#"{"n":1,"dry_run":1}"#, r#"{"n":null}"#]
    {
        let r = call(bad);
        kassert!(r.as_ref().is_err_and(|x| x.starts_with("BadArgument")), "{bad}: {r:?}");
    }
    // install_system's argument checks (the same as install_kernel's; all dry runs, so even a wrong acceptance
    // writes nothing): refused before anything is fetched.
    for (bad, kind) in [("{}", "BadArgument"), ("[1]", "BadArgument"), (r#"{"url":5}"#, "BadArgument"),
        (r#"{"url":"http://10.0.2.2/b/KERNEL","dry_run":true}"#, "BadArgument"), (r#"{"url":"http://10.0.2.2/b/","dry_run":"yes"}"#, "BadArgument"),
        (r#"{"url":"http://8.8.8.8/b/","dry_run":true}"#, "Refused"), (r#"{"url":"https://192.168.1.1/b/","dry_run":true}"#, "Refused")]
    {
        let r = call_tool("install_system", bad, 30 * SEC);
        kassert!(r.as_ref().is_err_and(|x| x.starts_with(kind)), "install_system {bad}: {r:?}");
    }
    let r = call("{\"n\":99999999,\"dry_run\":true}");
    kassert!(r.as_ref().is_err_and(|x| x.contains("no generation 99999999")), "{r:?}");
    kassert!(snapshot()? == before, "the stick changed");
    Ok(())
}
ktest!("rb.install.system_list", Safe, rb_install_system_list);

/// Puts PIN, a marker and an extra generation back (also when the test fails half way).
struct Restore {
    pin: Option<u32>,
    marked_ok: Option<u32>,
    made: Option<u32>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _l = LOCK.lock_timeout(60 * SEC);
        let _ = fs_fat::stick::with_sys(|s| {
            let st = FatStick(s);
            if let Some(n) = self.made {
                let _ = st.remove_generation(n);
            }
            if let Some(n) = self.marked_ok {
                let _ = st.set_marker(n, Marker::Ok, false);
            }
            let _ = st.set_pin(self.pin);
        });
    }
}

fn rb_install_system_pin() -> TestResult {
    let vol = match mounted() {
        Ok(v) => v,
        Err(r) => return r,
    };
    kassert!(vol.writable(), "the stick is read-only");
    let (gens, pin, _) = snapshot()?;
    let base = gens.iter().filter(|g| g.kernel && g.initrd).map(|g| g.n).max().ok_or("the stick holds no generation")?;
    let mut restore = Restore { pin, marked_ok: None, made: None };
    if !gens.iter().any(|g| g.n == base && g.ok) {
        fs_fat::stick::with_sys(|s| s.set_marker(base, Marker::Ok, true)).map_err(e)?.map_err(e)?;
        restore.marked_ok = Some(base);
    }
    // 1. Pin base.
    let a = call(&format!("{{\"n\":{base}}}"))?;
    kassert!(a.contains(&format!("PIN set to generation {base}")) && a.contains("PINNED") && a.contains("next start"), "{a}");
    let (g1, p1, _) = snapshot()?;
    kassert!(p1 == Some(base), "PIN on the stick: {p1:?}");
    kassert!(plan::next_boot(&g1, p1).is_ok_and(|x| x.0 == base && x.1 == "PINNED"), "next boot {:?}", plan::next_boot(&g1, p1));
    // 2. The same again writes nothing; `now` into the generation that runs needs no Reload.
    let a = call(&format!("{{\"n\":{base}}}"))?;
    kassert!(a.contains("pinned already"), "{a}");
    let running = crate::boot::boot_info().and_then(|(bi, _)| bi.generation());
    if running.is_some_and(|r| r.number == base) {
        let st = pin_step(Want::Pin(base), true, false, 60 * SEC).map_err(e)?;
        kassert!(st.target.is_none() && st.text.contains("runs already"), "{}", st.text);
    }
    // 3. Refusals change nothing: no such generation, a NEW one.
    let before = snapshot()?;
    let r = call(&format!("{{\"n\":{}}}", base + 1000));
    kassert!(r.as_ref().is_err_and(|x| x.contains("nothing on the stick changed")), "{r:?}");
    kassert!(snapshot()? == before, "a refused pin changed the stick");
    {
        let _l = LOCK.lock_timeout(60 * SEC).ok_or("the install lock is held")?;
        let o = Opts { unpin: false, allow_no_fallback: true, dry_run: false };
        let rep = fs_fat::stick::with_sys(|s| install_on(&FatStick(s), &kernel(9), &initrd(9), None, &o)).map_err(e)?.map_err(|x| String::from(x.text()))?;
        restore.made = Some(rep.plan.number);
    }
    let made = restore.made.unwrap_or(0);
    let before = snapshot()?;
    let r = call(&format!("{{\"n\":{made}}}"));
    kassert!(r.as_ref().is_err_and(|x| x.contains("not confirmed") && x.contains("NEW")), "{r:?}");
    kassert!(snapshot()? == before, "a refused pin changed the stick");
    // `now` after off would be the NEW generation's one try: refused, nothing written.
    let r = pin_step(Want::Off, true, false, 60 * SEC);
    kassert!(r.as_ref().is_err_and(|x| format!("{x:?}").contains("one try")), "{:?}", r.as_ref().err());
    kassert!(snapshot()? == before, "a refused now changed the stick");
    // A `now` that cannot finish (here: no answer to wait for, or the generation's files do not stage) leaves
    // the PIN as it was: it is written only once the Reload is staged and armed.
    fs_fat::stick::with_sys(|s| s.set_marker(made, Marker::Ok, true)).map_err(e)?.map_err(e)?;
    let marked = snapshot()?;
    let r = call(&format!("{{\"n\":{made},\"now\":true}}"));
    kassert!(r.as_ref().is_err_and(|x| x.contains("nothing on the stick changed")), "{r:?}");
    kassert!(snapshot()? == marked, "a failed now changed the stick");
    fs_fat::stick::with_sys(|s| s.set_marker(made, Marker::Ok, false)).map_err(e)?.map_err(e)?;
    kassert!(snapshot()? == before, "the OK marker was not taken back");
    // 4. Off: PIN gone, the next start by the marker rule (the NEW generation's one try).
    let a = call("{\"n\":\"off\"}")?;
    kassert!(a.contains(&format!("PIN removed (it named generation {base})")) && a.contains(&format!("generation {made} (NEWEST_TRY")), "{a}");
    let (g4, p4, _) = snapshot()?;
    kassert!(p4.is_none(), "PIN still there: {p4:?}");
    kassert!(plan::next_boot(&g4, None).is_ok_and(|x| x.0 == made), "next boot {:?}", plan::next_boot(&g4, None));
    drop(restore);
    let (g5, p5, _) = snapshot()?;
    kassert!(g5 == gens && p5 == pin, "the stick was not put back: {gens:?} {pin:?} -> {g5:?} {p5:?}");
    Ok(())
}
ktest!("rb.install.system_pin", QemuOnly, rb_install_system_pin);
