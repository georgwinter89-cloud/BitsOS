//! The decisions of the TLB shootdown (MM-6), free of hardware access so the host tests can check them:
//! how a CPU catches up from one generation to another, which PCID slot a context gets, when a target CPU
//! counts as having answered, and what the waiting initiator does next.

/// Up to this many pages a flush invalidates page by page (`invlpg`); a larger range, a range not known any
/// more or more than one missed generation flushes the whole context instead (Linux's
/// `tlb_single_page_flush_ceiling` is 33).
pub const RANGE_PAGES: u64 = 32;

/// How a CPU brings its TLB from generation `synced` to `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    Nothing,
    /// `invlpg` for `pages` pages from `start`.
    Pages { start: u64, pages: u64 },
    Full,
}

/// `range`: the range published with `generation` (None: a full flush was asked for, or the range was overwritten
/// by a later change while this CPU read it). Only a CPU exactly one generation behind can use the range.
pub fn plan(synced: u64, generation: u64, range: Option<(u64, u64)>) -> Plan {
    if generation <= synced {
        return Plan::Nothing;
    }
    if generation - synced != 1 {
        return Plan::Full;
    }
    match range {
        Some((s, e)) if e > s && (e - s).div_ceil(4096) <= RANGE_PAGES => Plan::Pages { start: s & !0xFFF, pages: (e - (s & !0xFFF)).div_ceil(4096) },
        _ => Plan::Full,
    }
}

/// The range an initiator publishes: (0, 0) stands for "whole context".
pub fn published_range(start: u64, end: u64, full: bool) -> (u64, u64) {
    if full || end <= start || (end - start).div_ceil(4096) > RANGE_PAGES { (0, 0) } else { (start, end) }
}

/// Word and bit of `cpu` in a CPU mask of 64-bit words.
pub fn mask_pos(cpu: usize) -> (usize, u64) {
    (cpu / 64, 1u64 << (cpu % 64))
}

/// What a request for a context asks of a CPU that has it loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// Drop the changed translations. A CPU that holds the context only lazily (a kernel thread runs) counts as
    /// answered at once: it catches up before it runs the context's user code again (MM-tlb3; Linux
    /// `tlb_is_not_lazy`, arch/x86/mm/tlb.c:901).
    Flush,
    /// As `Flush`, but page tables were freed: a lazy CPU answers too (it leaves the context), since its
    /// speculative page walks could still read a freed table (Linux tlb.c:921-927).
    FlushTables,
    /// The context ends: every CPU switches away from it, in sync or not.
    Leave,
}

/// Whether target CPU `c` answered a request for context `x` at generation `generation`: it is not loading `x`,
/// and either does not have `x` loaded or `ask` lets it keep `x` (its TLB reflects `generation` already, or it
/// holds `x` only lazily, `lazy`, and no tables were freed). `entering`/`loaded` are the context addresses the
/// CPU published (0: none).
pub fn user_acked(x: u64, entering: u64, loaded: u64, synced: u64, lazy: bool, generation: u64, ask: Ask) -> bool {
    entering != x
        && (loaded != x
            || match ask {
                Ask::Flush => lazy || synced >= generation,
                Ask::FlushTables => synced >= generation,
                Ask::Leave => false,
            })
}

/// The PCID slot for context `id` among the slots' ids: its own slot (`false`: not new) or the victim
/// `victim % ids.len()` (`true`: new, its PCID must be flushed when loaded). `ids` is never empty.
pub fn pick_slot(ids: &[u64], id: u64, victim: usize) -> (usize, bool) {
    match ids.iter().position(|&s| s == id) {
        Some(i) => (i, false),
        None => (victim % ids.len(), true),
    }
}

/// What a waiting initiator does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Wait,
    /// Send the IPI again to every CPU that has not answered.
    Resend,
    /// Stop waiting: the CPUs that have not answered are marked stuck and the caller keeps its frames.
    GiveUp,
}

/// The waits of one shootdown (MM-tlb). A fixed IPI to an online CPU is not lost, so a resend is a safety net,
/// not a cure for a slow target: the first one comes after `first_ns` (a CPU waking from a deep package
/// C-state, an SMI or a short interrupts-off stretch answers within that), then the interval doubles up to
/// `max_ns`. `limit_ns` ends the wait (B4), `stuck_limit_ns` when every CPU still missing was stuck already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub first_ns: u64,
    pub max_ns: u64,
    pub limit_ns: u64,
    pub stuck_limit_ns: u64,
}

/// The interval before resend number `resends + 1`: `first_ns`, doubled per resend so far, at most `max_ns`.
pub fn resend_interval(l: &Limits, resends: u32) -> u64 {
    l.first_ns.saturating_mul(1u64 << resends.min(32)).min(l.max_ns.max(l.first_ns))
}

/// Where a waiting initiator stands: when it started, last sent, last looked at the answers (before this
/// look), how many resend rounds it made, and whether its previous look was deferred because of a gap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub start: u64,
    pub last_send: u64,
    pub last_look: u64,
    pub resends: u32,
    pub deferred: bool,
}

impl Waiting {
    pub fn new(start: u64) -> Self {
        Waiting { start, last_send: start, last_look: start, resends: 0, deferred: false }
    }
}

/// Whether the initiator was away for half the current interval or more since its previous look.
fn away(w: &Waiting, now: u64, l: &Limits) -> bool {
    now.saturating_sub(w.last_look) >= (resend_interval(l, w.resends) / 2).max(1)
}

/// `now` is read **before** the look that still found CPUs missing. `all_stuck`: every CPU still missing was
/// stuck already (it missed an earlier request and has not served one since). When the initiator itself was
/// away for half an interval or more since its previous look (an SMI, an NMI, a long `service` of its own),
/// the targets were not given that time to answer while it watched: it looks once more before it resends or
/// gives up (a resend counted right after such a gap was noise in the A1 logs' sense). Never twice in a row
/// (`deferred`, kept by `looked`): an initiator that is away for every look still resends and gives up (B4).
pub fn step(w: &Waiting, now: u64, l: &Limits, all_stuck: bool) -> Step {
    let iv = resend_interval(l, w.resends);
    if !w.deferred && away(w, now, l) {
        return Step::Wait;
    }
    let limit = if all_stuck { l.stuck_limit_ns } else { l.limit_ns };
    if now.saturating_sub(w.start) >= limit {
        Step::GiveUp
    } else if now.saturating_sub(w.last_send) >= iv {
        Step::Resend
    } else {
        Step::Wait
    }
}

/// Records the look `step` just decided at `now` (call it after `step`, before acting on a resend): whether
/// that look was deferred, and its time.
pub fn looked(w: &mut Waiting, now: u64, l: &Limits) {
    w.deferred = !w.deferred && away(w, now, l);
    w.last_look = now;
}

/// How one answered shootdown is reported (`mm.report`): quick, slow (waited `slow_ns` or more without a
/// resend), resent once (Info), or several resends (Warn: a CPU did not answer for several intervals).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Quick,
    Slow,
    ResentOnce,
    Several,
}

pub fn verdict(wait_ns: u64, resends: u32, slow_ns: u64) -> Verdict {
    match resends {
        0 if wait_ns >= slow_ns => Verdict::Slow,
        0 => Verdict::Quick,
        1 => Verdict::ResentOnce,
        _ => Verdict::Several,
    }
}

/// Upper bounds of the wait histogram's buckets (10 µs, 100 µs, 1 ms, 10 ms; the last bucket is open).
pub const WAIT_BOUNDS_NS: [u64; 4] = [10_000, 100_000, 1_000_000, 10_000_000];
/// Buckets of the wait histogram (a fixed set of decades, not a count of things).
pub const WAIT_BUCKETS: usize = WAIT_BOUNDS_NS.len() + 1;

/// The histogram bucket of a wait of `ns`.
pub fn wait_bucket(ns: u64) -> usize {
    WAIT_BOUNDS_NS.iter().position(|&b| ns < b).unwrap_or(WAIT_BOUNDS_NS.len())
}

/// TSC ticks to ns at `hz` (0: unknown, 0 back); saturates instead of overflowing.
pub fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    if hz == 0 {
        return 0;
    }
    let ns = ticks as u128 * 1_000_000_000 / hz as u128;
    ns.min(u64::MAX as u128) as u64
}

/// Whether rewriting a present leaf from `old` to `new` (both in the 4 KiB layout, frame bits included) only
/// grants rights, so no other CPU needs a shootdown (MM-tlb2): WRITABLE set or NX cleared, everything else
/// (frame, present, user, memory type, global) the same; the Accessed/Dirty bits must not be cleared (a clear
/// needs a flush, linux-6.6/arch/x86/include/asm/tlbflush.h:306 `flush_on_clear`). A CPU with the old,
/// narrower translation cached takes one spurious fault and drops it there (Linux's fault path:
/// mm/memory.c:3030 `wp_page_reuse` -> `ptep_set_access_flags`, arch/x86/mm/pgtable.c:499, no flush;
/// `flush_tlb_fix_spurious_fault` is empty on x86, arch/x86/include/asm/pgtable.h:1325).
pub fn grant_only(old: u64, new: u64) -> bool {
    const W: u64 = 1 << 1;
    const A: u64 = 1 << 5;
    const D: u64 = 1 << 6;
    const NX: u64 = 1 << 63;
    let diff = old ^ new;
    old & 1 != 0
        && new & 1 != 0
        && diff & !(W | NX | A | D) == 0
        && diff & W & old == 0
        && diff & NX & new == 0
        && diff & (A | D) & old == 0
}

/// Frames one user unmap gathers at most before a shootdown round (MM-tlb2): Linux's mmu_gather keeps up to
/// `MAX_GATHER_BATCH_COUNT` batches of `MAX_GATHER_BATCH` pages, about 10,000 pages
/// (linux-6.6/include/asm-generic/tlb.h:252-261), to bound one round's latency and the memory held back.
pub const GATHER_MAX: usize = 10_000;

#[cfg(test)]
#[path = "tlb_plan_tests.rs"]
mod tests;
