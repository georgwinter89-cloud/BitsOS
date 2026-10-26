extern crate std;
use super::*;
use std::vec::Vec;

#[test]
fn plan_uses_the_range_only_one_generation_behind() {
    assert_eq!(plan(5, 5, None), Plan::Nothing);
    assert_eq!(plan(6, 5, Some((0x1000, 0x2000))), Plan::Nothing, "ahead (synced by a later load)");
    assert_eq!(plan(4, 5, Some((0x1000, 0x3000))), Plan::Pages { start: 0x1000, pages: 2 });
    assert_eq!(plan(3, 5, Some((0x1000, 0x3000))), Plan::Full, "two behind: the earlier range is gone");
    assert_eq!(plan(4, 5, None), Plan::Full);
    assert_eq!(plan(4, 5, Some((0, 0))), Plan::Full, "(0, 0) means the whole context");
    assert_eq!(plan(4, 5, Some((0x1800, 0x2001))), Plan::Pages { start: 0x1000, pages: 2 }, "unaligned ends cover their pages");
    let ceiling = RANGE_PAGES * 4096;
    assert_eq!(plan(0, 1, Some((0, ceiling))), Plan::Pages { start: 0, pages: RANGE_PAGES }, "start 0 with end > 0 is a real range");
    assert_eq!(plan(0, 1, Some((0x1000, 0x1000 + ceiling))), Plan::Pages { start: 0x1000, pages: RANGE_PAGES });
    assert_eq!(plan(0, 1, Some((0x1000, 0x2000 + ceiling))), Plan::Full);
}

#[test]
fn published_range_marks_large_and_full_requests() {
    assert_eq!(published_range(0x1000, 0x5000, false), (0x1000, 0x5000));
    assert_eq!(published_range(0x1000, 0x5000, true), (0, 0));
    assert_eq!(published_range(0x5000, 0x5000, false), (0, 0), "empty range: whole context, never a no-op");
    assert_eq!(published_range(0, (RANGE_PAGES + 1) * 4096, false), (0, 0));
    assert_eq!(published_range(0x10_0000, 0x10_0000 + RANGE_PAGES * 4096, false), (0x10_0000, 0x10_0000 + RANGE_PAGES * 4096));
}

#[test]
fn mask_positions() {
    assert_eq!(mask_pos(0), (0, 1));
    assert_eq!(mask_pos(63), (0, 1 << 63));
    assert_eq!(mask_pos(64), (1, 1));
    assert_eq!(mask_pos(130), (2, 4));
}

#[test]
fn ack_needs_the_cpu_out_of_the_entry_and_in_sync_or_gone() {
    let (x, y) = (0xA000, 0xB000);
    let f = Ask::Flush;
    assert!(!user_acked(x, x, 0, 0, false, 3, f), "loading x right now: not yet");
    assert!(!user_acked(x, x, y, 9, false, 3, f), "loading x while y is still the loaded one");
    assert!(!user_acked(x, 0, x, 2, false, 3, f), "behind");
    assert!(user_acked(x, 0, x, 3, false, 3, f), "in sync");
    assert!(user_acked(x, 0, x, 4, false, 3, f), "ahead (loaded after the change)");
    assert!(user_acked(x, 0, y, 0, false, 3, f), "left x");
    assert!(user_acked(x, y, 0, 0, false, 3, f), "loading another context");
    assert!(!user_acked(x, 0, x, 9, false, 3, Ask::Leave), "a leave needs the CPU off x, in sync or not");
    assert!(!user_acked(x, 0, x, 9, true, 3, Ask::Leave), "lazily too");
    assert!(user_acked(x, 0, 0, 0, false, 3, Ask::Leave));
}

#[test]
fn a_lazy_cpu_answers_a_flush_at_once_but_not_freed_tables() {
    let (x, y) = (0xA000, 0xB000);
    assert!(user_acked(x, 0, x, 2, true, 3, Ask::Flush), "lazy and behind: catches up when it runs x again");
    assert!(!user_acked(x, 0, x, 2, true, 3, Ask::FlushTables), "freed tables: a lazy CPU must leave");
    assert!(user_acked(x, 0, x, 3, true, 3, Ask::FlushTables), "in sync");
    assert!(user_acked(x, 0, y, 0, true, 3, Ask::FlushTables), "left x");
    assert!(!user_acked(x, x, 0, 0, true, 3, Ask::Flush), "loading x is never skipped");
    assert!(!user_acked(x, 0, x, 2, false, 3, Ask::FlushTables), "behind");
}

#[test]
fn slots_are_kept_per_context_and_victims_rotate() {
    let ids = [7, 0, 9, 0, 0, 0];
    assert_eq!(pick_slot(&ids, 9, 4), (2, false));
    assert_eq!(pick_slot(&ids, 7, 0), (0, false));
    assert_eq!(pick_slot(&ids, 11, 4), (4, true));
    assert_eq!(pick_slot(&ids, 11, 13), (1, true), "victim counter wraps over the slots");
}

const L: Limits = Limits { first_ns: 1_000, max_ns: 8_000, limit_ns: 500_000, stuck_limit_ns: 2_000 };

/// A wait that looked at `now - 10` (no gap) after `resends` resends, the last at `last_send`.
fn w(start: u64, last_send: u64, resends: u32, now: u64) -> Waiting {
    Waiting { start, last_send, last_look: now.saturating_sub(10), resends, deferred: false }
}

#[test]
fn resend_interval_doubles_up_to_the_cap() {
    let iv: Vec<u64> = (0..6).map(|r| resend_interval(&L, r)).collect();
    assert_eq!(iv, [1_000, 2_000, 4_000, 8_000, 8_000, 8_000]);
    assert_eq!(resend_interval(&L, u32::MAX), 8_000, "no shift overflow");
    let odd = Limits { max_ns: 10, ..L };
    assert_eq!(resend_interval(&odd, 0), 1_000, "a cap below the first interval never shortens it");
}

#[test]
fn waiting_resends_with_backoff_and_gives_up_at_the_limit() {
    assert_eq!(step(&w(0, 0, 0, 100), 100, &L, false), Step::Wait);
    assert_eq!(step(&w(0, 0, 0, 1_000), 1_000, &L, false), Step::Resend);
    assert_eq!(step(&w(0, 1_000, 1, 2_500), 2_500, &L, false), Step::Wait, "second interval is 2x");
    assert_eq!(step(&w(0, 1_000, 1, 3_000), 3_000, &L, false), Step::Resend);
    assert_eq!(step(&w(0, 3_000, 2, 6_000), 6_000, &L, false), Step::Wait, "third interval is 4x");
    assert_eq!(step(&w(0, 3_000, 2, 7_000), 7_000, &L, false), Step::Resend);
    assert_eq!(step(&w(0, 1_000, 1, 2_500), 2_500, &L, true), Step::GiveUp, "only stuck CPUs left: short limit");
    assert_eq!(step(&w(0, 499_000, 9, 500_000), 500_000, &L, false), Step::GiveUp);
    assert_eq!(step(&w(10, 10, 0, 5), 5, &L, false), Step::Wait, "a clock that steps back never underflows");
}

#[test]
fn an_initiator_that_was_away_looks_again_before_resending_or_giving_up() {
    // Away 600 ns (more than half the 1 µs interval) since its last look: the targets get one more look.
    let away = Waiting { start: 0, last_send: 0, last_look: 400, resends: 0, deferred: false };
    assert_eq!(step(&away, 1_000, &L, false), Step::Wait);
    let away = Waiting { start: 0, last_send: 0, last_look: 100, resends: 3, deferred: false };
    assert_eq!(step(&away, 600_000, &L, false), Step::Wait, "not even a give-up right after a gap");
    // Back from the gap: the next look (no gap) decides as usual.
    assert_eq!(step(&Waiting { last_look: 999_990, ..away }, 1_000_000, &L, false), Step::GiveUp);
    let back = Waiting { start: 0, last_send: 0, last_look: 990, resends: 0, deferred: false };
    assert_eq!(step(&back, 1_000, &L, false), Step::Resend);
}

#[test]
fn a_gap_defers_only_one_look() {
    // Deferred once already: the next look decides even after another gap.
    let away = Waiting { start: 0, last_send: 0, last_look: 100, resends: 3, deferred: true };
    assert_eq!(step(&away, 600_000, &L, false), Step::GiveUp);
    let away = Waiting { start: 0, last_send: 0, last_look: 400, resends: 0, deferred: true };
    assert_eq!(step(&away, 1_000, &L, false), Step::Resend);
    // `looked` sets the flag for a deferred look and clears it on the look after.
    let mut w = Waiting::new(0);
    looked(&mut w, 600, &L);
    assert!(w.deferred && w.last_look == 600);
    looked(&mut w, 1_200, &L);
    assert!(!w.deferred, "never two deferred looks in a row");
    looked(&mut w, 1_210, &L);
    assert!(!w.deferred, "no gap, no deferral");
}

/// Finding MM-tlb fixer 1: an initiator away for at least half an interval before every look (an
/// oversubscribed host, an SMI storm) still resends and gives up near the limit (B4).
#[test]
fn an_initiator_away_for_every_look_still_gives_up() {
    for gap in [500u64, 777, 4_000, 8_000, 50_000] {
        let mut w = Waiting::new(0);
        let mut t = 0u64;
        let mut resends = 0;
        let end = loop {
            t += gap;
            let s = step(&w, t, &L, false);
            looked(&mut w, t, &L);
            match s {
                Step::Wait => {}
                Step::Resend => {
                    resends += 1;
                    w.resends += 1;
                    w.last_send = t;
                }
                Step::GiveUp => break t,
            }
            assert!(t < 10 * L.limit_ns, "gap {gap}: never gave up");
        };
        assert!(end < L.limit_ns + 2 * gap, "gap {gap}: gave up at {end}");
        assert!(resends > 0, "gap {gap}: never resent");
    }
}

#[test]
fn verdicts_and_buckets() {
    assert_eq!(verdict(500, 0, 1_000), Verdict::Quick);
    assert_eq!(verdict(1_000, 0, 1_000), Verdict::Slow);
    assert_eq!(verdict(3_000, 1, 1_000), Verdict::ResentOnce);
    assert_eq!(verdict(9_000, 2, 1_000), Verdict::Several);
    assert_eq!(verdict(100, 7, 1_000), Verdict::Several, "resends decide, not the wait");
    let b: Vec<usize> = [0, 9_999, 10_000, 99_999, 100_000, 999_999, 1_000_000, 9_999_999, 10_000_000, u64::MAX]
        .iter()
        .map(|&n| wait_bucket(n))
        .collect();
    assert_eq!(b, [0, 0, 1, 1, 2, 2, 3, 3, 4, 4]);
    assert_eq!(WAIT_BUCKETS, 5);
    assert_eq!(ticks_to_ns(3_000_000_000, 3_000_000_000), 1_000_000_000);
    assert_eq!(ticks_to_ns(7, 0), 0, "unknown rate");
    assert_eq!(ticks_to_ns(u64::MAX, 1), u64::MAX, "saturates");
}

#[test]
fn grant_only_allows_widening_alone() {
    const P: u64 = 1;
    const W: u64 = 1 << 1;
    const U: u64 = 1 << 2;
    const PCD: u64 = 1 << 4;
    const A: u64 = 1 << 5;
    const D: u64 = 1 << 6;
    const G: u64 = 1 << 8;
    const NX: u64 = 1 << 63;
    let ro = 0x1234_5000 | P | U | NX;
    assert!(grant_only(ro, ro | W), "read-only to writable");
    assert!(grant_only(ro, ro & !NX), "NX dropped");
    assert!(grant_only(ro, (ro | W | A | D) & !NX), "both, with A/D set by the writer");
    assert!(grant_only(ro | A, ro | A | W), "A kept");
    assert!(grant_only(ro, ro), "no change at all needs no flush either");
    assert!(!grant_only(ro | W, ro), "writable to read-only");
    assert!(!grant_only(ro & !NX, ro), "NX set");
    assert!(!grant_only(ro | A | D, ro | W), "A/D cleared");
    assert!(!grant_only(ro, (ro & !0xFFFF_F000) | 0x9999_9000 | W), "another frame");
    assert!(!grant_only(ro, ro | W | PCD), "another memory type");
    assert!(!grant_only(ro, (ro & !U) | W), "user bit gone");
    assert!(!grant_only(ro, ro | W | G), "global bit");
    assert!(!grant_only(ro & !P, ro | W), "not present before");
    assert!(!grant_only(ro, (ro | W) & !P), "not present after");
}
