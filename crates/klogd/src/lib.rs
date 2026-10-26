//! The klog service (owner: RB, item RB-1): what klog itself cannot do because `sched` and `mcp` depend on it.
//!
//! - `klog.size` (Core init): sizes the ring from the machine's memory (`klog::size_for_memory`; CONFIG
//!   `log_buf=` wins).
//! - `klog.drain` (Core init, RB-klog; was Service): the `klogd` thread feeds the deferred sinks (framebuffer
//!   console, the mirror of RB-4) and prints the serial lines a storm left, so the code that logs never waits
//!   for a slow sink. Before it starts (and if it cannot start) the emitters feed them in order
//!   (`klog::set_drainer`). It also reports klog's own health (`health`: lost records, slow ring holds, the
//!   `boot.stats` line 10 s after boot).
//! - MCP tool `log` (`tool.rs`): records by sequence number with filters.
//! - RB-7a: the per-CPU trace rings are sized here (`klog.size`); klogd samples IRQ storms (`irqwatch`),
//!   reports rate-limited records once a second and grows klog's key table; MCP tool `events` (`events.rs`).
//!
//! - The console mirror (`mirror`, RB-4): every line as UDP to the listener CONFIG `mirror=` names, buffered
//!   from boot until the network is up, with an emergency path through `NetDevice::poll_tx` for panics.
//!
//! - RB-7b: the metric sampler (thread `metrics`, every 2 s) and the MCP tool `metrics` (`metrics/`).
//!
//! - RB-8: `/dev/bitsos/events` (`devevents`), the reader of the journal service bitsjournal; MCP `events` and
//!   `metrics` forward `since`/`until` (wall time) to its tool `journal` (`journal`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod devevents;
pub mod events;
mod health;
pub mod irqwatch;
mod journal_ktests;
mod ktests;
pub mod metrics;
mod metrics_ktests;
pub mod mirror;
pub mod journal;
mod tool;
mod trace_ktests;

use core::sync::atomic::{AtomicBool, Ordering};
use kreg::Level;

/// Idle poll of the drain thread. Waking it from `emit` is not possible (emit may run under the scheduler's
/// own lock), so it looks every few ms; a poll is one atomic compare per sink.
const DRAIN_POLL_MS: u64 = 10;
/// Records per sink and round, so one busy sink cannot starve the others.
const DRAIN_BUDGET: usize = 512;
/// How often klogd reports rate-limited records and grows klog's key table (RB-7a).
const SWEEP_NS: u64 = 1_000_000_000;

static DRAIN_RUNNING: AtomicBool = AtomicBool::new(false);

pub fn drain_running() -> bool {
    DRAIN_RUNNING.load(Ordering::Acquire)
}

fn size_ring() -> Result<(), &'static str> {
    let total = mm_core::frames().map(|f| f.stats().total_bytes).ok_or("no frame allocator")?;
    klog::size_for_memory(total);
    klog::trace::init(total);
    Ok(())
}
kreg::initcall!(Level::Core, "klog.size", size_ring);

fn drain_loop() {
    DRAIN_RUNNING.store(true, Ordering::Release);
    ksync::set_slow_wait_hook(klog::trace::lock_wait);
    klog::event!(Info, "klog", "drain.start", poll_ms = DRAIN_POLL_MS);
    let mut watch = health::Watch::default();
    let mut irqs = irqwatch::Sampler::default();
    let mut swept = 0;
    klog::trace::set_tsc_hz(time::tsc_hz());
    loop {
        let now = time::now_ns();
        irqs.step(now);
        if now.saturating_sub(swept) >= SWEEP_NS {
            swept = now;
            klog::ratelimit::sweep();
            klog::keys::grow();
        }
        // Serial lines a storm left (the emitters print only a bounded share once klogd runs), then the sinks.
        let printed = klog::flush_serial(DRAIN_BUDGET);
        let written = if klog::pending() { klog::drain_deferred(DRAIN_BUDGET) } else { 0 };
        if written > 0 {
            #[cfg(target_os = "none")]
            mirror::kick();
        }
        if let Some(r) = watch.step(time::now_ns(), klog::lost(), klog::lock_stats()) {
            health::emit(r);
        }
        if printed + written > 0 {
            sched::yield_now();
        } else if klog::pending() || klog::serial_pending() {
            // Work left but no progress (the ring or the printer busy elsewhere): look again soon.
            sched::sleep_ms(1);
        } else {
            sched::sleep_ms(DRAIN_POLL_MS);
        }
    }
}

fn start_drain() -> Result<(), &'static str> {
    // Normal, not Background: the console and the mirror are how Georg sees the machine.
    match sched::spawn("klogd", sched::Class::Normal, drain_loop) {
        Ok(_) => {
            // From now on the emitters leave the deferred sinks to klogd (until here they fed them in order).
            klog::set_drainer(true);
            Ok(())
        }
        Err(e) => {
            klog::event!(Error, "klog", "drain.spawn_failed", error = alloc::format!("{e:?}"); "the emitters keep feeding the deferred sinks");
            Err("klogd thread")
        }
    }
}
// RB-klog: Core, right after `cpu.smp` and before `sched.smp` (names sort within a level), so klogd draws
// before any AP thread runs. Until then only the boot CPU logs, and it feeds the screen itself (in order, from
// the ring). Before, klogd came at Service: from `sched.smp` to then all CPUs drew the screen from their own
// emit calls, also with interrupts off, and overtook each other (false `[... n records lost]`).
kreg::initcall!(Level::Core, "klog.drain", start_drain);
