//! In-kernel tests of the klog service: the `log` tool through the MCP registry and the drain thread.

use crate::tool;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use klog::{Level, Sink};
use ksync::SpinLock;
use ktest::{TestResult, kassert, ktest};

fn err(e: mcp::ToolError) -> String {
    alloc::format!("{e:?}")
}

fn registered() -> TestResult {
    let t = mcp::find("log").ok_or("log tool missing")?;
    kassert!(t.owner == "RB" && t.safety == mcp::Safety::Read);
    kassert!(mcp::Json::parse(t.input_schema).is_ok());
    Ok(())
}
ktest!("rb.log.tool_registered", Safe, registered);

fn reads_and_filters() -> TestResult {
    let s = klog::next_seq();
    klog::ktests::with_trace("rblog", || {
        klog::event!(Warn, "rblog", "t.one", n = 1u32);
        klog::event!(Info, "rblog.sub", "t.two", n = 2u32);
    })?;
    klog::ktests::with_trace("rbother", || klog::event!(Info, "rbother", "u.three"))?;
    let all = tool::call(&alloc::format!(r#"{{"since":{s}}}"#)).map_err(err)?;
    kassert!(all.starts_with("log next="), "{}", all);
    kassert!(all.contains(&alloc::format!("\n#{s} [")) && all.contains("W rblog t.one n=1"), "{}", all);
    kassert!(all.contains("u.three"));
    let warn = tool::call(&alloc::format!(r#"{{"since":{s},"level":"warn","subsystem":"rblog"}}"#)).map_err(err)?;
    kassert!(warn.contains("t.one") && !warn.contains("t.two"), "{}", warn);
    let sub = tool::call(&alloc::format!(r#"{{"since":{s},"subsystem":"rblog"}}"#)).map_err(err)?;
    kassert!(sub.contains("t.two") && !sub.contains("u.three"), "{}", sub);
    let one = tool::call(&alloc::format!(r#"{{"since":{s},"subsystem":"rblog","max":1}}"#)).map_err(err)?;
    kassert!(one.lines().count() == 2 && one.lines().nth(1).is_some_and(|l| l.contains("t.one")), "{}", one);
    let tail = tool::call(r#"{"name":"u.","max":1}"#).map_err(err)?;
    kassert!(tail.lines().nth(1).is_some_and(|l| l.ends_with("I rbother u.three")), "{}", tail);
    kassert!(tool::call(r#"{"level":"loud"}"#) == Err(mcp::ToolError::BadArgument("level".to_string())));
    Ok(())
}
ktest!("rb.log.tool_reads", Safe, reads_and_filters);

fn reports_gaps() -> TestResult {
    let s = klog::next_seq();
    let stored = klog::__with_ring_held(|| klog::try_event!(Error, "rblog", "lost.one"));
    kassert!(!stored);
    klog::event!(Error, "rblog", "after.gap");
    let out = tool::call(&alloc::format!(r#"{{"since":{s},"subsystem":"rblog"}}"#)).map_err(err)?;
    let lines: Vec<&str> = out.lines().collect();
    kassert!(lines.first().is_some_and(|l| l.contains("missed=1")), "{}", out);
    kassert!(lines.get(1) == Some(&"[... 1 records lost]"), "{}", out);
    kassert!(lines.get(2).is_some_and(|l| l.ends_with("E rblog after.gap")), "{}", out);
    Ok(())
}
ktest!("rb.log.tool_reports_gaps", Safe, reports_gaps);

struct Lines(SpinLock<Vec<String>>);
impl Sink for Lines {
    fn write_line(&self, _l: Level, line: &str) {
        self.0.lock().push(line.to_string());
    }
}
static SEEN: Lines = Lines(SpinLock::new(Vec::new()));

/// The klogd thread feeds a deferred sink without anyone calling drain by hand.
fn drain_thread() -> TestResult {
    kassert!(crate::drain_running() || sched::threads().iter().any(|t| t.1 == "klogd"), "klogd thread missing");
    let id = klog::add_deferred_sink("ktest-drain", &SEEN, false);
    klog::event!(Error, "rblog", "drain.me");
    let deadline = time::now_ns() + 5_000_000_000;
    let mut got = false;
    while time::now_ns() < deadline && !got {
        sched::sleep_ms(5);
        got = SEEN.0.lock().iter().any(|l| l.ends_with("E rblog drain.me"));
    }
    klog::remove_sink(id);
    kassert!(got, "the drain thread did not deliver within 5 s");
    Ok(())
}
ktest!("rb.log.drain_thread", Safe, drain_thread);

fn ring_sized_from_memory() -> TestResult {
    let total = mm_core::frames().map(|f| f.stats().total_bytes).ok_or("no frames")?;
    let info = klog::ring_info();
    kassert!(!info.early);
    if info.sized_by_config {
        return ktest::skip("CONFIG log_buf= chose the size");
    }
    kassert!(info.bytes == klog::ring_size(klog::ring_bytes_for_memory(total)), "{} MB RAM: {:?}", total >> 20, info);
    Ok(())
}
ktest!("rb.log.ring_sized", Safe, ring_sized_from_memory);

// ---- RB-klog: every CPU logs at once while a slow sink is attached ----------------------------------------

/// A deferred sink that needs `SLOW_NS` per storm line (a framebuffer scroll, a busy network card).
struct Slow {
    lines: SpinLock<Vec<String>>,
    lost: AtomicU64,
}
impl Sink for Slow {
    fn write_line(&self, _l: Level, line: &str) {
        if !line.contains(" rbstorm e ") {
            return;
        }
        let until = time::now_ns() + SLOW_NS;
        while time::now_ns() < until {
            core::hint::spin_loop();
        }
        self.lines.lock().push(line.to_string());
    }
    fn lost(&self, records: u64) {
        self.lost.fetch_add(records, Ordering::Relaxed);
    }
}
static SLOW: Slow = Slow { lines: SpinLock::new(Vec::new()), lost: AtomicU64::new(0) };
const SLOW_NS: u64 = 50_000;
/// Events per storm in all (about 150 KiB of the ring, which is 1 MiB under QEMU's 2 GiB: the storm must not
/// push the boot out of it, `rb.mirror.early_buffer` compares the boot), split over the CPUs.
const STORM_RECORDS: usize = 1600;

/// Events per CPU.
fn each(cpus: usize) -> u32 {
    (STORM_RECORDS / cpus.max(1)).clamp(50, 300) as u32
}

/// The emitter-fed phase (no klogd) is on; restored on every way out.
struct NoDrainer;
impl Drop for NoDrainer {
    fn drop(&mut self) {
        klog::set_drainer(true);
    }
}

/// `c=<cpu> i=<n>` of a storm line.
fn storm_key(line: &str) -> Option<(u32, u32)> {
    let c = line.split(" c=").nth(1)?.split(' ').next()?.parse().ok()?;
    let i = line.split(" i=").nth(1)?.split(' ').next()?.parse().ok()?;
    Some((c, i))
}

/// Each CPU's lines in its own order, all of them.
fn per_cpu_complete(lines: &[String], cpus: usize) -> TestResult {
    let want = each(cpus);
    let mut next = alloc::vec![0u32; cpus];
    for l in lines {
        let (c, i) = storm_key(l).ok_or_else(|| alloc::format!("not a storm line: {l}"))?;
        let n = next.get_mut(c as usize).ok_or_else(|| alloc::format!("cpu {c} out of range: {l}"))?;
        kassert!(*n == i, "cpu {} line {} came when {} was due", c, i, n);
        *n += 1;
    }
    kassert!(next.iter().all(|&n| n == want), "lines per cpu (want {}): {:?}", want, next);
    Ok(())
}

struct Storm {
    cpus: usize,
    start: u64,
    stats: klog::LockStats,
    lost: u64,
}

/// One thread pinned to each online CPU logs `each(cpus)` events, every second one with interrupts off (as an IRQ
/// handler or an `IrqSpinLock` holder would).
fn storm() -> Result<Storm, String> {
    let cpus = sched::wait_cpus_online(2_000_000_000).max(1);
    {
        let mut l = SLOW.lines.lock();
        l.clear();
        l.reserve(cpus * each(cpus) as usize);
    }
    SLOW.lost.store(0, Ordering::Relaxed);
    let go = Arc::new(AtomicBool::new(false));
    let lost0 = klog::lost();
    klog::reset_lock_stats();
    let start = klog::next_seq();
    let mut hs = Vec::new();
    let n = each(cpus);
    for c in 0..cpus {
        let go = go.clone();
        let opts = sched::SpawnOpts { cpu: Some(c), pin: true, rt_prio: sched::RT_PRIO_DEFAULT };
        let h = sched::spawn_with("rb-storm", sched::Class::Normal, opts, move || {
            let t0 = time::now_ns();
            while !go.load(Ordering::Acquire) && time::now_ns() - t0 < 5_000_000_000 {
                core::hint::spin_loop();
            }
            for i in 0..n {
                if i % 2 == 1 {
                    let _irq = hal::cpu::IrqGuard::new();
                    klog::event!(Info, "rbstorm", "e", c = c, i = i);
                } else {
                    klog::event!(Info, "rbstorm", "e", c = c, i = i);
                }
            }
        })
        .map_err(|e| alloc::format!("spawn on cpu {c}: {e:?}"))?;
        hs.push(h);
    }
    go.store(true, Ordering::Release);
    for h in &hs {
        kassert!(h.join_timeout(60_000_000_000), "a storm thread did not finish in 60 s");
    }
    Ok(Storm { cpus, start, stats: klog::lock_stats(), lost: klog::lost() - lost0 })
}

/// The ring kept the whole storm (no hole, nothing lost: far below the ring size) and the lock stayed short.
fn check_ring(s: &Storm) -> Result<Vec<String>, String> {
    let page = klog::read(s.start, usize::MAX, &klog::Filter { subsystem: Some("rbstorm"), ..klog::Filter::ALL });
    kassert!(!page.busy, "ring busy");
    kassert!(s.lost == 0 && page.missed() == 0, "lost {} records, gaps {:?}", s.lost, page.gaps);
    let lines: Vec<String> = page.records.iter().map(|r| r.render()).collect();
    per_cpu_complete(&lines, s.cpus)?;
    // Generous under QEMU (a vCPU holding the ring may be descheduled by the host); tight on real hardware,
    // where a hold is one record copied in or a small batch copied out. Interrupts-off waiters queue for the
    // ring (KLock tickets, INT-A1e), so their wait is about one hold per CPU ahead: 16 CPUs x 5 us is ~80 us,
    // and 300 us leaves room without hiding an unfair lock (the Mini-PC measured 370 us before the queue).
    let (wait_limit, hold_limit) = if hal::cpu::detect().hypervisor { (1_000_000, 1_000_000) } else { (300_000, 100_000) };
    let st = s.stats;
    kassert!(st.timeouts == 0, "{:?}", st);
    kassert!(st.irqoff_wait_max_ns < wait_limit, "interrupts-off wait {} us: {:?}", st.irqoff_wait_max_ns / 1000, st);
    kassert!(st.hold_max_ns < hold_limit, "ring held {} us: {:?}", st.hold_max_ns / 1000, st);
    Ok(lines)
}

fn report(what: &str, s: &Storm, sink_ms: u64) {
    let st = s.stats;
    klog::event!(Info, "rb", "ktest.storm", phase = what, cpus = s.cpus, records = s.cpus * each(s.cpus) as usize,
        wait_max_us = st.wait_max_ns / 1000, irqoff_wait_max_us = st.irqoff_wait_max_ns / 1000,
        hold_max_us = st.hold_max_ns / 1000, skips = st.skips, sink_ms = sink_ms);
}

/// Waits (sleeping, bounded) until the slow sink has `n` lines; returns the ms it took.
fn wait_sink(n: usize, t0: u64) -> Result<u64, String> {
    let deadline = t0 + 60_000_000_000;
    while SLOW.lines.lock().len() < n {
        kassert!(time::now_ns() < deadline, "slow sink got {} of {} lines in 60 s", SLOW.lines.lock().len(), n);
        sched::sleep_ms(5);
    }
    Ok((time::now_ns() - t0) / 1_000_000)
}

/// One storm with klogd feeding the slow sink: emitters never wait for it.
fn storm_with_klogd() -> TestResult {
    let id = klog::add_deferred_sink("ktest-slow", &SLOW, false);
    let r = (|| {
        let s = klog::ktests::with_trace("rbstorm", storm)??;
        check_ring(&s)?;
        let ms = wait_sink(s.cpus * each(s.cpus) as usize, time::now_ns())?;
        kassert!(SLOW.lost.load(Ordering::Relaxed) == 0, "the sink was told of {} lost records", SLOW.lost.load(Ordering::Relaxed));
        per_cpu_complete(&SLOW.lines.lock(), s.cpus)?;
        report("klogd", &s, ms);
        Ok(())
    })();
    klog::remove_sink(id);
    r
}

/// Under QEMU a loaded host may stop a vCPU that holds the ring for milliseconds: one more try before failing.
fn tries(f: fn() -> TestResult) -> TestResult {
    match f() {
        Err(e) if hal::cpu::detect().hypervisor && !e.starts_with("\u{1}skip:") => {
            klog::event!(Warn, "rb", "ktest.retry", reason = e.as_str(); "one more try (QEMU timing)");
            f()
        }
        r => r,
    }
}

fn smp_storm() -> TestResult {
    tries(storm_with_klogd)
}
ktest!("rb.klog.smp_storm", Safe, smp_storm);

/// The boot before klogd: the emitters feed the slow sink themselves, one at a time, from the ring. Every line
/// arrives once, in sequence order, and nothing is reported lost (the old per-emitter feeding overtook itself
/// on several CPUs: lines dropped, false `[... 1 records lost]`). QemuOnly: it turns klogd's feeding off for a
/// moment, so all CPUs would draw the real console.
fn storm_without_klogd() -> TestResult {
    let id = klog::add_deferred_sink("ktest-slow", &SLOW, false);
    let r = (|| {
        let _back = NoDrainer;
        klog::set_drainer(false);
        let s = klog::ktests::with_trace("rbstorm", storm)??;
        let lines = check_ring(&s)?;
        // What the last feeder left (bounded rounds) goes to the next emitter, or to klogd once it is back.
        drop(_back);
        let ms = wait_sink(lines.len(), time::now_ns())?;
        kassert!(SLOW.lost.load(Ordering::Relaxed) == 0, "the sink was told of {} lost records", SLOW.lost.load(Ordering::Relaxed));
        let got = SLOW.lines.lock().clone();
        kassert!(got == lines, "the sink's lines differ from the ring's order");
        report("emitters", &s, ms);
        Ok(())
    })();
    klog::remove_sink(id);
    r
}

fn smp_storm_early() -> TestResult {
    tries(storm_without_klogd)
}
ktest!("rb.klog.smp_storm_early", QemuOnly, smp_storm_early);
