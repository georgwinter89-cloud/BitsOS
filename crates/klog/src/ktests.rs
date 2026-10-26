//! In-kernel tests of klog (RB-1). Class Safe: they only log and read the ring.

use crate::{Filter, Level, Record, Sink, read};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use ksync::SpinLock;
use ktest::{TestResult, kassert, ktest};

fn find<'a>(recs: &'a [Record], name: &str) -> Option<&'a Record> {
    recs.iter().find(|r| r.name == name)
}

/// Runs `f` with `subsystem` at trace, whatever CONFIG `loglevel=` says, and without rate limits (RB-7a: the
/// storm tests log thousands of records of one event on purpose), then restores the settings.
pub fn with_trace<R>(subsystem: &str, f: impl FnOnce() -> R) -> Result<R, String> {
    let before = crate::levels();
    crate::set_levels(&alloc::format!("{before},{subsystem}=trace"))?;
    let unlimited = crate::ratelimit::suspend();
    let r = f();
    drop(unlimited);
    crate::set_levels(&before)?;
    Ok(r)
}

/// The records emitted before the heap existed are in the heap ring, with their sequence numbers intact. The
/// move itself is checked from `early_move` (independent of how much the boot logged since); the records are
/// checked too while the ring still holds them.
fn early_survives() -> TestResult {
    let m = crate::early_move().ok_or("heap_ready did not move the early ring")?;
    kassert!(m.records > 0 && m.moved == m.records && m.oldest_after == m.oldest, "{:?}", m);
    let info = crate::ring_info();
    kassert!(!info.early, "still on the early ring: {:?}", info);
    kassert!(info.bytes >= crate::RING_MIN, "ring of {} bytes", info.bytes);
    let page = read(m.oldest, 4096, &Filter::ALL);
    if page.oldest > m.oldest {
        // A chatty boot overwrote them since; the move above is the proof.
        return Ok(());
    }
    let early = find(&page.records, "ring.early").ok_or("ring.early missing")?;
    let ready = find(&page.records, "ring.ready").ok_or("ring.ready missing")?;
    kassert!(early.subsystem == "klog" && early.seq < ready.seq);
    // Every record up to ring.early came from the static ring; none of them may be missing.
    let before: Vec<u64> = page.records.iter().map(|r| r.seq).take_while(|&s| s <= early.seq).collect();
    kassert!(before.len() as u64 == early.seq + 1 - m.oldest, "early records lost: kept {:?}", before);
    kassert!(page.gaps.iter().all(|g| g.before > early.seq), "gap in the early records: {:?}", page.gaps);
    Ok(())
}
ktest!("rb.klog.early_ring_survives", Safe, early_survives);

/// The ring is held with interrupts masked, so an IRQ handler that logs never meets its own CPU holding it
/// (only NMI/#MC can, and they use try_event!).
fn ring_masks_irqs() -> TestResult {
    if !hal::cpu::interrupts_enabled() {
        return ktest::skip("interrupts are off in the test runner, nothing to mask");
    }
    let inside = crate::__with_ring_held(hal::cpu::interrupts_enabled);
    kassert!(!inside, "interrupts were on while the ring was held");
    kassert!(hal::cpu::interrupts_enabled(), "interrupts not restored");
    // And a normal event right after goes through.
    let s = crate::next_seq();
    crate::event!(Warn, "rbktest", "after.mask");
    kassert!(find(&read(s, 8, &Filter::ALL).records, "after.mask").is_some());
    Ok(())
}
ktest!("rb.klog.ring_masks_irqs", Safe, ring_masks_irqs);

/// try_event! on a busy ring returns at once, the record still counts as a hole for readers.
fn try_event_busy() -> TestResult {
    let start = crate::next_seq();
    let stored = crate::__with_ring_held(|| {
        let s = crate::try_event!(Error, "rbktest", "nmi.path"; "ring busy");
        crate::event!(Warn, "rbktest", "irq.path");
        s
    });
    kassert!(!stored);
    // SC-3: another CPU may hold the ring for a moment; the ring is free again within a few tries.
    let mut ok = false;
    for _ in 0..1000 {
        if crate::try_event!(Error, "rbktest", "nmi.free") {
            ok = true;
            break;
        }
        core::hint::spin_loop();
    }
    kassert!(ok);
    let page = read(start, 16, &Filter { subsystem: Some("rbktest"), ..Filter::ALL });
    kassert!(page.missed() >= 2, "gaps {:?}", page.gaps);
    kassert!(find(&page.records, "nmi.free").is_some());
    Ok(())
}
ktest!("rb.klog.try_event_busy_ring", Safe, try_event_busy);

fn levels() -> TestResult {
    let before = crate::levels();
    let start = crate::next_seq();
    crate::set_levels("info,rbklv=debug")?;
    crate::event!(Debug, "rbklv.x", "kept");
    crate::event!(Debug, "rbklother", "dropped");
    let r = crate::set_levels(&before);
    kassert!(r.is_ok(), "restore {}: {:?}", before, r);
    let names: Vec<_> = read(start, 64, &Filter::ALL).records.iter().filter(|r| r.subsystem.starts_with("rbkl")).map(|r| r.name).collect();
    kassert!(names == ["kept"], "{:?}", names);
    kassert!(crate::min_level() >= Level::Info);
    Ok(())
}
ktest!("rb.klog.levels_per_subsystem", Safe, levels);

struct Lines(SpinLock<Vec<String>>);
impl Sink for Lines {
    fn write_line(&self, _l: Level, line: &str) {
        self.0.lock().push(line.to_string());
    }
}
static DEFERRED: Lines = Lines(SpinLock::new(Vec::new()));

/// A deferred sink gets its lines from the drain, never in the emitter's context, once a drainer (klogd) runs;
/// before that (or in a kernel without klogd) the emitter feeds it in order.
fn deferred_sink() -> TestResult {
    let id = crate::add_deferred_sink("ktest", &DEFERRED, false);
    let drainer = crate::drainer();
    crate::event!(Warn, "rbktest", "deferred.line", n = 7u32);
    let inline = DEFERRED.0.lock().iter().any(|l| l.ends_with("W rbktest deferred.line n=7"));
    crate::drain_deferred(256);
    // SC-3: klogd may be delivering the line on another CPU right now (a second drainer returns at once).
    let t0 = crate::now();
    while !DEFERRED.0.lock().iter().any(|l| l.ends_with("W rbktest deferred.line n=7")) && crate::now() - t0 < 2_000_000_000 {
        crate::drain_deferred(256);
        core::hint::spin_loop();
    }
    let got = core::mem::take(&mut *DEFERRED.0.lock());
    crate::remove_sink(id);
    // With klogd running it may deliver the line before the check above, so "inline" proves nothing then;
    // the host tests check the switch exactly. Here: exactly one copy, and fed at once without a drainer.
    let copies = got.iter().filter(|l| l.ends_with("W rbktest deferred.line n=7")).count();
    kassert!(copies == 1, "{} copies: {:?}", copies, got);
    kassert!(drainer || inline, "no drainer, but the emitter did not feed the sink");
    Ok(())
}
ktest!("rb.klog.deferred_sink", Safe, deferred_sink);

fn filters() -> TestResult {
    let start = crate::next_seq();
    with_trace("rbkf", || {
        crate::event!(Warn, "rbkf", "a.one");
        crate::event!(Info, "rbkf.sub", "b.two");
    })?;
    let warn = read(start, 16, &Filter { level: Some(Level::Warn), subsystem: Some("rbkf"), ..Filter::ALL });
    kassert!(warn.records.len() == 1 && warn.records[0].name == "a.one");
    let byname = read(start, 16, &Filter { name: Some("b."), ..Filter::ALL });
    kassert!(byname.records.len() == 1 && byname.records[0].subsystem == "rbkf.sub");
    kassert!(byname.next <= crate::next_seq());
    Ok(())
}
ktest!("rb.klog.read_filters", Safe, filters);
