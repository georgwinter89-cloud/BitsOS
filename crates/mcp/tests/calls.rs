//! MC-2's tool framework over real sockets: deadlines and "Still running (call n)", `output` and `stop`
//! answered by the server itself (also while every worker is busy), results picked up after the deadline,
//! worker reuse, and the answer-first-then-restart rule (`chk-restart.mjs`). Ports from the OS (`serve`).
mod common;
use common::*;
use std::time::{Duration, Instant};

fn send(c: &mut Client, id: u64, name: &str, args: &str) -> Answer {
    c.write(tool_call(id, name, args));
    c.answer(30_000).expect("an answer")
}

#[test]
fn still_running_then_output_then_stop() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let t0 = Instant::now();
    let a = send(&mut c, 1, "t_spin", r#"{"timeout_ms":300}"#);
    let ms = t0.elapsed().as_millis();
    assert!(is_error(&a) && texts(&a).starts_with("Still running: t_spin (call ") && (250..2000).contains(&ms), "{ms} ms {}", a.body);
    assert!(texts(&a).contains("output {\"call\":") && texts(&a).contains("stop {\"call\":"), "{}", texts(&a));
    // The progress so far comes with the answer; output then shows what came after it.
    assert!(texts(&a).contains("\n--- progress so far ---\ntick 0\n"), "{}", texts(&a));
    let n = call_number(&a);
    assert_eq!(srv.calls(), 1);
    sleep(200);
    let o = send(&mut c, 2, "output", &format!(r#"{{"call":{n}}}"#));
    let t = texts(&o);
    assert!(!is_error(&o) && t.starts_with(&format!("call {n} t_spin: running after ")) && !t.contains("tick 0\n") && t.contains("tick") && t.contains("deadline passed"), "{t}");
    // Only new output the second time.
    sleep(120);
    let t2 = texts(&send(&mut c, 3, "output", &format!(r#"{{"call":{n}}}"#)));
    assert!(!t2.contains("tick 0\n") && t2.contains("tick"), "{t2}");
    let s = send(&mut c, 4, "stop", &format!(r#"{{"call":{n}}}"#));
    let t = texts(&s);
    assert!(!is_error(&s) && t.starts_with(&format!("call {n} t_spin: stopped")) && t.contains("spun") && t.contains("stopped"), "{t}");
    assert_eq!(srv.calls(), 0, "a handed-over call leaves the table");
    let gone = send(&mut c, 5, "output", &format!(r#"{{"call":{n}}}"#));
    assert!(is_error(&gone) && texts(&gone).contains(&format!("no call {n}")), "{}", gone.body);
    assert!(events("call.stop").iter().any(|e| e.contains(&format!("call={n}"))));
    c.close();
    stop(srv);
}

#[test]
fn a_late_result_waits_for_output() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let a = send(&mut c, 1, "t_sleep", r#"{"ms":700,"timeout_ms":100}"#);
    let n = call_number(&a);
    assert!(until(3000, || srv.tools_running() == 0));
    assert_eq!(srv.calls(), 1, "kept for output");
    // Without a number: the newest call.
    let o = send(&mut c, 2, "output", "{}");
    let t = texts(&o);
    assert!(t.starts_with(&format!("call {n} t_sleep: finished after ")) && t.contains("slept 700 ms"), "{t}");
    assert_eq!(srv.calls(), 0);
    let none = send(&mut c, 3, "output", "{}");
    assert_eq!(texts(&none), "no tool call is running or waiting to be read");
    let none = send(&mut c, 4, "stop", "{}");
    assert_eq!(texts(&none), "no tool call is running");
    // A bad call number is a bad argument (the schema says integer).
    let bad = send(&mut c, 5, "output", r#"{"call":"seven"}"#);
    assert_eq!(bad.json().get("error").and_then(|e| e.get("code")).and_then(mcp::Json::as_i64), Some(-32602), "{}", bad.body);
    c.close();
    stop(srv);
}

#[test]
fn stop_of_a_tool_that_does_not_look() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let n = call_number(&send(&mut c, 1, "t_spin", r#"{"ignore_stop":true,"ms":3500,"timeout_ms":100}"#));
    let t0 = Instant::now();
    let s = send(&mut c, 2, "stop", &format!(r#"{{"call":{n}}}"#));
    let ms = t0.elapsed().as_millis();
    let t = texts(&s);
    assert!(t.contains("still running") && t.contains("stop asked") && (400..1500).contains(&ms), "{ms} ms: {t}");
    assert!(events("call.stop_ignored").iter().any(|e| e.contains(&format!("call={n}"))));
    assert!(until(5000, || srv.tools_running() == 0));
    let o = send(&mut c, 3, "output", &format!(r#"{{"call":{n}}}"#));
    assert!(texts(&o).contains("finished") && texts(&o).contains("spun") && texts(&o).contains("done"), "{}", texts(&o));
    c.close();
    stop(srv);
}

#[test]
fn output_and_stop_answer_while_every_worker_is_busy() {
    let (p, srv) = serve(|_| {});
    let mut busy = Vec::new();
    for i in 0..3 {
        let mut c = Client::new(p);
        // Abandoned (Still running) so `output {}` may pick them; the workers stay stuck.
        c.write(tool_call(i, "t_spin", r#"{"ignore_stop":true,"ms":4000,"timeout_ms":100}"#));
        busy.push(c);
    }
    assert!(until(3000, || srv.tools_running() == 3));
    for c in &busy {
        assert!(texts(&c.answer(3000).expect("Still running")).starts_with("Still running"));
    }
    let mut c = Client::new(p);
    let t0 = Instant::now();
    let o = send(&mut c, 9, "output", "{}");
    assert!(texts(&o).contains("running") && t0.elapsed() < Duration::from_millis(500), "{:?} {}", t0.elapsed(), texts(&o));
    assert_eq!(srv.workers(), 3, "output ran in the connection, not in a fourth worker");
    // The server stopping asks the running calls to stop; waiting callers answer Still running.
    for c in busy {
        c.close();
    }
    c.close();
    stop(srv);
}

#[test]
fn stopping_the_server_stops_tools_that_look() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    c.write(tool_call(1, "t_spin", r#"{"ms":20000,"timeout_ms":60000}"#));
    assert!(until(3000, || srv.tools_running() == 1));
    srv.stop();
    // Either the tool saw its flag first (its own answer) or the connection gave up first (Still running).
    let a = c.answer(3000).expect("an answer at once");
    let t = texts(&a);
    assert!(t == "spun 1, stopped" || t.starts_with("spun") || (t.starts_with("Still running") && t.contains("the server stops")), "{t}");
    assert!(until(2000, || srv.tools_running() == 0), "the spinning tool saw its stop flag");
    assert!(srv.wait_stopped(3_000_000_000));
    c.close();
}

#[test]
fn workers_are_reused() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    for i in 0..20 {
        assert_eq!(texts(&send(&mut c, i, "t_echo", r#"{"text":"x"}"#)), "x");
    }
    assert_eq!(srv.workers(), 1, "one worker serves calls one after the other");
    assert_eq!(srv.calls(), 0);
    c.close();
    stop(srv);
}

#[test]
fn answer_first_then_the_action() {
    let (p, srv) = serve(|cfg| cfg.after_grace_ns = 300_000_000);
    let mut c = Client::new(p);
    let a = send(&mut c, 1, "t_after", r#"{"tag":"first"}"#);
    let got = Instant::now();
    assert_eq!(texts(&a), "after first registered=true");
    assert!(a.closes(), "the connection ends with an answer that restarts something: {}", a.head);
    assert!(after_ran("first").is_none(), "the action waits for the answer");
    assert!(c.wait_ended(3000), "closed after the answer");
    assert!(until(3000, || after_ran("first").is_some()));
    let ran = after_ran("first").unwrap();
    assert!(ran > got && ran.duration_since(got) >= Duration::from_millis(150), "answer at {got:?}, action at {ran:?}");
    assert!(events("tool.after_answer").iter().any(|e| e.contains("tool=t_after") && e.contains("answer_sent=true")));
    c.close();
    // Registered, but the tool returned after its caller got Still running: the action is dropped, nothing
    // restarts behind the caller's back.
    let mut c = Client::new(p);
    let a = send(&mut c, 2, "t_after", r#"{"tag":"late","ms":700,"timeout_ms":100}"#);
    assert!(texts(&a).starts_with("Still running"));
    assert!(until(3000, || srv.tools_running() == 0));
    sleep(500);
    assert!(after_ran("late").is_none(), "no action after a Still running answer");
    assert!(events("call.after_dropped").iter().any(|e| e.contains("tool=t_after")));
    c.close();
    stop(srv);
}

#[test]
fn a_detached_context_has_no_call() {
    init();
    let ctx = mcp::ToolContext::detached(u64::MAX);
    ctx.output("nobody reads this");
    assert!(!ctx.stop_requested() && !ctx.expired() && ctx.call_id().is_none());
    assert!(!ctx.after_answer(Box::new(|| panic!("must never run"))), "no answer to wait for: the tool must not restart");
    let out = (mcp::find("output").unwrap().handler)(&ctx, &mcp::Json::Null);
    assert!(matches!(out, Err(mcp::ToolError::Failed(ref m)) if m.contains("server itself")), "{out:?}");
}

/// `output`/`stop` from another connection never take the result of a call whose caller still waits: without
/// a number they do not even see it; with its number they show it and may ask it to stop, and the result still
/// goes to the caller that started it.
#[test]
fn a_waited_call_stays_with_its_caller() {
    let (p, srv) = serve(|_| {});
    let mut a = Client::new(p);
    a.write(tool_call(1, "t_spin", r#"{"ms":20000,"timeout_ms":60000}"#));
    assert!(until(3000, || srv.tools_running() == 1));
    let mut b = Client::new(p);
    assert_eq!(texts(&send(&mut b, 1, "output", "{}")), "no tool call is running or waiting to be read");
    assert_eq!(texts(&send(&mut b, 2, "stop", "{}")), "no tool call is running");
    sleep(120);
    let o = texts(&send(&mut b, 3, "output", r#"{"call":1}"#));
    assert!(o.starts_with("call 1 t_spin: running; its caller is still waiting") && o.contains("tick 0"), "{o}");
    let s = texts(&send(&mut b, 4, "stop", r#"{"call":1}"#));
    assert!(s.starts_with("call 1 t_spin: stopped; its caller is still waiting and gets the result") && !s.contains("spun"), "{s}");
    let mine = a.answer(3000).expect("the caller's own answer");
    assert_eq!(texts(&mine), format!("spun {}, stopped", texts(&mine).split(' ').nth(1).unwrap().trim_end_matches(',')));
    assert!(!is_error(&mine), "{}", mine.body);
    assert_eq!(srv.calls(), 0);
    // Right after a call finished, before its caller took it: still the caller's.
    a.write(tool_call(2, "t_sleep", r#"{"ms":300,"timeout_ms":60000}"#));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        let t = texts(&send(&mut b, 9, "output", r#"{"call":2}"#));
        assert!(!t.contains("slept"), "output took the waited call's result: {t}");
        if t.contains("no call 2") {
            break;
        }
    }
    assert!(texts(&a.answer(3000).expect("answer")).starts_with("slept 300 ms"));
    a.close();
    b.close();
    stop(srv);
}

/// A worker that took its job from the queue must not hold the queue while the tool runs: a later call that
/// goes to another, idle worker is answered at once (it used to wait for the first tool to end; in the kernel
/// the spin froze the CPU).
#[test]
fn a_queued_job_does_not_block_the_queue() {
    let (p, srv) = serve(|_| {});
    let mut a = Client::new(p);
    assert_eq!(texts(&send(&mut a, 1, "t_echo", r#"{"text":"warm"}"#)), "warm");
    assert!(until(2000, || srv.workers() == 1 && srv.tools_running() == 0));
    // Goes to the waiting worker through the queue and keeps it busy.
    a.write(tool_call(2, "t_sleep", r#"{"ms":2500,"timeout_ms":60000}"#));
    assert!(until(2000, || srv.tools_running() == 1));
    let mut b = Client::new(p);
    assert_eq!(texts(&send(&mut b, 1, "t_echo", r#"{"text":"second worker"}"#)), "second worker");
    assert!(until(2000, || srv.tools_running() == 1 && srv.workers() == 2));
    // The second worker waits idle on the queue now; this call reaches it through the queue.
    let t0 = Instant::now();
    assert_eq!(texts(&send(&mut b, 2, "t_echo", r#"{"text":"queued"}"#)), "queued");
    assert!(t0.elapsed() < Duration::from_millis(800), "the queue was held by the busy worker: {:?}", t0.elapsed());
    assert!(texts(&a.answer(5000).expect("the sleeper's answer")).starts_with("slept 2500 ms"));
    a.close();
    b.close();
    stop(srv);
}

/// MC-5b: a call that runs a program is found by its pid (`output`/`stop` with `pid`), `stop` hands the grace to
/// it, a pid no call runs goes to the process back end (none in a host test: a clean error), and call/pid/pgid
/// exclude each other.
#[test]
fn output_and_stop_by_pid() {
    let (p, srv) = serve(|_| {});
    let mut c = Client::new(p);
    let a = send(&mut c, 1, "t_spin", r#"{"pid":4242,"timeout_ms":200}"#);
    let n = call_number(&a);
    let o = texts(&send(&mut c, 2, "output", r#"{"pid":4242}"#));
    assert!(o.starts_with(&format!("call {n} t_spin: running after ")), "{o}");
    let both = send(&mut c, 3, "output", &format!(r#"{{"pid":4242,"call":{n}}}"#));
    assert_eq!(both.json().get("error").and_then(|e| e.get("code")).and_then(mcp::Json::as_i64), Some(-32602), "{}", both.body);
    let bad = send(&mut c, 4, "stop", r#"{"pid":4242,"grace_ms":10001}"#);
    assert_eq!(bad.json().get("error").and_then(|e| e.get("code")).and_then(mcp::Json::as_i64), Some(-32602), "{}", bad.body);
    let other = send(&mut c, 5, "stop", r#"{"pid":4243}"#);
    assert!(is_error(&other) && texts(&other).contains("no call of this server runs it"), "{}", other.body);
    let s = texts(&send(&mut c, 6, "stop", r#"{"pid":4242,"grace_ms":250}"#));
    assert!(s.starts_with(&format!("call {n} t_spin: stopped")) && s.contains("grace 250 ms"), "{s}");
    assert!(events("call.stop").iter().any(|e| e.contains(&format!("call={n}")) && e.contains("pid=4242") && e.contains("grace_ms=250")));
    // The default grace: by pid 1 s, by call none (A1's SIGKILL at once).
    call_number(&send(&mut c, 7, "t_spin", r#"{"pid":4244,"timeout_ms":100}"#));
    let s = texts(&send(&mut c, 8, "stop", r#"{"pid":4244}"#));
    assert!(s.contains(&format!("grace {} ms", mcp::DEFAULT_GRACE_MS)), "{s}");
    let n3 = call_number(&send(&mut c, 9, "t_spin", r#"{"pid":4245,"timeout_ms":100}"#));
    let s = texts(&send(&mut c, 10, "stop", &format!(r#"{{"call":{n3}}}"#)));
    assert!(s.contains("grace 0 ms"), "{s}");
    c.close();
    stop(srv);
}
