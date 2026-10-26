//! RB-9: MCP tools `system_list` (Read) and `system_pin` (Change), and the console commands of the same names
//! plus `install_system` (the MCP tool `install_system` lives beside `install_kernel` in `tool`).
//!
//! `system_pin <n>` writes `\BITSOS2\SYS\PIN` ("system back to generation n": bitsboot's rule 1 at the next
//! start), `system_pin off` removes it (the newest confirmed generation starts again). With `now` the machine
//! also goes there at once: a Reload into the generation the next start would choose, its KERNEL and INITRD
//! read from the stick, the new kernel told its GENERATION record as bitsboot would hand it.

use super::system::{self, PinPlan, Want};
use super::system_stick;
use crate::stage::{self, StageError, Staged};
use crate::tool::{Busy, opt_bool};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use core::sync::atomic::{AtomicBool, Ordering};
use bootinfo::Generation;
use mcp::{Json, ToolContext, ToolError, ToolOutput, ToolResult, mcp_tool};

const SEC: u64 = 1_000_000_000;
/// How long `system_list` waits for the stick before it answers with what it has.
const LIST_WAIT_NS: u64 = 10 * SEC;

fn err_text(e: &ToolError) -> String {
    match e {
        ToolError::BadArgument(t) | ToolError::Refused(t) | ToolError::Failed(t) => t.clone(),
    }
}

fn offset(utc: i64) -> i32 {
    time::wall::tz().offset_at(utc)
}

/// `system_list`'s text, waiting at most `wait_ns` (bounded by the call's deadline).
pub fn list_text(wait_ns: u64) -> Result<String, String> {
    system_stick::listing(wait_ns).map(|l| system::text(&l, &offset))
}

fn system_list(ctx: &ToolContext, _args: &Json) -> ToolResult {
    let left = ctx.deadline_ns.saturating_sub(time::now_ns()).saturating_sub(SEC);
    list_text(left.clamp(SEC, LIST_WAIT_NS)).map(|t| vec![ToolOutput::Text(t)]).map_err(ToolError::Failed)
}

mcp_tool!(
    name: "system_list",
    owner: "RB",
    safety: Read,
    description: "The system generations on the boot stick (\\BITSOS2\\SYS\\<n>\\: kernel + initramfs with init and the early services): number, state (OK confirmed, NEW installed, TRY on trial), when installed and confirmed, kernel id (FNV-1a 64 of KERNEL, as install_kernel prints it), sizes; which one runs (and why), which one the next start takes (bitsboot's rule) and the pin.",
    schema: r#"{"type":"object","properties":{}}"#,
    handler: system_list,
);

/// What `pin_step` found: the answer's first line, the plan, and the Reload target (`now`).
pub struct PinStep {
    pub text: String,
    pub plan: PinPlan,
    pub target: Option<Generation>,
}

fn refuse(t: String) -> ToolError {
    klog::event!(Warn, "kexec", "system.pin_refused", reason = t.as_str(); "nothing on the stick changed");
    ToolError::Refused(format!("{t} (nothing on the stick changed)"))
}

/// How long a call may wait for the install lock: what is left of its deadline, at most a minute.
fn lock_wait(ctx: &ToolContext) -> u64 {
    ctx.remaining_ns().saturating_sub(SEC).clamp(SEC, 60 * SEC)
}

/// The PIN step of `system_pin` under the install lock (waiting at most `lock_ns` for it): plan, the `now`
/// checks, the write. When a Reload follows (`target` is Some) nothing is written here: the caller stages the
/// generation first and then writes with `commit_pin`, so a refused or failed `now` leaves the stick as it was.
pub fn pin_step(want: Want, now: bool, dry: bool, lock_ns: u64) -> Result<PinStep, ToolError> {
    // Right after a start the stick mounts a moment after MCP is up.
    let vol = fs_fat::stick::wait_mounted(5_000)
        .ok_or_else(|| refuse(format!("no boot stick mounted at /boot ({})", fs_fat::stick::last_reason().unwrap_or("none recognised"))))?;
    if !dry && !vol.writable() {
        return Err(refuse(format!("the boot stick is read-only ({})", fs_fat::stick::last_reason().unwrap_or("?"))));
    }
    if now && !dry {
        // NotBootCpu is no refusal: the jump moves to CPU 0 first.
        match hal::kexec_stub::check_cpus() {
            Ok(()) | Err(hal::kexec_stub::StubError::NotBootCpu { .. }) => {}
            Err(e) => return Err(refuse(format!("{e:?}: the other CPUs cannot be stopped for a Reload (without now it works)"))),
        }
    }
    let r = system_stick::with_gens(lock_ns, |s, gens, pin| {
        let p: PinPlan = match system::pin_plan(gens, pin, want) {
            Ok(p) => p,
            Err(t) => return Ok(Err(t)),
        };
        let target = if now {
            match system::now_target(&p, system_stick::running_number(s)) {
                Ok(t) => t,
                Err(t) => return Ok(Err(t)),
            }
        } else {
            None
        };
        if !dry && p.write && target.is_none() {
            s.set_pin(p.pin).map_err(|e| format!("writing \\BITSOS2\\SYS\\PIN: {}", super::errno_name(e)))?;
        }
        Ok(Ok((p, target)))
    });
    let (p, target) = match r {
        Ok(Ok(x)) => x,
        Ok(Err(t)) => return Err(refuse(t)),
        Err(t) => {
            klog::event!(Error, "kexec", "system.pin_failed", error = t.as_str());
            return Err(ToolError::Failed(t));
        }
    };
    let (rec, _, _) = &p.next;
    klog::event!(Info, "kexec", "system.pin", pin = p.pin.unwrap_or(0), was = p.was.unwrap_or(0), written = p.write && !dry && target.is_none(),
        dry_run = dry, next = rec.number, reason = bitsboot_pure::gens::reason_name(rec.reason), now = now, reload_into = target.map_or(0, |t| t.number));
    let mut t = system::pin_text(&p, dry);
    if !now {
        t.push_str(". It takes effect at the next start (or add now=true: a Reload into it at once)");
    } else if target.is_none() {
        t.push_str(&format!(". Generation {} runs already: no Reload needed", rec.number));
    }
    Ok(PinStep { text: t, plan: p, target })
}

/// Writes `plan`'s PIN once the Reload target is staged. The plan is made again under the lock and must come out
/// the same: `Busy` keeps installs and the boot confirmation off the stick meanwhile, so a difference means the
/// stick changed under us (then nothing is written).
pub fn commit_pin(want: Want, plan: &PinPlan, lock_ns: u64) -> Result<(), ToolError> {
    let r = system_stick::with_gens(lock_ns, |s, gens, pin| {
        if system::pin_plan(gens, pin, want).as_ref() != Ok(plan) {
            return Ok(Err(String::from("the stick changed while the generation was staged")));
        }
        if plan.write {
            s.set_pin(plan.pin).map_err(|e| format!("writing \\BITSOS2\\SYS\\PIN: {}", super::errno_name(e)))?;
        }
        Ok(Ok(()))
    });
    match r {
        Ok(Ok(())) => {
            klog::event!(Info, "kexec", "system.pin_written", pin = plan.pin.unwrap_or(0), written = plan.write);
            Ok(())
        }
        Ok(Err(t)) => Err(refuse(t)),
        Err(t) => {
            klog::event!(Error, "kexec", "system.pin_failed", error = t.as_str(); "no Reload");
            Err(ToolError::Failed(t))
        }
    }
}

/// Stages `target` from the stick for a Reload (the call's deadline bounds the reads).
fn stage_target(target: Generation, deadline_ns: u64) -> Result<Staged, String> {
    stage::stage_generation(target, deadline_ns, u64::MAX).map_err(|e| {
        klog::event!(Warn, "kexec", "system.reload_refused", generation = target.number, reason = e.text(); "nothing on the stick changed");
        match e {
            StageError::Refused(t) | StageError::Bad(t) | StageError::Fetch(t) | StageError::NotReady(t) => t,
        }
    })
}

fn prefixed(e: ToolError, pre: &str) -> ToolError {
    match e {
        ToolError::BadArgument(t) => ToolError::BadArgument(format!("{pre}{t}")),
        ToolError::Refused(t) => ToolError::Refused(format!("{pre}{t}")),
        ToolError::Failed(t) => ToolError::Failed(format!("{pre}{t}")),
    }
}

fn want_from(args: &Json) -> Result<Want, ToolError> {
    match args.get("n") {
        Some(Json::Str(s)) if s == "off" => Ok(Want::Off),
        Some(v) => match v.as_u64().filter(|n| (1..=super::plan::NUMBER_MAX as u64).contains(n)) {
            Some(n) => Ok(Want::Pin(n as u32)),
            None => Err(ToolError::BadArgument(String::from("n must be a generation number (1..99999999) or \"off\""))),
        },
        None => Err(ToolError::BadArgument(String::from("n is missing: a generation number, or \"off\""))),
    }
}

fn system_pin(ctx: &ToolContext, args: &Json) -> ToolResult {
    let want = want_from(args)?;
    let now = opt_bool(args, "now")?.unwrap_or(false);
    let dry = opt_bool(args, "dry_run")?.unwrap_or(false);
    let Some(busy) = Busy::take() else {
        return Err(ToolError::Refused(String::from(
            "a Reload, an install or the boot confirmation's stick write is in progress; try again in a few seconds (nothing on the stick changed)",
        )));
    };
    klog::event!(Info, "kexec", "system.pin_requested", n = match want { Want::Pin(n) => n, Want::Off => 0 }, now = now, dry_run = dry);
    let PinStep { text: mut t, plan, target } = pin_step(want, now, dry, lock_wait(ctx))?;
    let Some(target) = target else { return Ok(vec![ToolOutput::Text(t)]) };
    let st = stage_target(target, ctx.deadline_ns).map_err(|e| ToolError::Failed(format!("{t}. No Reload: {e} (nothing on the stick changed)")))?;
    let staged = crate::tool::describe(&st.summary);
    if dry {
        let cfg = crate::refresh::dry_run(&st);
        t.push_str(&format!(". Reload into generation {} (dry run, nothing happens): {staged}; {cfg}", target.number));
        return Ok(vec![ToolOutput::Text(t)]);
    }
    // The jump is armed only once the PIN is on the stick: a failed write leaves this kernel running.
    let armed = Arc::new(AtomicBool::new(false));
    let fire = armed.clone();
    if !ctx.after_answer(Box::new(move || {
        let _busy = busy;
        if fire.load(Ordering::Acquire) {
            crate::tool::go(st)
        } else {
            klog::event!(Info, "kexec", "system.reload_dropped", generation = target.number; "the PIN was not written: no Reload");
        }
    })) {
        return Err(ToolError::Failed(format!(
            "{t}. No Reload: a Reload happens only after its answer, and this call has none to wait for (nothing on the stick changed)"
        )));
    }
    commit_pin(want, &plan, lock_wait(ctx)).map_err(|e| prefixed(e, &format!("{t}. No Reload: ")))?;
    armed.store(true, Ordering::Release);
    let mut note = String::new();
    if let Some((g, covered, need)) = super::confirm::pending() {
        note = format!(" Note: this kernel is generation {g}'s one try and not confirmed yet ({covered} of {need} s): it stays unconfirmed.");
        klog::event!(Warn, "kexec", "reload.unconfirmed_generation", generation = g, covered_s = covered, need_s = need);
    }
    Ok(vec![ToolOutput::Text(format!(
        "{t}. Reloading into generation {} ({}) once this answer is out: {staged}. MCP answers again from it in seconds (QEMU) to ~15 s (Mini-PC).{note}",
        target.number,
        bitsboot_pure::gens::reason_name(target.reason)
    ))])
}

mcp_tool!(
    name: "system_pin",
    owner: "RB",
    safety: Change,
    description: "System back to an older generation (B1): n=<number> writes \\BITSOS2\\SYS\\PIN so bitsboot starts that confirmed generation at every start until n=\"off\" removes the pin (or an install with unpin). now=true also goes there at once: a Reload into the generation the next start would take (its KERNEL and INITRD read from the stick; refused for a generation still on trial). dry_run: check and plan only (with now: also stage the Reload).",
    schema: r#"{"type":"object","properties":{"n":{"description":"generation number, or \"off\"","oneOf":[{"type":"integer","minimum":1},{"type":"string","enum":["off"]}]},"now":{"type":"boolean","description":"Reload into it at once"},"dry_run":{"type":"boolean"},"timeout_ms":{"type":"integer","default":120000}},"required":["n"]}"#,
    handler: system_pin,
);

// ---- console --------------------------------------------------------------------------------------------

fn console_system_list(c: &mut console::monitor::Ctx) -> Result<(), String> {
    if !c.args.is_empty() {
        return Err(String::from("usage: system_list"));
    }
    for line in list_text(LIST_WAIT_NS)?.lines() {
        c.println(line);
    }
    Ok(())
}
console::command!(name: "system_list", args: "", help: "system generations on the boot stick: state, dates, kernel id, running, next start", physical_only: false, run: console_system_list);

/// `<n>|off [now]` (pure; host-tested).
pub fn parse_pin_args(args: &[&str]) -> Result<(Want, bool), String> {
    let usage = || String::from("usage: system_pin <n>|off [now]");
    let want = match args.first().copied() {
        Some("off") => Want::Off,
        Some(n) => Want::Pin(n.parse::<u32>().ok().filter(|n| (1..=super::plan::NUMBER_MAX).contains(n)).ok_or_else(usage)?),
        None => return Err(usage()),
    };
    match args.get(1..).unwrap_or(&[]) {
        [] => Ok((want, false)),
        ["now"] => Ok((want, true)),
        _ => Err(usage()),
    }
}

fn console_system_pin(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let (want, now) = parse_pin_args(&c.args)?;
    let Some(busy) = Busy::take() else { return Err(String::from("a Reload or an install is in progress; try again in a few seconds")) };
    let PinStep { text, plan, target } = pin_step(want, now, false, 60 * SEC).map_err(|e| err_text(&e))?;
    let Some(target) = target else {
        c.println(&text);
        return Ok(());
    };
    c.println(&format!("plan: {text}"));
    c.println(&format!(
        "staging generation {} from the stick; the PIN is written once that worked, then the Reload (the log says if it is refused: then nothing on the stick changed)",
        target.number
    ));
    // The stick reads take seconds: not on the console's thread.
    let started = sched::spawn("system-pin-now", sched::Class::Normal, move || {
        let _busy = busy;
        let st = match stage_target(target, time::now_ns().saturating_add(120 * SEC)) {
            Ok(st) => st,
            Err(e) => return klog::event!(Warn, "kexec", "system.reload_failed", generation = target.number, error = e.as_str(); "nothing on the stick changed"),
        };
        match commit_pin(want, &plan, 60 * SEC) {
            Ok(()) => crate::tool::go(st),
            Err(e) => klog::event!(Warn, "kexec", "system.reload_failed", generation = target.number, error = err_text(&e).as_str(); "no Reload"),
        }
    });
    started.map(|_| ()).map_err(|_| String::from("no thread for the Reload (no memory now); nothing on the stick changed"))
}
console::command!(name: "system_pin", args: "<n>|off [now]", help: "system back to generation n (or off: the newest); now = Reload into it at once", physical_only: true, run: console_system_pin);

fn console_install_system(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let [url] = c.args[..] else { return Err(String::from("usage: install_system <http://LAN-address/dir/>")) };
    if url.contains(['"', '\\']) {
        return Err(String::from("the URL must not contain quotes or backslashes"));
    }
    let args = Json::parse(&format!("{{\"url\":\"{url}\"}}")).map_err(|_| String::from("bad URL"))?;
    let ctx = ToolContext::detached(time::now_ns().saturating_add(300 * SEC));
    let out = super::tool::install_with(&ctx, &args, true).map_err(|e| err_text(&e))?;
    for o in out {
        if let ToolOutput::Text(t) = o {
            for line in t.lines() {
                c.println(line);
            }
        }
    }
    Ok(())
}
console::command!(name: "install_system", args: "<url>", help: "install kernel + initramfs (with init and the early services) from the LAN as a new system generation", physical_only: true, run: console_install_system);
