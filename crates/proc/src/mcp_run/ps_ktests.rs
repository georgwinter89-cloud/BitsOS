//! MC-5b ktests (prefix `mc.ps.`): `ps`, `kill` and `output`/`stop` by pid over a loopback MCP server of the
//! test's own (port 25060 or the next free one), with `progs`' tiny programs. Safe and rerunnable: every
//! program a test starts is ended (and waited for) before it returns.

use super::progs;
use super::ktests::{b64, needs_two_cpus, pid_in, result_of};
use crate::Process;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use mcp::testclient::{Client, call_number, with_server};
use vfs::File;

const PORT: u16 = 25060;
const SEC: u64 = 1_000_000_000;

/// The `ps` row of `pid` (the line whose first column is it).
fn row_of(text: &str, pid: u32) -> Option<String> {
    text.lines().find(|l| l.split_whitespace().next() == Some(&*format!("{pid}"))).map(String::from)
}

/// Column `i` of a ps row (0 = PID, 5 = S, 6 = THR, 8 = TIME, 9 = USER).
fn col(row: &str, i: usize) -> &str {
    row.split_whitespace().nth(i).unwrap_or("")
}

fn ps_state(c: &mut Client, pid: u32) -> Result<String, String> {
    let t = c.ok("ps", &format!(r#"{{"pid":{pid}}}"#))?;
    Ok(String::from(col(&row_of(&t, pid).ok_or_else(|| format!("no row for {pid}: {t}"))?, 5)))
}

/// A program no run call started (as a console job), its stdout into a sink; a child of `parent` if given.
fn background(name: &str, code: &[u8], parent: Option<&Arc<Process>>) -> Result<Arc<Process>, String> {
    let image = progs::elf(code);
    let argv = [name];
    let mut o = crate::SpawnOptions::new(&argv, &[], super::MCP_PROGRAM);
    o.parent = parent;
    o.stdout = Some(super::Stream::new(4096) as Arc<dyn File>);
    crate::spawn_image_with(&format!("/t/{name}"), &image, &o).map_err(|e| format!("spawn: {e}"))
}

/// Ends `p` whatever a failed check left behind (rerunnable).
fn end(p: &Process) {
    p.kill(uapi::ab::signal::SIGKILL);
    let _ = p.wait(Some(5 * SEC));
}

/// A run job in ps (state, threads, command line, its thread line), found by pid through `output`, stopped and
/// continued by `kill`, then `stop {"pid"}`: SIGINT ends it (no handler), the result names it.
fn job_ps_kill_stop_by_pid() -> TestResult {
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"ps-sleeploop","args":["a b"],"timeout_ms":300}}"#, b64(progs::SLEEPLOOP)))?;
        kassert!(r.error && r.text.starts_with("Still running"), "{}", r.text);
        let n = call_number(&r.text)?;
        let pid = pid_in(&r.text)?;
        let all = c.ok("ps", "{}")?;
        kassert!(all.starts_with("processes ") && all.contains("committed ") && all.contains(" COMMIT_KB COMMAND"), "{all}");
        let row = row_of(&all, pid).ok_or_else(|| format!("no row for {pid}: {all}"))?;
        kassert!(matches!(col(&row, 5), "S" | "R") && col(&row, 6) == "1" && row.ends_with("ps-sleeploop a b"), "{row}");
        let one = c.ok("ps", &format!(r#"{{"pid":{pid}}}"#))?;
        kassert!(one.contains(&format!("tid={pid} state=")) && one.contains(" class="), "{one}");
        let o = c.ok("output", &format!(r#"{{"pid":{pid}}}"#))?;
        kassert!(o.starts_with(&format!("call {n} run: running after ")), "{o}");
        let k = c.ok("kill", &format!(r#"{{"pid":{pid},"signal":"SIGSTOP"}}"#))?;
        kassert!(k.contains("sent SIGSTOP") && k.ends_with("now stopped (T)"), "{k}");
        kassert!(ps_state(c, pid)? == "T", "stopped");
        let k = c.ok("kill", &format!(r#"{{"pid":{pid},"signal":18}}"#))?;
        kassert!(k.contains("sent SIGCONT") && k.ends_with("now running"), "{k}");
        let s = c.call("stop", &format!(r#"{{"pid":{pid},"grace_ms":2000}}"#))?;
        let text = if s.text.contains("still running") { result_of(c, n, 10 * SEC)? } else { s.text };
        kassert!(text.contains("ended: signal 2 (SIGINT), code 130\nstopped by the stop tool (SIGINT)"), "{text}");
        let p = crate::find(pid);
        kassert!(p.is_none_or(|p| p.wait(Some(5 * SEC)).is_some()), "its thread ended");
        Ok(())
    })
}
ktest!("mc.ps.job_ps_kill_stop_by_pid", Safe, job_ps_kill_stop_by_pid);

/// A program that never leaves user mode: R with CPU time in ps, and `kill` (SIGTERM by default) ends it; its
/// run call then hands over the SIGTERM end.
fn spinner_cpu_time_and_kill() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"ps-spin","timeout_ms":300}}"#, b64(progs::SPIN)))?;
        let n = call_number(&r.text)?;
        let pid = pid_in(&r.text)?;
        sched::sleep_ns(SEC / 5);
        let t = c.ok("ps", &format!(r#"{{"pid":{pid}}}"#))?;
        let row = row_of(&t, pid).ok_or_else(|| format!("no row: {t}"))?;
        let user: f64 = col(&row, 9).parse().unwrap_or(0.0);
        kassert!(col(&row, 5) == "R" && user > 0.1, "{row}");
        let k = c.ok("kill", &format!(r#"{{"pid":{pid}}}"#))?;
        kassert!(k.contains("sent SIGTERM: its default action ends it; now ended: signal 15 (SIGTERM)"), "{k}");
        let res = result_of(c, n, 10 * SEC)?;
        kassert!(res.contains("ended: signal 15 (SIGTERM), code 143"), "{res}");
        Ok(())
    })
}
ktest!("mc.ps.spinner_cpu_time_and_kill", Safe, spinner_cpu_time_and_kill);

/// What is refused or wrong: PID 1 without confirm (kill and stop; signal 0 may look), a kernel thread's id,
/// a pid that is not there, a bad signal, pid and pgid together.
fn refusals() -> TestResult {
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        if crate::find(1).is_some_and(|p| !p.is_finished()) {
            let k = c.call("kill", r#"{"pid":1,"signal":"SIGTERM"}"#)?;
            kassert!(k.error && k.text.contains("PID 1 (init) gets SIGTERM only with confirm: true"), "{}", k.text);
            let k = c.call("kill", r#"{"pid":1,"signal":"KILL"}"#)?;
            kassert!(k.error && k.text.contains("only with confirm"), "{}", k.text);
            let s = c.call("stop", r#"{"pid":1}"#)?;
            kassert!(s.error && s.text.contains("only with confirm"), "{}", s.text);
            let k = c.ok("kill", r#"{"pid":1,"signal":0}"#)?;
            kassert!(k.starts_with("pid 1 (") && k.contains("exists"), "{k}");
            kassert!(crate::find(1).is_some_and(|p| p.status().is_none()), "init lives on");
        }
        let me = sched::current_id();
        if me <= u64::from(u32::MAX) && crate::find(me as u32).is_none() && crate::find_thread(me as u32).is_none() {
            // A kernel thread has no pid (sched's ids are another number space): its id names no process.
            let k = c.call("kill", &format!(r#"{{"pid":{me},"signal":9}}"#))?;
            kassert!(k.error && k.text.contains(&format!("no process {me} (ESRCH)")), "{}", k.text);
            let s = c.call("stop", &format!(r#"{{"pid":{me}}}"#))?;
            kassert!(s.error && s.text.contains("(ESRCH)"), "{}", s.text);
        }
        let k = c.call("kill", r#"{"pid":4000000000}"#)?;
        kassert!(k.error && k.text.contains("no process 4000000000 (ESRCH)"), "{}", k.text);
        for bad in [r#"{"pid":2,"signal":65}"#, r#"{"pid":2,"signal":"SIGNOPE"}"#, r#"{"pid":2,"pgid":2}"#, r#"{}"#, r#"{"pid":2,"wait_ms":10001}"#] {
            let k = c.call("kill", bad)?;
            kassert!(k.rpc_error == Some(-32602), "{bad}: {:?} {}", k.rpc_error, k.text);
        }
        let p = c.call("ps", r#"{"pid":4000000000}"#)?;
        kassert!(p.error && p.text.contains("ESRCH"), "{}", p.text);
        Ok(())
    })
}
ktest!("mc.ps.refusals", Safe, refusals);

/// Sets `sig`'s action in `p` to SIG_IGN (as the program's own sigaction would).
fn ignore(p: &Process, sig: u32) -> Result<(), String> {
    let a = crate::signal::SigAction { handler: uapi::ab::sigabi::SIG_IGN, ..Default::default() };
    p.signals.raw(|s| s.sigaction(sig, Some(a))).map(|_| ()).map_err(|e| format!("sigaction {sig}: {e:?}"))
}

/// Programs no run call started (console jobs): `output {"pid"}` describes one; `stop {"pid"}` of one that
/// ignores SIGINT sends SIGKILL after the grace; `stop {"pid"}` of a stopped job continues it so SIGINT ends it;
/// `stop {"pgid"}` (default grace) ends a whole process group with SIGINT; `kill` of a zombie (its parent does
/// not wait) sends nothing and ps shows it as Z.
fn console_style_jobs() -> TestResult {
    let parent = background("ps-bg-parent", progs::SLEEPLOOP, None)?;
    let a = background("ps-bg-a", progs::SLEEPLOOP, Some(&parent))?;
    let b = background("ps-bg-b", progs::SLEEPLOOP, Some(&parent))?;
    let d = background("ps-bg-c", progs::SLEEPLOOP, None)?;
    let e = background("ps-bg-e", progs::SLEEPLOOP, None)?;
    b.set_pgid(a.pid);
    a.set_pgid(a.pid);
    let r = ignore(&d, uapi::ab::signal::SIGINT).and_then(|()| with_server(PORT, mcp::TOOLS.entries(), |c| {
        let o = c.ok("output", &format!(r#"{{"pid":{}}}"#, a.pid))?;
        kassert!(o.contains("was not started by a run call") && o.contains(&format!("tid={} state=", a.pid)), "{o}");
        let t0 = time::now_ns();
        let s = c.ok("stop", &format!(r#"{{"pid":{},"grace_ms":300}}"#, d.pid))?;
        kassert!(s.contains("stopped by the stop tool (SIGINT, then SIGKILL after the grace); ended: signal 9 (SIGKILL)"), "{s}");
        kassert!(time::now_ns() - t0 >= 300_000_000, "SIGKILL only after the grace");
        let k = c.ok("kill", &format!(r#"{{"pid":{},"signal":"STOP"}}"#, e.pid))?;
        kassert!(k.ends_with("now stopped (T)"), "{k}");
        let s = c.ok("stop", &format!(r#"{{"pid":{}}}"#, e.pid))?;
        kassert!(s.contains("stopped by the stop tool (SIGINT); ended: signal 2 (SIGINT)"), "{s}");
        let s = c.ok("stop", &format!(r#"{{"pgid":{}}}"#, a.pid))?;
        kassert!(s.matches("stopped by the stop tool (SIGINT); ended: signal 2 (SIGINT)").count() == 2, "{s}");
        // Zombies until reaped (their parent never waits): kill sends nothing, ps shows Z.
        let k = c.ok("kill", &format!(r#"{{"pid":{}}}"#, a.pid))?;
        kassert!(k.contains("already ended") && k.contains("nothing sent"), "{k}");
        kassert!(ps_state(c, a.pid)? == "Z", "a zombie");
        let g = c.call("stop", &format!(r#"{{"pgid":{}}}"#, a.pid))?;
        kassert!(g.error && g.text.contains("no live process in process group"), "{}", g.text);
        Ok(())
    }));
    for p in [&a, &b, &d, &e, &parent] {
        end(p);
    }
    r
}
ktest!("mc.ps.console_style_jobs", Safe, console_style_jobs);

/// How `kill` reports a signal that does not act at once: blocked (pending, `now running`), a blocked SIGTSTP
/// answers without waiting `wait_ms` for a stop that cannot come, a real-time signal by name, an ignored one.
fn kill_outcomes() -> TestResult {
    use crate::signal::bit;
    use uapi::ab::signal::{SIGTERM, SIGTSTP, SIGUSR1};
    let p = background("ps-outcomes", progs::SLEEPLOOP, None)?;
    let r = ignore(&p, SIGUSR1).and_then(|()| {
        p.signals.raw(|s| {
            for k in s.thread_keys() {
                let _ = s.set_blocked(k, bit(SIGTERM) | bit(SIGTSTP) | bit(35));
            }
        });
        with_server(PORT, mcp::TOOLS.entries(), |c| {
            let k = c.ok("kill", &format!(r#"{{"pid":{},"signal":"TERM"}}"#, p.pid))?;
            kassert!(k.contains("sent SIGTERM: pending") && k.ends_with("now running"), "{k}");
            let t0 = time::now_ns();
            let k = c.ok("kill", &format!(r#"{{"pid":{},"signal":"SIGTSTP","wait_ms":5000}}"#, p.pid))?;
            kassert!(k.contains("sent SIGTSTP") && k.ends_with("now running"), "{k}");
            kassert!(time::now_ns() - t0 < 2 * SEC, "kill waited for a stop a blocked SIGTSTP cannot cause");
            let k = c.ok("kill", &format!(r#"{{"pid":{},"signal":"SIGRTMIN+1"}}"#, p.pid))?;
            kassert!(k.contains("sent SIGRTMIN+1: pending"), "{k}");
            let k = c.ok("kill", &format!(r#"{{"pid":{},"signal":"usr1"}}"#, p.pid))?;
            kassert!(k.contains("sent SIGUSR1: ignored"), "{k}");
            kassert!(matches!(&*ps_state(c, p.pid)?, "S" | "R"), "still alive");
            Ok(())
        })
    });
    end(&p);
    r
}
ktest!("mc.ps.kill_outcomes", Safe, kill_outcomes);

/// A program with four threads (AB-4's `thread_maker`, all spinning): `ps {"threads","kernel"}` lists its
/// threads and the kernel threads; `kill {"pid": <a thread id>}` names the whole process (kill(2)) and ends it.
fn threads_kill_by_tid() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    let code = crate::ktests::need(hal::entry::ab4_progs::thread_maker())?;
    let img = crate::ktests::image(code, true);
    let (p, _) = crate::ktests::start("/mc/ps-threads", &img, &["t"], None, false)?;
    let r = (|| -> TestResult {
        let t0 = time::now_ns();
        while p.thread_count() < 4 && time::now_ns() - t0 < 5 * SEC {
            sched::sleep_ns(SEC / 50);
        }
        kassert!(p.thread_count() == 4, "4 threads expected, {}", p.thread_count());
        let other = p.threads().iter().map(|t| t.tid()).find(|&t| t != p.pid).ok_or("no second thread")?;
        with_server(PORT, mcp::TOOLS.entries(), |c| {
            let t = c.ok("ps", r#"{"threads":true,"kernel":true}"#)?;
            for tid in p.threads().iter().map(|t| t.tid()) {
                kassert!(t.contains(&format!("tid={tid} state=")), "thread {tid} missing: {t}");
            }
            kassert!(t.contains("kernel threads (never signalled):\n") && t.contains("kthread="), "{t}");
            let one = c.ok("ps", &format!(r#"{{"pid":{other}}}"#))?;
            kassert!(row_of(&one, p.pid).is_some_and(|r| col(&r, 6) == "4"), "a thread id names its process: {one}");
            let k = c.ok("kill", &format!(r#"{{"pid":{other},"signal":"KILL","wait_ms":5000}}"#))?;
            kassert!(k.starts_with(&format!("pid {} (", p.pid)) && k.contains("ended: signal 9 (SIGKILL)"), "{k}");
            Ok(())
        })
    })();
    end(&p);
    r
}
ktest!("mc.ps.threads_kill_by_tid", Safe, threads_kill_by_tid);
