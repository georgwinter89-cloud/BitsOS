//! MM-6's kernel-side glue for the TLB shootdown in `hal::paging::tlb`: set-up before the APs start, the
//! events for what hal can only count (hal runs in IRQ context and under the switch, B6), and the retry of
//! vmap ranges that a shootdown could not confirm.

use core::sync::atomic::{AtomicBool, Ordering};
use hal::paging::tlb::{self, TlbStats};
use ksync::SpinLock;

static STARTED: AtomicBool = AtomicBool::new(false);

/// Registers the shootdown vector and the clock. Runs before the APs start (from `kernel_tables`, which the
/// kernel calls before the init levels, or from the Core initcall below); later calls change nothing.
pub(crate) fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Err(e) = tlb::init(Some(time::now_ns)) {
        klog::event!(Error, "mm", "tlb.vector_taken", vector = klog::Hex(tlb::VECTOR as u64), error = alloc::format!("{e:?}");
            "no TLB shootdown: unmaps on several CPUs keep what they free out of use");
    }
}

fn ready() -> Result<(), &'static str> {
    start();
    // `start` may have run before ACPI knew the CPUs; create every CPU's TLB state here in thread context so
    // no AP grows it from its IPI handler (CP-4 should call this before the SIPIs; see the MM-6 notes).
    tlb::prepare_cpus();
    let s = tlb::stats();
    klog::event!(Info, "mm", "tlb.ready", ready = s.ready, vector = klog::Hex(tlb::VECTOR as u64), pcid = s.pcid, invpcid = s.invpcid,
        resend_ms = tlb::RESEND_NS / 1_000_000, resend_max_ms = tlb::RESEND_MAX_NS / 1_000_000, give_up_ms = tlb::GIVE_UP_NS / 1_000_000, range_pages = tlb::RANGE_PAGES;
        "TLB shootdown: CPU masks, generations, PCID per CPU, lazy TLB");
    if s.ready { Ok(()) } else { Err("TLB shootdown vector not registered") }
}
kreg::initcall!(kreg::Level::Core, "mm.tlb", ready);

klog::counter!(pub ROUNDS, "mm.tlb.rounds");
klog::counter!(pub SLOW_ROUNDS, "mm.tlb.slow_rounds");
klog::counter!(pub RESENT_ROUNDS, "mm.tlb.resent_rounds");
klog::counter!(pub RESENDS, "mm.tlb.resends");

/// The CPU that took longest from a shootdown IPI's send to its answer since the last call, and that time in
/// ns (None: no CPU was asked, or the TSC rate is unknown).
fn slowest_answer() -> Option<(usize, u64)> {
    let hz = time::tsc_hz();
    (0..hal::percpu::cpu_count())
        .map(|c| (c, tlb::ticks_to_ns(tlb::take_answer_max(c), hz)))
        .filter(|&(_, ns)| ns > 0)
        .max_by_key(|&(_, ns)| ns)
}

/// What the last report saw, and what the last `tlb.stats` event showed.
static SEEN: SpinLock<(Option<TlbStats>, Option<TlbStats>)> = SpinLock::named("mm.tlb.seen", (None, None));
static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Report periods between two `tlb.slow` events.
const SLOW_EVERY: u64 = 10;

/// Slow rounds not reported yet (since report period `since`) and the slowest among them.
struct SlowSum {
    since: u64,
    slow: u64,
    resent_once: u64,
    rounds: u64,
    wait_us: u64,
    last_cpu: i64,
    answer_cpu: i64,
    answer_us: u64,
}

impl SlowSum {
    const EMPTY: SlowSum = SlowSum { since: 0, slow: 0, resent_once: 0, rounds: 0, wait_us: 0, last_cpu: -1, answer_cpu: -1, answer_us: 0 };
}

static SLOW: SpinLock<SlowSum> = SpinLock::named("mm.tlb.slow", SlowSum::EMPTY);
/// Report periods between two `tlb.stats` events (when anything moved).
const STATS_EVERY: u64 = 60;

/// Called once a second by the `mm.report` thread (no lock held): events for resends, give-ups and kept
/// frames since the last call, a statistics event once a minute, and the retry of quarantined vmap ranges.
pub(crate) fn report() {
    let now = tlb::stats();
    let before = SEEN.lock().0.replace(now).unwrap_or_default();
    ROUNDS.add(now.rounds.saturating_sub(before.rounds));
    SLOW_ROUNDS.add(now.slow.saturating_sub(before.slow));
    RESENT_ROUNDS.add((now.resent_once + now.several).saturating_sub(before.resent_once + before.several));
    RESENDS.add(now.resends.saturating_sub(before.resends));
    // Read every second, so each event names the slowest round and CPU of its own second.
    let slowest = tlb::take_slowest();
    let late = slowest_answer();
    let (wait_us, last_cpu) = slowest.map_or((0, -1), |(ns, c)| (ns / 1000, c as i64));
    let (answer_cpu, answer_us) = late.map_or((-1, 0), |(c, ns)| (c as i64, ns / 1000));
    if now.several > before.several {
        klog::event!(Warn, "mm", "tlb.resend", rounds = now.several - before.several, resends = now.resends - before.resends,
            wait_us = wait_us, last_cpu = last_cpu, answer_cpu = answer_cpu, answer_us = answer_us, total = now.several, stuck_cpus = now.stuck_cpus;
            "a CPU did not answer a TLB shootdown through several resends (2 ms, then doubling); the round waited wait_us");
    } else {
        // Slow rounds are summed and reported at most every `SLOW_EVERY` periods (Info; the console shows Info,
        // and on a machine with deep C-states one slow round every few seconds is normal).
        let mut a = SLOW.lock();
        a.slow += now.slow - before.slow;
        a.resent_once += now.resent_once - before.resent_once;
        a.rounds += now.rounds - before.rounds;
        if now.slow + now.resent_once > before.slow + before.resent_once && wait_us >= a.wait_us {
            (a.wait_us, a.last_cpu, a.answer_cpu, a.answer_us) = (wait_us, last_cpu, answer_cpu, answer_us);
        }
        let t = TICKS.load(Ordering::Relaxed);
        if a.slow + a.resent_once > 0 && t.saturating_sub(a.since) >= SLOW_EVERY {
            let x = core::mem::replace(&mut *a, SlowSum { since: t, ..SlowSum::EMPTY });
            drop(a);
            klog::event!(Info, "mm", "tlb.slow", slow = x.slow, resent_once = x.resent_once, wait_us = x.wait_us, last_cpu = x.last_cpu,
                answer_cpu = x.answer_cpu, answer_us = x.answer_us, rounds = x.rounds, period_s = t - x.since;
                "TLB shootdowns waited 1 ms or more for a CPU (at most one resend) in period_s; every CPU answered");
        } else if a.slow + a.resent_once == 0 {
            a.since = t;
            a.rounds = 0;
        }
    }
    if let Some(s) = tlb::take_late_sample() {
        klog::event!(Warn, "mm", "tlb.late_cpu", cpu = s.cpu, waited_us = s.waited_ns / 1000, rip = klog::Hex(s.rip),
            interrupts_on = s.interrupts_on, user = s.user, trace = alloc::format!("{:x?}", &s.trace[..s.ntrace]);
            "where a CPU was that had not answered a TLB shootdown for 8 ms (NMI sample; interrupts_on=false: it ran with interrupts off there; resolve rip/trace with boot/kernel.syms)");
    }
    if now.give_ups > before.give_ups {
        klog::event!(Error, "mm", "tlb.give_up", shootdowns = now.give_ups - before.give_ups, cpus = now.unanswered - before.unanswered,
            not_ready = now.not_ready - before.not_ready, stuck_cpus = now.stuck_cpus;
            "a CPU did not answer a TLB shootdown at all; what that change freed stays out of use");
    }
    if now.send_failed > before.send_failed {
        klog::event!(Warn, "mm", "tlb.send_failed", ipis = now.send_failed - before.send_failed, total = now.send_failed;
            "a TLB shootdown IPI could not be sent (no APIC id or the APIC refused); the initiator waits and resends");
    }
    if now.pcid_no_pge + now.pcid_cr3_bits > before.pcid_no_pge + before.pcid_cr3_bits {
        klog::event!(Warn, "mm", "tlb.pcid_off", no_pge = now.pcid_no_pge, cr3_bits = now.pcid_cr3_bits, pcid_cpus = now.pcid_cpus;
            "CPUID offers PCID but CPUs stayed without it (CR4.PGE off, or CR3 low bits set when PCIDE was to go on): every switch flushes there");
    }
    if now.kept_pages > before.kept_pages || now.kept_tables > before.kept_tables {
        klog::event!(Warn, "mm", "tlb.kept", pages = now.kept_pages - before.kept_pages, tables = now.kept_tables - before.kept_tables,
            total_pages = now.kept_pages, total_tables = now.kept_tables);
    }
    let t = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if t.is_multiple_of(STATS_EVERY) {
        let changed = {
            let mut g = SEEN.lock();
            let c = g.1.is_none_or(|l| l != now);
            g.1 = Some(now);
            c
        };
        if changed {
            stats_event(&now);
        }
    }
    if crate::vmap::stats().is_some_and(|v| v.quarantined_windows > 0) || crate::vmap::kva_counts().1 > 0 {
        crate::vmap::release_quarantined();
    }
}

pub(crate) fn stats_event(s: &TlbStats) {
    klog::event!(Info, "mm", "tlb.stats", user = s.user, kernel = s.kernel, leaves = s.leaves, ipis = s.ipis, irqs = s.irqs,
        resends = s.resends, give_ups = s.give_ups, send_failed = s.send_failed, page_flushes = s.page_flushes, full_flushes = s.full_flushes,
        everything = s.everything, switches = s.switches, pcid_cpus = s.pcid_cpus, pcid_no_pge = s.pcid_no_pge,
        pcid_cr3_bits = s.pcid_cr3_bits, grants = s.grants, grant_rounds = s.grant_rounds, copy_retry = s.spurious_copy_retry, pcid_kept = s.pcid_kept,
        pcid_flushed = s.pcid_flushed, pcid_evictions = s.pcid_evictions, lazy_entries = s.lazy_entries,
        lazy_leaves = s.lazy_leaves, lazy_skips = s.lazy_skips, lazy_catchups = s.lazy_catchups, lazy_drops = s.lazy_drops, dying_leaves = s.dying_leaves, kept_pages = s.kept_pages, kept_tables = s.kept_tables,
        rounds = s.rounds, slow = s.slow, resent_once = s.resent_once, several = s.several, wait_lt10us = s.wait_hist[0],
        wait_lt100us = s.wait_hist[1], wait_lt1ms = s.wait_hist[2], wait_lt10ms = s.wait_hist[3], wait_ge10ms = s.wait_hist[4]);
}

/// Totals since boot (`hal::paging::tlb::stats`).
pub fn stats() -> TlbStats {
    tlb::stats()
}
