//! MC-5a ktests: `run` over a loopback MCP server of the test's own with the tiny programs of `progs`, sent
//! along as `elf_b64` (nothing is written to the file tree): the deadline answer with the output so far, partial
//! output through `output` (only new bytes, adjacent ranges), `stop` killing the program, a program that never
//! leaves user mode, separate stderr, the fault message, stdin as base64, a stream above the cap, a finished
//! job's result through `output` and clean errors for gone or unknown jobs, a stopping server ending its jobs, and
//! `retry_kill`. All rerunnable (no statics; `with_server` moves to the next port if this one is still taken).

use super::progs;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};
use mcp::testclient::{Client, call_number, with_server};

const PORT: u16 = 25060;
const SEC: u64 = 1_000_000_000;

pub(super) fn b64(code: &[u8]) -> String {
    let mut s = String::new();
    mcp::b64::encode_into(&progs::elf(code), &mut s);
    s
}

/// The pid in "pid <n> ..." anywhere in `text`.
pub(super) fn pid_in(text: &str) -> Result<u32, String> {
    text.split("pid ").nth(1).and_then(|s| s.split(' ').next()).and_then(|s| s.parse().ok()).ok_or_else(|| format!("no pid in {text}"))
}

/// Every "--- stdout bytes a..b" range in `text`, in order.
fn stdout_ranges(text: &str) -> Vec<(u64, u64)> {
    text.split("--- stdout bytes ")
        .skip(1)
        .filter_map(|s| {
            let r = s.split(' ').next()?;
            let (a, b) = r.split_once("..")?;
            Some((a.parse().ok()?, b.parse().ok()?))
        })
        .collect()
}

/// `output` of call `n` until its result came (bounded).
pub(super) fn result_of(c: &mut Client, n: u64, within_ns: u64) -> Result<String, String> {
    let end = time::now_ns() + within_ns;
    loop {
        let o = c.call("output", &format!(r#"{{"call":{n}}}"#))?;
        if o.text.starts_with(&format!("call {n} run: finished")) {
            return Ok(o.text);
        }
        kassert!(time::now_ns() < end, "call {n} did not finish: {}", o.text);
        sched::sleep_ns(SEC / 10);
    }
}

/// `output` and `stop` of a call that is gone (result handed over) or never was: a clean error naming it.
fn gone(c: &mut Client, n: u64) -> TestResult {
    for tool in ["output", "stop"] {
        let r = c.call(tool, &format!(r#"{{"call":{n}}}"#))?;
        kassert!(r.error && r.text.contains(&format!("no call {n} here")), "{tool} of call {n}: {}", r.text);
    }
    Ok(())
}

/// Until CP-5's timer a CPU busy with a program that never makes a syscall is interrupted only by another CPU
/// (the poll-duty CPU fires its tick, `nudge` preempts it); with one CPU nobody would.
pub(super) fn needs_two_cpus() -> Option<TestResult> {
    (sched::online_cpus() < 2).then(|| ktest::skip("a program that never makes a syscall needs a second CPU until CP-5's timer"))
}

/// Pins this test's thread to CPU 0 (as the MCP `ktest` tool runs every test) until dropped, then gives it its
/// previous pin back.
struct OnCpu0(Option<usize>);

impl OnCpu0 {
    fn new() -> OnCpu0 {
        let before = sched::thread_stats().into_iter().find(|t| t.id == sched::current_id()).and_then(|t| t.pinned);
        sched::pin_current(Some(0));
        OnCpu0(before)
    }
}

impl Drop for OnCpu0 {
    fn drop(&mut self) {
        sched::pin_current(self.0);
    }
}

/// A program that never makes a syscall: answered at the deadline with its pid, keeps running, `stop` kills it.
/// The client waits pinned to CPU 0, as on the Mini-PC: when the program lands there, the client's wake-up
/// still gets CPU 0 back (the tick armed for the second runnable thread, fired by the poll-duty CPU's IPI).
fn spin_deadline_then_stop() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    let _cpu0 = OnCpu0::new();
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let t0 = time::now_ns();
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"spin","timeout_ms":400}}"#, b64(progs::SPIN)))?;
        let ms = (time::now_ns() - t0) / 1_000_000;
        kassert!(r.error && r.text.starts_with("Still running: run (call ") && (350..10_000).contains(&ms), "{ms} ms: {}", r.text);
        kassert!(r.text.contains("--- progress so far ---\npid ") && r.text.contains("/bin/spin is running (job = call "), "{}", r.text);
        let n = call_number(&r.text)?;
        let pid = pid_in(&r.text)?;
        sched::sleep_ns(SEC / 5);
        let p = crate::find(pid).ok_or("the process is gone after the deadline")?;
        kassert!(p.status().is_none(), "it keeps running after the answer");
        let cpu = sched::thread_stats().into_iter().find(|t| t.id == p.thread_id()).map(|t| t.cpu);
        klog::event!(Info, "proc", "test.spin_cpu", pid = pid, cpu = cpu.map_or(-1, |c| c as i64), client_cpu = sched::current_cpu());
        kassert!(super::running() >= 1, "the job is counted");
        let o = c.ok("output", &format!(r#"{{"call":{n}}}"#))?;
        kassert!(o.starts_with(&format!("call {n} run: running after ")), "{o}");
        let s = c.call("stop", &format!(r#"{{"call":{n}}}"#))?;
        let text = if s.text.contains("still running") { result_of(c, n, 10 * SEC)? } else { s.text };
        kassert!(text.contains("ended: signal 9 (SIGKILL), code 137\nstopped by the stop tool"), "{text}");
        kassert!(p.wait(Some(5 * SEC)).is_some(), "its thread ended");
        gone(c, n)
    })
}
ktest!("mc.run.spin_deadline_then_stop", Safe, spin_deadline_then_stop);

/// The same with every other CPU busy (a kernel thread pinned to each, with preemption points), so the program
/// lands next to the client on CPU 0 (seen: `test.spin_cpu cpu=0` with `-smp 2`): the client's wake-up still
/// gets CPU 0 back and `stop` still ends the program. QemuOnly: it keeps every CPU busy for about a second.
fn spin_beside_the_client_on_cpu0() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    let stop = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let mut hogs = Vec::new();
    for cpu in 1..sched::online_cpus() {
        let st = stop.clone();
        let opts = sched::SpawnOpts { cpu: Some(cpu), pin: true, ..Default::default() };
        let h = sched::spawn_with("mc-test-hog", sched::Class::Normal, opts, move || {
            let end = time::now_ns() + 30 * SEC;
            while !st.load(core::sync::atomic::Ordering::Relaxed) && time::now_ns() < end {
                sched::cond_resched();
                core::hint::spin_loop();
            }
        });
        if let Ok(h) = h {
            hogs.push(h);
        }
    }
    sched::sleep_ns(SEC / 10);
    let r = spin_deadline_then_stop();
    stop.store(true, core::sync::atomic::Ordering::Relaxed);
    let joined = hogs.iter().all(|h| h.join_timeout(5 * SEC));
    r?;
    kassert!(joined, "a hog thread did not end");
    Ok(())
}
ktest!("mc.run.spin_beside_the_client_on_cpu0", QemuOnly, spin_beside_the_client_on_cpu0);

/// "tick" every 100 ms: the deadline answer has the first ticks, each `output` only the next ones (the byte
/// ranges follow each other without a gap), the result after `stop` all of them.
fn sleeploop_partial_output() -> TestResult {
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"/t/sleeploop","timeout_ms":650}}"#, b64(progs::SLEEPLOOP)))?;
        kassert!(r.error && r.text.starts_with("Still running"), "{}", r.text);
        kassert!(r.text.contains("--- stdout bytes 0..") && r.text.contains("tick\ntick\n"), "the output so far: {}", r.text);
        let n = call_number(&r.text)?;
        let mut seen = stdout_ranges(&r.text);
        for _ in 0..2 {
            sched::sleep_ns(SEC * 4 / 10);
            let o = c.ok("output", &format!(r#"{{"call":{n}}}"#))?;
            let next = stdout_ranges(&o);
            kassert!(!next.is_empty() && o.contains("tick\n") && !o.contains("--- stdout bytes 0.."), "only new output: {o}");
            kassert!(!o.contains("before not shown") && !o.contains("stderr"), "no gap, nothing on stderr: {o}");
            let (end, from) = (seen.last().map_or(0, |r| r.1), next[0].0);
            kassert!(from == end, "new output starts where the last ended ({end}), not at {from}: {o}");
            kassert!(next.iter().all(|r| (r.1 - r.0) % 5 == 0), "whole ticks: {next:?}");
            seen.extend(next);
        }
        let s = c.call("stop", &format!(r#"{{"call":{n}}}"#))?;
        let text = if s.text.contains("still running") { result_of(c, n, 10 * SEC)? } else { s.text };
        let ticks = text.split("--- stdout (").nth(1).map(|t| t.matches("tick\n").count()).unwrap_or(0);
        kassert!(text.contains("signal 9 (SIGKILL)") && ticks >= 10, "{ticks} ticks: {text}");
        Ok(())
    })
}
ktest!("mc.run.sleeploop_partial_output", Safe, sleeploop_partial_output);

/// A job that ends by itself after its deadline: `output` hands over the whole result once, then the call is
/// gone; an unknown call number is a clean error too.
fn ended_job_result_then_gone() -> TestResult {
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"ends-later","timeout_ms":300,"kill_after_ms":900}}"#, b64(progs::SLEEPLOOP)))?;
        kassert!(r.error && r.text.starts_with("Still running"), "{}", r.text);
        let n = call_number(&r.text)?;
        let t = result_of(c, n, 10 * SEC)?;
        let ticks = t.split("--- stdout (").nth(1).map(|t| t.matches("tick\n").count()).unwrap_or(0);
        kassert!(t.contains("ended: signal 9 (SIGKILL), code 137\nkilled after kill_after_ms=900") && ticks >= 6, "{ticks} ticks: {t}");
        kassert!(t.contains("--- stderr (0 bytes) ---"), "{t}");
        gone(c, n)?;
        gone(c, 4_000_000_000)
    })
}
ktest!("mc.run.ended_job_result_then_gone", Safe, ended_job_result_then_gone);

/// stdout and stderr apart, the exit code, the fault message, stdin as bytes, a stream above the cap.
fn streams_exit_fault_stdin() -> TestResult {
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let t = c.ok("run", &format!(r#"{{"elf_b64":"{}","path":"two-streams"}}"#, b64(progs::STDERR)))?;
        kassert!(t.contains(" ended: exit 7\n--- stdout (10 bytes) ---\nto stdout\n--- stderr (10 bytes) ---\nto stderr\n"), "{t}");
        let t = c.ok("run", &format!(r#"{{"elf_b64":"{}","path":"faulty"}}"#, b64(progs::FAULT)))?;
        kassert!(t.contains("ended: signal 11 (SIGSEGV), code 139\nProgram ") && t.contains("Page fault at 0x0") && t.contains("--- stdout (17 bytes) ---\nbefore the fault\n"), "{t}");
        // "hi", a NUL, 0xff, "there": bytes through stdin_b64 come back unchanged (0xff shown as U+FFFD).
        let t = c.ok("run", &format!(r#"{{"elf_b64":"{}","path":"echo","stdin_b64":"aGkA/3RoZXJl"}}"#, b64(progs::ECHO)))?;
        kassert!(t.contains("ended: exit 0\n--- stdout (9 bytes) ---\nhi\0\u{fffd}there\n--- stderr (0 bytes) ---"), "{t:?}");
        let r = c.call("run", r#"{"elf_b64":"AAAA"}"#)?;
        kassert!(r.error && r.text.contains("/mcp/inline"), "not an ELF: {}", r.text);
        // 1.6 MB on stdout: the first 1 MiB in the answer, the rest counted.
        let t = c.ok("run", &format!(r#"{{"elf_b64":"{}","path":"flood"}}"#, b64(progs::FLOOD)))?;
        let dropped = progs::FLOOD_BYTES - super::OUTPUT_CAP as u64;
        let head = format!("ended: exit 0\n--- stdout ({} bytes, last {dropped} not kept) ---\n", progs::FLOOD_BYTES);
        kassert!(t.contains(&head) && t.contains("--- stderr (0 bytes) ---"), "{}", &t[..t.len().min(300)]);
        let xs = t.split(head.as_str()).nth(1).map(|s| s.bytes().take_while(|b| *b == b'x').count()).unwrap_or(0);
        kassert!(xs == super::OUTPUT_CAP, "{xs} bytes of output in the answer");
        Ok(())
    })
}
ktest!("mc.run.streams_exit_fault_stdin", Safe, streams_exit_fault_stdin);

/// A stopping server (Reload, reboot) asks its calls to stop: a job still running is killed, not left behind.
fn server_stop_ends_jobs() -> TestResult {
    let mut job = None;
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        let r = c.call("run", &format!(r#"{{"elf_b64":"{}","path":"left-behind","timeout_ms":300}}"#, b64(progs::SLEEPLOOP)))?;
        kassert!(r.error && r.text.starts_with("Still running"), "{}", r.text);
        job = crate::find(pid_in(&r.text)?);
        kassert!(job.is_some(), "the job is not in the process table");
        Ok(())
    })?;
    let p = job.ok_or("no job")?;
    kassert!(p.wait(Some(5 * SEC)).is_some(), "the job outlived its server");
    kassert!(p.status() == Some(crate::status::ExitStatus::Signaled(9)), "{:?}", p.status());
    Ok(())
}
ktest!("mc.run.server_stop_ends_jobs", Safe, server_stop_ends_jobs);

/// What the tool hands over when its 5 s are up: `retry_kill` alone (no nudge before it) ends a killed program
/// that never leaves user mode, and gives its job count back.
fn retry_kill_ends_a_spinner() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    let image = progs::elf(progs::SPIN);
    let o = crate::SpawnOptions::new(&["spin"], &[], super::MCP_PROGRAM);
    let p = crate::spawn_image_with("/t/spin-retry", &image, &o).map_err(|e| format!("spawn: {e}"))?;
    let before = super::running();
    // Into user mode first: a kill before its first instruction would end it without any nudge.
    sched::sleep_ns(SEC / 5);
    kassert!(p.status().is_none(), "the spinner ended by itself: {:?}", p.status());
    p.kill(uapi::ab::signal::SIGKILL);
    kassert!(super::retry_kill(p.clone(), super::Running::new()).is_none(), "no retry thread");
    kassert!(p.wait(Some(10 * SEC)).is_some(), "the retry thread's nudges did not end it");
    let end = time::now_ns() + 2 * SEC;
    while super::running() > before && time::now_ns() < end {
        sched::sleep_ns(SEC / 50);
    }
    kassert!(super::running() <= before, "the retry thread still counts the job: {} > {before}", super::running());
    Ok(())
}
ktest!("mc.run.retry_kill_ends_a_spinner", Safe, retry_kill_ends_a_spinner);

/// Hostile `elf_b64`/`stdin_b64` through the real server and loader: invalid base64, empty, not an ELF, an ELF
/// cut short, garbage behind a valid header, a body above MC-1's 256 KiB request limit. Each is a clean error
/// (no panic), no job is left counted, and the server still answers afterwards.
fn bad_elf_b64() -> TestResult {
    let before = super::running();
    let procs_before = crate::processes().len();
    with_server(PORT, mcp::TOOLS.entries(), |c| {
        for (what, args) in [
            ("invalid base64", String::from(r#"{"elf_b64":"%%%%"}"#)),
            ("empty", String::from(r#"{"elf_b64":""}"#)),
            ("invalid stdin_b64", format!(r#"{{"elf_b64":"{}","stdin_b64":"not base64!"}}"#, b64(progs::ECHO))),
        ] {
            let r = c.call("run", &args)?;
            kassert!(r.rpc_error == Some(-32602) && r.text.is_empty(), "{what}: {:?} {}", r.rpc_error, r.text);
        }
        let image = progs::elf(progs::SPIN);
        let mut garbage = image[..64].to_vec();
        garbage.extend(core::iter::repeat_n(0xa5u8, 4096));
        for (what, bytes) in [("not an ELF", alloc::vec![0x41u8; 300]), ("an ELF cut short", image[..80].to_vec()), ("garbage behind the header", garbage)] {
            let mut b = String::new();
            mcp::b64::encode_into(&bytes, &mut b);
            let r = c.call("run", &format!(r#"{{"elf_b64":"{b}","path":"bad"}}"#))?;
            kassert!(r.error && r.text.starts_with("ERROR: /bin/bad: "), "{what}: {}", r.text);
        }
        // Above the request limit: refused by HTTP before any tool runs (a connection of its own: the server
        // may close it while the body is still being sent).
        let mut big = c.second();
        let huge = "A".repeat(mcp::http::BODY_MAX + 1024);
        match big.call("run", &format!(r#"{{"elf_b64":"{huge}"}}"#)) {
            Err(e) => kassert!(e.contains("413") || e.contains("write") || e.contains("answer"), "oversized body: {e}"),
            Ok(r) => kassert!(false, "an oversized body was taken: {:?} {}", r.rpc_error, &r.text[..r.text.len().min(200)]),
        }
        big.close();
        c.ok("version", "{}")?;
        Ok(())
    })?;
    kassert!(super::running() == before, "a job is still counted: {} > {before}", super::running());
    kassert!(crate::processes().len() <= procs_before, "a process was left behind: {} > {procs_before}", crate::processes().len());
    Ok(())
}
ktest!("mc.run.bad_elf_b64", Safe, bad_elf_b64);

/// The path when no retry thread can be started: the tool's own worker nudges on (`keep_nudging`) and ends a
/// killed program that never leaves user mode; its job is then given back as usual.
fn keep_nudging_inline_ends_a_spinner() -> TestResult {
    if let Some(r) = needs_two_cpus() {
        return r;
    }
    let image = progs::elf(progs::SPIN);
    let o = crate::SpawnOptions::new(&["spin"], &[], super::MCP_PROGRAM);
    let p = crate::spawn_image_with("/t/spin-inline", &image, &o).map_err(|e| format!("spawn: {e}"))?;
    sched::sleep_ns(SEC / 5);
    kassert!(p.status().is_none(), "the spinner ended by itself: {:?}", p.status());
    p.kill(uapi::ab::signal::SIGKILL);
    let t0 = time::now_ns();
    let ended = super::keep_nudging(&p);
    kassert!(ended && p.wait(Some(SEC)).is_some(), "the nudges did not end it");
    klog::event!(Info, "proc", "test.inline_nudge", ms = (time::now_ns() - t0) / 1_000_000);
    Ok(())
}
ktest!("mc.run.keep_nudging_inline_ends_a_spinner", Safe, keep_nudging_inline_ends_a_spinner);
