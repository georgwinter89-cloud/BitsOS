//! Sinks: where rendered lines go besides the ring and serial.
//!
//! - Inline sinks (`add_sink`, the G0 contract) run in the emitter's context and must be quick.
//! - Deferred sinks (`add_deferred_sink`) are fed from the ring by `drain_deferred`, which the klogd thread
//!   runs; the emitter never waits for them, so a slow sink (framebuffer scroll, the network mirror of RB-4)
//!   cannot slow down or block the code that logs. A deferred sink that falls behind the ring gets one
//!   `Sink::lost(n)` call (default: the line `[... n records lost]`; RB-4's mirror overrides it to send
//!   console-listen.mjs's own `[... n bytes lost]` marker, spec net.md 5.3).
//! - Until a drainer runs (`set_drainer(true)`, klogd's initcall), deferred sinks are fed by the emitters, in
//!   sequence order straight from the ring (`catch_up`): the framebuffer console must show the boot lines at
//!   once (spec core.md 10.3), not when the scheduler is up. RB-klog: one CPU at a time feeds (the others leave
//!   their records to it and go on), and only from a context with interrupts on and no spin lock held, so no
//!   IRQ handler or lock holder ever draws the screen or waits for a CPU that does. The old way (each emitter
//!   handed its own line to the sink) let two CPUs overtake each other: the later record moved the cursor past
//!   the earlier one, which was dropped and reported as `[... 1 records lost]` (seen on the Mini-PC at boot).
//!   A sink registered with `replay` in that phase gets the records before it right away.
//! - Every sink may also take lines on the panic/NMI path through `Sink::write_emergency` (no locks, no
//!   allocation; the mirror sends them by polled TX).
//!
//! The list is append-only and walked without a lock: nodes are leaked once and chained through `OnceRef`, so
//! a walk from an NMI never waits for anyone.

use crate::reader::{self, Filter};
use crate::{Level, Sink};
use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::OnceRef;

pub struct SinkNode {
    name: &'static str,
    sink: &'static dyn Sink,
    deferred: bool,
    /// Deferred sinks: the next sequence number this sink has not seen.
    cursor: AtomicU64,
    removed: AtomicBool,
    /// CPU tag while an inline sink runs, so a nested emit on that CPU skips it instead of recursing.
    busy: AtomicUsize,
    next: OnceRef<SinkNode>,
}

/// A handle to a registered sink (to remove it again, e.g. when the mirror target changes).
#[derive(Clone, Copy)]
pub struct SinkId(&'static SinkNode);

static HEAD: OnceRef<SinkNode> = OnceRef::new();
static DRAINING: AtomicBool = AtomicBool::new(false);
static DRAINER: AtomicBool = AtomicBool::new(false);
/// Rounds of `CATCH_UP_BUDGET` records per sink that `add_deferred_sink` and an emitter catch up at most (bounded
/// even if other CPUs log fast; what is left goes to the next emitter or to klogd).
const SYNC_ROUNDS: usize = 64;
const EMIT_ROUNDS: usize = 4;
const CATCH_UP_BUDGET: usize = 512;

/// klogd says it feeds the deferred sinks from now on (true), or no longer (false: the emitter feeds them).
pub fn set_drainer(running: bool) {
    if DRAINER.swap(running, Ordering::AcqRel) != running {
        crate::event!(Info, "klog", "drainer", running = running);
    }
}

/// True while a drainer (klogd) feeds the deferred sinks.
pub fn drainer() -> bool {
    DRAINER.load(Ordering::Acquire)
}

fn append(node: SinkNode) -> SinkId {
    let node: &'static SinkNode = Box::leak(Box::new(node));
    if HEAD.set(node) {
        return SinkId(node);
    }
    let mut cur = HEAD.get().unwrap_or(node);
    loop {
        match cur.next.get() {
            Some(n) => cur = n,
            None if cur.next.set(node) => return SinkId(node),
            None => {}
        }
    }
}

fn iter() -> impl Iterator<Item = &'static SinkNode> {
    let mut cur = HEAD.get();
    core::iter::from_fn(move || {
        let n = cur?;
        cur = n.next.get();
        Some(n)
    })
}

fn node(name: &'static str, sink: &'static dyn Sink, deferred: bool, cursor: u64) -> SinkNode {
    SinkNode {
        name,
        sink,
        deferred,
        cursor: AtomicU64::new(cursor),
        removed: AtomicBool::new(false),
        busy: AtomicUsize::new(0),
        next: OnceRef::new(),
    }
}

/// G0 contract: an inline sink, called in the emitter's context (needs the heap).
pub fn add_sink(s: &'static dyn Sink) {
    append(node("inline", s, false, 0));
}

/// A sink fed by the klogd thread. `replay`: start with the oldest record still kept (the mirror wants the
/// whole boot), else with the next record.
pub fn add_deferred_sink(name: &'static str, s: &'static dyn Sink, replay: bool) -> SinkId {
    let start = if replay { 0 } else { crate::next_seq() };
    let id = append(node(name, s, true, start));
    if replay && !drainer() {
        // No drainer yet: catch this sink up now (the caller is the boot, a normal context); then the emitters
        // feed it. If another CPU feeds right now, it takes this sink along.
        catch_up(SYNC_ROUNDS);
    }
    crate::event!(Info, "klog", "sink.added", sink = name, replay = replay);
    id
}

/// Stops feeding a sink (the node itself stays, it is a few bytes).
pub fn remove_sink(id: SinkId) {
    id.0.removed.store(true, Ordering::Release);
}

/// The emitter's part: inline sinks always; while no drainer runs also the deferred sinks, from the ring in
/// order (`catch_up`), if this context may draw.
pub(crate) fn inline(level: Level, line: &str) {
    {
        // The thread stays on this CPU while it owns a sink's `busy` tag (a move would leave the tag naming a
        // CPU it no longer runs on, and that CPU's lines would skip the sink; review INT-A1d core 2). Not across
        // `catch_up` below, which needs `may_feed` (preempt count 0).
        let _pg = ksync::PreemptGuard::new();
        let me = crate::cpu_tag();
        for n in iter().filter(|n| !n.deferred && !n.removed.load(Ordering::Acquire)) {
            if n.busy.load(Ordering::Acquire) == me {
                continue;
            }
            n.busy.store(me, Ordering::Release);
            n.sink.write_line(level, line);
            let _ = n.busy.compare_exchange(me, 0, Ordering::AcqRel, Ordering::Relaxed);
        }
    }
    if !drainer() && may_feed() {
        catch_up(EMIT_ROUNDS);
    }
}

/// A deferred sink (the screen, the mirror) may be fed here: interrupts on and no spin lock held, so its slow
/// drawing never runs in an IRQ handler or under someone's lock, where other CPUs would spin for it.
fn may_feed() -> bool {
    crate::lock::irqs_on() && ksync::preempt_count() == 0
}

/// Feeds every deferred sink from its cursor, in sequence order, from the ring. One caller at a time (the
/// `DRAINING` flag; klogd's `drain_deferred` takes it too): a second caller returns at once, and the holder looks
/// again after letting go, so a record stored meanwhile is not left behind.
fn catch_up(rounds: usize) {
    for _ in 0..rounds {
        // SeqCst on both sides (the flag here, the sequence number in `record`): a CPU that stored a record and
        // then found the flag taken knows the holder sees its record in the `pending` check below.
        if DRAINING.swap(true, Ordering::SeqCst) {
            return;
        }
        for n in iter().filter(|n| n.deferred && !n.removed.load(Ordering::Acquire)) {
            drain_node(n, CATCH_UP_BUDGET);
        }
        DRAINING.swap(false, Ordering::SeqCst);
        if drainer() || !pending() {
            return;
        }
    }
}

pub(crate) fn emergency(level: Level, line: &str) {
    for n in iter().filter(|n| !n.removed.load(Ordering::Acquire)) {
        n.sink.write_emergency(level, line);
    }
}

pub(crate) fn flush_emergency() {
    for n in iter().filter(|n| !n.removed.load(Ordering::Acquire)) {
        n.sink.flush_emergency();
    }
}

/// True when a deferred sink has records it has not seen.
pub fn pending() -> bool {
    let next = crate::next_seq();
    iter().any(|n| n.deferred && !n.removed.load(Ordering::Acquire) && n.cursor.load(Ordering::Acquire) < next)
}

/// Feeds every deferred sink up to `budget` records each; returns the lines written. Only one drainer runs
/// at a time (a second caller returns 0 at once).
pub fn drain_deferred(budget: usize) -> usize {
    // Serial too (its lines are not counted): fch-wdt and kexec call this right before a reset or a jump.
    crate::serial::flush(None, budget);
    if DRAINING.swap(true, Ordering::SeqCst) {
        return 0;
    }
    let mut written = 0;
    for n in iter().filter(|n| n.deferred && !n.removed.load(Ordering::Acquire)) {
        written += drain_node(n, budget);
    }
    DRAINING.swap(false, Ordering::SeqCst);
    written
}

/// Feeds one deferred sink up to `budget` records from its cursor (caller holds `DRAINING`). A busy ring
/// leaves the cursor after what was delivered; the next round goes on from there.
fn drain_node(n: &SinkNode, budget: usize) -> usize {
    let mut written = 0;
    let page = reader::read(n.cursor.load(Ordering::Acquire), budget, &Filter::ALL);
    let mut gaps = page.gaps.iter().peekable();
    for r in &page.records {
        while let Some(g) = gaps.next_if(|g| g.before <= r.seq) {
            n.sink.lost(g.count);
            written += 1;
        }
        n.sink.write_line(r.level, &r.render());
        written += 1;
    }
    for g in gaps {
        n.sink.lost(g.count);
        written += 1;
    }
    n.cursor.fetch_max(page.next, Ordering::AcqRel);
    written
}

/// (name, deferred, cursor) of every active sink, for `ring_info` and the `log` tool.
pub fn list() -> alloc::vec::Vec<(&'static str, bool, u64)> {
    iter().filter(|n| !n.removed.load(Ordering::Acquire)).map(|n| (n.name, n.deferred, n.cursor.load(Ordering::Relaxed))).collect()
}
