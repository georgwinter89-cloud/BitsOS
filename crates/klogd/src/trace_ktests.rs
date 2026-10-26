//! In-kernel tests of RB-7a: trace rings, rate limits, the `events` tool. All Safe: the rings under test are
//! local, and the global rate limit is changed only for a moment and restored (rerunnable on the Mini-PC).

use crate::tool::call_tool;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use klog::trace::{Kind, Ring};
use ktest::{TestResult, kassert, ktest};

fn err(e: mcp::ToolError) -> String {
    alloc::format!("{e:?}")
}

fn secs(ns: u64) -> String {
    alloc::format!("{}.{:09}", ns / 1_000_000_000, ns % 1_000_000_000)
}

fn registered() -> TestResult {
    let t = mcp::find("events").ok_or("events tool missing")?;
    kassert!(t.owner == "RB" && t.safety == mcp::Safety::Read);
    kassert!(mcp::Json::parse(t.input_schema).is_ok());
    kassert!(klog::trace::ring(hal::percpu::this_cpu()).is_some(), "no trace ring on this CPU");
    Ok(())
}
ktest!("rb.events.tool_registered", Safe, registered);

/// A ring keeps the newest entries in order, written from thread and interrupts-off context alike.
fn ring_wrap() -> TestResult {
    let r = Ring::new(hal::percpu::this_cpu() as u32, 64);
    for i in 0..1000u64 {
        if i % 2 == 0 {
            let _irq = hal::cpu::IrqGuard::new();
            r.push(i, Kind::Mark, 1, 0, i, !i);
        } else {
            r.push(i, Kind::Mark, 1, 0, i, !i);
        }
    }
    kassert!(r.span() == (936, 1000) && r.overwritten() == 936, "{:?}", r.span());
    let mut seen = Vec::new();
    let lost = r.scan(false, |e| {
        seen.push((e.a, e.b == !e.a));
        true
    });
    kassert!(lost == 0 && seen.len() == 64, "lost {lost}, {} seen", seen.len());
    kassert!(seen.iter().enumerate().all(|(i, &(a, ok))| ok && a == 936 + i as u64), "{:?}", seen);
    Ok(())
}
ktest!("rb.trace.ring_wrap", Safe, ring_wrap);

/// One writer pinned to every CPU, a reader on this one: no entry is ever torn, every write is counted.
fn smp_writers() -> TestResult {
    const EACH: u64 = 20_000;
    let cpus = sched::wait_cpus_online(2_000_000_000).max(1);
    let r = Arc::new(Ring::new(0, 256));
    let done = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for c in 0..cpus {
        let r = r.clone();
        let opts = sched::SpawnOpts { cpu: Some(c), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
        let h = sched::spawn_with("rb-trace", sched::Class::Normal, opts, move || {
            for i in 0..EACH {
                r.push(i, Kind::Mark, c as u64, 0, i, i ^ ((c as u64) << 40));
            }
        })
        .map_err(|e| alloc::format!("spawn on cpu {c}: {e:?}"))?;
        hs.push(h);
    }
    let mut checked = 0u64;
    let mut bad = None;
    while !done.load(Ordering::Relaxed) && checked < 1_000_000 {
        r.scan(true, |e| {
            if e.b != e.a ^ (e.thread << 40) {
                bad = Some(*e);
            }
            checked += 1;
            bad.is_none()
        });
        if r.span().1 == cpus as u64 * EACH {
            done.store(true, Ordering::Relaxed);
        }
        sched::yield_now();
    }
    for h in &hs {
        kassert!(h.join_timeout(60_000_000_000), "a writer did not finish in 60 s");
    }
    kassert!(bad.is_none(), "torn entry {:?}", bad);
    kassert!(r.span().1 == cpus as u64 * EACH, "{} writes counted of {}", r.span().1, cpus as u64 * EACH);
    Ok(())
}
ktest!("rb.trace.smp_writers", Safe, smp_writers);

fn reported_now(since: u64, subsystem: &str) -> u64 {
    let filter = klog::Filter { subsystem: Some("klog"), name: Some("ratelimit.suppressed"), ..klog::Filter::ALL };
    klog::read(since, usize::MAX, &filter)
        .records
        .iter()
        .filter(|r| klog::fields::value_of(&r.text, "subsystem") == Some(subsystem))
        .filter_map(|r| klog::fields::value_of(&r.text, "count")?.parse::<u64>().ok())
        .sum()
}

/// Sum of `count=` over klogd's `ratelimit.suppressed` records for `subsystem` from `since` on: waits (bounded)
/// until it reaches `want`, then one more sweep (klogd sweeps once a second), so a count reported twice shows.
fn reported(since: u64, subsystem: &str, want: u64) -> u64 {
    let t0 = time::now_ns();
    while reported_now(since, subsystem) < want && time::now_ns() - t0 < 5_000_000_000 {
        sched::sleep_ms(50);
    }
    sched::sleep_ms(1200);
    reported_now(since, subsystem)
}

/// A suppressed record takes no sequence number: the log from `since` on has no gap (unless the ring wrapped
/// past `since` meanwhile, then there is nothing to check).
fn no_gap(since: u64) -> TestResult {
    let page = klog::read(since, usize::MAX, &klog::Filter::ALL);
    if page.oldest > since {
        return Ok(());
    }
    kassert!(page.gaps.is_empty(), "gaps {:?}", page.gaps.iter().map(|g| (g.before, g.count)).collect::<Vec<_>>());
    let bad = page.records.windows(2).find(|w| w[1].seq != w[0].seq + 1).map(|w| (w[0].seq, w[1].seq));
    kassert!(bad.is_none(), "sequence jumps {:?}", bad);
    Ok(())
}

fn stored(since: u64, subsystem: &str) -> usize {
    klog::read(since, usize::MAX, &klog::Filter { subsystem: Some(subsystem), ..klog::Filter::ALL }).records.len()
}

/// 300 records of one event at a limit of 20 at once: about 20 stored, the rest counted, and klogd reports them.
fn ratelimit() -> TestResult {
    let before = klog::ratelimit::get();
    let lost0 = klog::lost();
    klog::ratelimit::set(20, 5);
    let start = klog::next_seq();
    for i in 0..300u32 {
        klog::event!(Warn, "rbrl", "flood", i = i);
    }
    klog::ratelimit::set(before.0, before.1);
    let kept = stored(start, "rbrl") as u64;
    // 20 at once plus 5 per second (a rerun right after the last finds less of the burst left).
    kassert!(kept <= 22, "{kept} of 300 stored at a limit of 20");
    kassert!(klog::lost() == lost0, "{} records lost", klog::lost() - lost0);
    // Exact: the last run waited for its whole report, so nothing of it comes after `start`; more is a double report.
    let sum = reported(start, "rbrl", 300 - kept);
    kassert!(sum == 300 - kept, "klogd reported {sum} suppressed, want {}", 300 - kept);
    no_gap(start)
}
ktest!("rb.events.ratelimit", Safe, ratelimit);

/// Every CPU logs one event as fast as it can: the ring gets about the burst, not the storm, and every record
/// is either stored or reported as suppressed.
fn storm_bounded() -> TestResult {
    const EACH: u64 = 3000;
    let cpus = sched::wait_cpus_online(2_000_000_000).max(1);
    let (burst, rate) = klog::ratelimit::get();
    if rate == 0 {
        return ktest::skip("rate limits are off (CONFIG log_ratelimit=off)");
    }
    let lost0 = klog::lost();
    let start = klog::next_seq();
    let t0 = time::now_ns();
    let mut hs = Vec::new();
    for c in 0..cpus {
        let opts = sched::SpawnOpts { cpu: Some(c), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
        let h = sched::spawn_with("rb-rlstorm", sched::Class::Normal, opts, move || {
            for i in 0..EACH {
                // One call site (= one rate-limit key), every second record with interrupts off.
                let _irq = (i % 2 == 1).then(hal::cpu::IrqGuard::new);
                klog::event!(Warn, "rbsb", "storm", c = c, i = i);
            }
        })
        .map_err(|e| alloc::format!("spawn on cpu {c}: {e:?}"))?;
        hs.push(h);
    }
    for h in &hs {
        kassert!(h.join_timeout(60_000_000_000), "a storm thread did not finish in 60 s");
    }
    let secs = (time::now_ns() - t0) / 1_000_000_000 + 1;
    let total = cpus as u64 * EACH;
    let kept = stored(start, "rbsb") as u64;
    let bound = burst + rate * secs + cpus as u64;
    kassert!(kept <= bound, "{kept} of {total} stored, bound {bound}");
    kassert!(klog::lost() == lost0, "{} records lost", klog::lost() - lost0);
    let sum = reported(start, "rbsb", total - kept);
    kassert!(sum == total - kept, "reported {sum} suppressed, want {}", total - kept);
    no_gap(start)?;
    klog::event!(Info, "rb", "ktest.rl_storm", cpus = cpus, attempted = total, stored = kept, secs = secs);
    Ok(())
}
ktest!("rb.events.storm_bounded", Safe, storm_bounded);

/// The `events` tool merges log records and trace entries and filters them by time, level, subsystem, fields.
fn filters() -> TestResult {
    if !klog::trace::enabled(Kind::Mark) {
        return ktest::skip("trace kind mark is off (CONFIG trace=)");
    }
    let t0 = klog::now();
    let tm = klog::ktests::with_trace("rbev", || {
        klog::event!(Warn, "rbev", "x.one", dev = "eth0", n = 1u32);
        klog::trace::mark("rbev", "m.hit", 7, 9);
        let tm = klog::now();
        klog::event!(Info, "rbev", "x.two", n = 5u32);
        tm
    })?;
    let t1 = klog::now();
    let from = secs(t0);
    let ask = |extra: &str| call_tool("events", &alloc::format!(r#"{{"from":{from},"subsystem":"rbev"{extra}}}"#)).map_err(err);
    let all = ask("")?;
    let lines: Vec<&str> = all.lines().collect();
    kassert!(lines.len() == 4 && lines[0].starts_with("events from=") && lines[0].contains("shown=3 more=0"), "{all}");
    kassert!(lines[1].starts_with('#') && lines[1].ends_with("W rbev x.one dev=eth0 n=1"), "{all}");
    kassert!(lines[2].starts_with('~') && lines[2].contains(" D rbev m.hit a=7 b=9 cpu="), "{all}");
    kassert!(lines[3].ends_with("I rbev x.two n=5"), "{all}");
    let f = ask(r#","fields":"n>2""#)?;
    kassert!(f.contains("shown=1") && f.contains("x.two"), "{f}");
    let w = ask(r#","level":"warn""#)?;
    kassert!(w.contains("shown=1") && w.contains("x.one"), "{w}");
    let t = ask(r#","source":"trace""#)?;
    kassert!(t.contains("shown=1") && t.contains("m.hit"), "{t}");
    let one = ask(r#","max":1"#)?;
    kassert!(one.contains("shown=1 more=1") && one.contains("x.one"), "from: the oldest first: {one}");
    let tail = call_tool("events", r#"{"subsystem":"rbev","max":1}"#).map_err(err)?;
    kassert!(tail.contains("shown=1 more=1") && tail.contains("x.two"), "without from: the newest: {tail}");
    let later = call_tool("events", &alloc::format!(r#"{{"from":{},"subsystem":"rbev"}}"#, secs(t1 + 1000))).map_err(err)?;
    kassert!(later.contains("shown=0"), "{later}");
    kassert!(call_tool("events", r#"{"fields":"a>b"}"#) == Err(mcp::ToolError::BadArgument(String::from("fields"))));
    // Time range: `to` ends it, `last` counts back from now, and the two ways cannot be mixed.
    let mid = call_tool("events", &alloc::format!(r#"{{"from":{from},"to":{},"subsystem":"rbev"}}"#, secs(tm))).map_err(err)?;
    kassert!(mid.contains("shown=2 more=0") && mid.contains("x.one") && mid.contains("m.hit") && !mid.contains("x.two"), "{mid}");
    let none = call_tool("events", &alloc::format!(r#"{{"from":{from},"to":{from},"subsystem":"rbev"}}"#)).map_err(err)?;
    kassert!(none.contains("shown=0"), "a range that ends before the first event: {none}");
    let bad = |a: &str, which: &str| call_tool("events", a) == Err(mcp::ToolError::BadArgument(String::from(which)));
    kassert!(bad(r#"{"from":5,"to":4}"#, "to") && bad(r#"{"from":1,"last":2}"#, "last") && bad(r#"{"last":-1}"#, "last"));
    sched::sleep_ms(50);
    let recent = call_tool("events", r#"{"last":0.01,"subsystem":"rbev"}"#).map_err(err)?;
    kassert!(recent.contains("shown=0"), "nothing of rbev in the last 10 ms: {recent}");
    let hour = call_tool("events", r#"{"last":3600,"subsystem":"rbev","max":1}"#).map_err(err)?;
    kassert!(hour.contains("shown=1 more=1") && hour.contains("x.two"), "last: the newest of the window: {hour}");
    Ok(())
}
ktest!("rb.events.filters", Safe, filters);

/// The hooks the other lanes wire in (slow syscall, lock wait, switch) record through to `events` with the
/// limits in force (CONFIG `trace_syscall_ms=`, `trace_lock_us=`); a blocked syscall is not slow.
fn hooks() -> TestResult {
    let hz = klog::trace::tsc_hz();
    if hz == 0 {
        return ktest::skip("no TSC rate set (klogd not started)");
    }
    let lim = klog::trace::limits();
    let kinds = klog::trace::kinds();
    let t0 = klog::now();
    klog::trace::set_kinds("all").map_err(|e| alloc::format!("kinds: {e}"))?;
    let slow = t0.saturating_sub(2 * lim.syscall_ns);
    klog::trace::syscall_done(4242, slow, -4, false);
    klog::trace::syscall_done(4243, slow, -4, true);
    klog::trace::syscall_done(4244, klog::now(), 0, false);
    let ticks = |ns: u64| ((ns as u128 * hz as u128) / 1_000_000_000) as u64;
    klog::trace::lock_wait("rbtest.lock", 0x1234, ticks(2 * lim.lock_wait_ns) + 1);
    klog::trace::lock_wait("rbtest.short", 0x5678, ticks(lim.lock_wait_ns / 2));
    klog::trace::switch(77_001, 77_002);
    klog::trace::set_kinds(&kinds).map_err(|e| alloc::format!("kinds: {e}"))?;
    let from = secs(t0);
    let ask = |extra: &str| call_tool("events", &alloc::format!(r#"{{"from":{from},"source":"trace"{extra}}}"#)).map_err(err);
    let sys = ask(r#","subsystem":"syscall","fields":"nr>=4242 nr<=4244""#)?;
    kassert!(sys.contains("shown=1") && sys.contains("trace.slow_syscall nr=4242 ret=-4 us="), "{sys}");
    let lock = ask(r#","subsystem":"lock","fields":"addr=0x1234""#)?;
    kassert!(lock.contains("shown=1") && lock.contains("lock=rbtest.lock"), "{lock}");
    let short = ask(r#","subsystem":"lock","fields":"addr=0x5678""#)?;
    kassert!(short.contains("shown=0"), "a wait under the limit: {short}");
    let sw = ask(r#","subsystem":"sched","fields":"prev=77001 next=77002""#)?;
    kassert!(sw.contains("shown=1") && sw.contains(" T sched trace.switch "), "{sw}");
    Ok(())
}
ktest!("rb.trace.hooks", Safe, hooks);
