//! In-kernel tests of RB-7b: the sources read real numbers into a private store, the running sampler fills
//! the global one, the `metrics` tool answers in its shape. All Safe and rerunnable: nothing global changes.

use crate::metrics::sampler::{Sampler, token};
use crate::metrics::store::{Store, Unit};
use crate::metrics::{PERIOD_NS, sources::Sources, with_store};
use crate::tool::call_tool;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ktest::{TestResult, kassert, ktest};

fn latest(s: &Store, name: &str) -> Option<(Unit, u64)> {
    s.series().find(|x| x.0 == name).and_then(|x| Some((x.1.unit, x.1.latest()?)))
}

fn registered() -> TestResult {
    let t = mcp::find("metrics").ok_or("metrics tool missing")?;
    kassert!(t.owner == "RB" && t.safety == mcp::Safety::Read);
    kassert!(mcp::Json::parse(t.input_schema).is_ok());
    Ok(())
}
ktest!("rb.metrics.tool_registered", Safe, registered);

/// Two real readings 200 ms apart give every source's series, with sane values.
fn sources() -> TestResult {
    let src = Sources::detect();
    let mut store = Store::new(1 << 20);
    let mut m = Sampler::default();
    let r1 = src.read(klog::now(), 0);
    m.step(&r1, &mut store);
    sched::sleep_ms(200);
    let r2 = src.read(klog::now(), 1000);
    m.step(&r2, &mut store);
    let cpu = sched::cpu_stats().into_iter().find(|c| c.online).ok_or("no online CPU")?.cpu;
    for what in ["busy", "user", "system"] {
        let (u, v) = latest(&store, &format!("cpu.{cpu}.{what}")).ok_or_else(|| format!("no cpu.{cpu}.{what}"))?;
        kassert!(u == Unit::Pct && v <= 10_000, "cpu {what} {v}");
    }
    kassert!(latest(&store, "cpu.all.busy").is_some_and(|(_, v)| v <= 10_000));
    let total = mm_core::frames().map(|f| f.stats().total_bytes).ok_or("no frames")?;
    kassert!(latest(&store, "mem.total") == Some((Unit::Bytes, total)));
    kassert!(latest(&store, "mem.free").is_some_and(|(_, v)| v <= total));
    kassert!(latest(&store, "mem.used.heap").is_some_and(|(_, v)| v > 0), "the heap uses frames");
    if let Some((name, _)) = klog::metrics::snapshot().next() {
        kassert!(latest(&store, name).is_some_and(|(u, _)| u == Unit::Count), "counter {name} missing");
    }
    // Cards and disks in both readings have rates; one that came up in between only its levels.
    for d in r2.nets.iter().filter(|d| r1.nets.iter().any(|e| e.name == d.name)) {
        let n = format!("net.{}.rx_bytes", token(&d.name));
        kassert!(latest(&store, &n).is_some_and(|(u, _)| u == Unit::BytesPerS), "no {n}");
    }
    for d in &r2.disks {
        let n = format!("disk.{}.queued", token(&d.name));
        kassert!(latest(&store, &n).is_some(), "no {n}");
        if r1.disks.iter().any(|e| e.name == d.name) {
            let n = format!("disk.{}.requests", token(&d.name));
            kassert!(latest(&store, &n).is_some_and(|(u, _)| u == Unit::PerS), "no {n}");
        }
    }
    kassert!(latest(&store, "metrics.cost").is_some());
    Ok(())
}
ktest!("rb.metrics.sources", Safe, sources);

/// Waits (bounded) until the global store has `n` rounds.
fn rounds_at_least(n: u64) -> Result<u64, String> {
    let until = time::now_ns() + 5 * PERIOD_NS;
    loop {
        let have = with_store(1_000_000_000, |s| s.rounds().end - s.rounds().start).ok_or("metrics off or busy")?;
        if have >= n {
            return Ok(have);
        }
        if time::now_ns() > until {
            return Err(format!("the sampler made {have} rounds in {} s", 5 * PERIOD_NS / 1_000_000_000));
        }
        sched::sleep_ms(200);
    }
}

/// The running sampler fills the global store, and the tool shows it in its shape.
fn tool_answer() -> TestResult {
    rounds_at_least(2)?;
    let a = call_tool("metrics", r#"{"name":"cpu.all.busy,mem.total","last":3600,"max":5}"#).map_err(|e| format!("{e:?}"))?;
    let lines: Vec<&str> = a.lines().collect();
    kassert!(lines.len() == 4, "{a}");
    kassert!(lines[0].starts_with("metrics from=") && lines[0].contains(" period_s=2 ") && lines[0].contains(" series=2 more=0"), "{}", lines[0]);
    let points = lines[1].split(' ').count() - 1;
    kassert!(lines[1].starts_with("time ") && (1..=5).contains(&points), "{}", lines[1]);
    kassert!(lines[2].starts_with("cpu.all.busy % ") && lines[2].split(' ').count() == points + 2, "{}", lines[2]);
    kassert!(lines[3].starts_with("mem.total B ") && !lines[3].ends_with(" -"), "{}", lines[3]);
    kassert!(matches!(call_tool("metrics", r#"{"agg":"sum"}"#), Err(mcp::ToolError::BadArgument(ref a)) if a == "agg"));
    let all = call_tool("metrics", "{}").map_err(|e| format!("{e:?}"))?;
    kassert!(all.lines().any(|l| l.starts_with("metrics.cost % ")), "{all}");
    kassert!(all.lines().any(|l| l.starts_with("cpu.all.busy % ")), "{all}");
    // The largest question (every series, 2000 points) stays inside the answer cap.
    let big = call_tool("metrics", r#"{"max":2000,"last":1e9}"#).map_err(|e| format!("{e:?}"))?;
    kassert!(big.len() <= crate::metrics::tool::MAX_ANSWER_BYTES, "{} bytes", big.len());
    kassert!(big.starts_with("metrics from="));
    Ok(())
}
ktest!("rb.metrics.tool_answer", Safe, tool_answer);

/// k10temp sensors on AMD Zen (the Mini-PC): Tctl and every CCD in a plausible range. QEMU has none. The
/// running sampler may hold the SMN pair at the same moment: a busy probe or read is tried again (bounded).
fn temps() -> TestResult {
    use crate::metrics::temp::{Probe, Zen};
    let until = time::now_ns() + 1_000_000_000;
    let z = loop {
        match Zen::detect() {
            Ok(z) => break z,
            Err(Probe::Absent(why)) => return ktest::skip(why),
            Err(Probe::Again) if time::now_ns() < until => sched::sleep_ms(1),
            Err(Probe::Again) => return Err("SMN busy for 1 s".into()),
        }
    };
    let t = loop {
        let t = z.read();
        if t.len() == z.names().count() || time::now_ns() >= until {
            break t;
        }
        sched::sleep_ms(1);
    };
    kassert!(t.first().is_some_and(|(n, _)| n == "tctl"), "no tctl");
    for (name, mc) in &t {
        kassert!((15_000..=105_000).contains(mc), "{name} = {mc} m°C");
    }
    klog::event!(Info, "rb", "ktest.temps", values = t.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join(","));
    Ok(())
}
ktest!("rb.metrics.temps", Safe, temps);

/// RAPL package power over 500 ms between 0.1 and 200 W (Mini-PC; QEMU has no RAPL).
fn rapl() -> TestResult {
    let src = Sources::detect();
    if let Err(why) = src.esu {
        return ktest::skip(why);
    }
    let mut store = Store::new(1 << 16);
    let mut m = Sampler::default();
    m.step(&src.read(klog::now(), 0), &mut store);
    sched::sleep_ms(500);
    m.step(&src.read(klog::now(), 0), &mut store);
    let (_, mw) = latest(&store, "power.pkg").ok_or("no power.pkg")?;
    kassert!((100..=200_000).contains(&mw), "power.pkg {mw} mW");
    klog::event!(Info, "rb", "ktest.rapl", mw = mw);
    Ok(())
}
ktest!("rb.metrics.rapl", Safe, rapl);
