//! The space warning (VS-6, Georg 22.10.): snapshots taken by hand and the "before restore" ones are never
//! pruned (retention.rs), so they can fill the pool. Once the pool is more than 80 % full, bitssnap tells Georg
//! (a Warn line in the kernel log once per crossing, `bitssnap status`, the MCP `snapshots` answer) and proposes
//! the oldest of them for deletion. Nothing here deletes: the proposal is a list; `bitssnap delete` and the MCP
//! tool `snapshot_delete` are Georg's way to act on it.
//! There is no desktop notification path yet (wave 12): when one exists, `notice` is where it plugs in.

use crate::dev::{Dev, Snap, Stats};
use crate::{human, local_time, log, log_warn};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Warn below this much free space (per mille of the pool, the kernel's `free_permille`): more than 80 % full.
pub const WARN_FREE_PERMILLE: u64 = 200;
/// The "below again" line only from this much free space (75 % full or less): a pool that hovers around 80 %
/// would otherwise log a pair of lines every round. The same line as the default `snap.min_free_pct` 25
/// (VS-6b): below it automatic snapshots are pruned down to 70 % full (VS-6c, `retention::space_target`), so
/// they alone never warn; only manual and before-restore ones do.
pub const REARM_FREE_PERMILLE: u64 = 250;
/// Ids named in the kernel log line; the rest are counted (the log cuts lines at 240 bytes).
const LOG_IDS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub id: u64,
    pub created_s: u64,
    pub kind: &'static str,
    pub label: String,
    /// What deleting only this one frees (`None`: listed without sizes).
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub free_permille: u64,
    pub pool_bytes: u64,
    pub warning: bool,
    /// The oldest manual and before-restore snapshots, oldest first, until their sizes would bring the pool
    /// back below the warning line (all of them when that is not enough); empty without a warning.
    pub proposal: Vec<Candidate>,
    /// The sum of the proposal's own sizes: deleting them together frees at least this much (chunks shared
    /// only between two of them come free too).
    pub proposal_bytes: u64,
}

/// The pool is more than 80 % full: the warning and the proposal for these stats and this listing. Pure.
pub fn check(st: &Stats, list: &[Snap]) -> Space {
    check_sized(st, list, |_| None)
}

/// As `check`; a candidate listed without its size gets it from `size(id)`, asked only for the candidates the
/// proposal reaches (not for every automatic snapshot: their number grows every 5 minutes, R1).
pub fn check_sized(st: &Stats, list: &[Snap], mut size: impl FnMut(u64) -> Option<u64>) -> Space {
    let pool_bytes = st.chunks.saturating_mul(st.chunk_size);
    let mut sp = Space { free_permille: st.free_permille, pool_bytes, ..Space::default() };
    // No pool behind the answer (chunks=0): nothing to judge.
    if st.chunks == 0 || st.free_permille >= WARN_FREE_PERMILLE {
        return sp;
    }
    sp.warning = true;
    // Mounted ones and the one a rollback goes back to cannot be deleted now (EBUSY): not proposed.
    let mut c: Vec<&Snap> = list.iter().filter(|s| !s.auto && !s.mounted && !s.rollback).collect();
    c.sort_by_key(|s| (s.created_ns, s.id));
    let need = WARN_FREE_PERMILLE - st.free_permille;
    for s in c {
        let bytes = s.bytes.or_else(|| size(s.id));
        sp.proposal.push(Candidate { id: s.id, created_s: s.created_ns / 1_000_000_000, kind: s.kind(), label: s.label.clone(), bytes });
        sp.proposal_bytes = sp.proposal_bytes.saturating_add(bytes.unwrap_or(0));
        let freed = (sp.proposal_bytes as u128 * 1000 / pool_bytes.max(1) as u128) as u64;
        if freed >= need {
            break;
        }
    }
    sp
}

/// Stats and listing from the device; a `size` command only for each proposed snapshot.
pub fn read(dev: &mut Dev) -> Result<(Stats, Space), String> {
    let st = dev.stats().map_err(|e| e.to_string())?;
    // A stats answer without the pool's size would silently never warn: said as a failure instead.
    if st.chunks == 0 || st.chunk_size == 0 {
        return Err(format!("stats answer without chunks/chunk_size: {}", st.raw));
    }
    let l = dev.list(false).map_err(|e| e.to_string())?;
    // A snapshot deleted meanwhile has no size: shown as "size unknown", the proposal goes on.
    let sp = check_sized(&st, &l, |id| dev.size(id).ok());
    Ok((st, sp))
}

fn pct(permille: u64) -> String {
    format!("{}.{}", permille / 10, permille % 10)
}

/// One proposal entry: `7 (2026-10-22 14:00, manual "vs6", frees 4.2 MB)`.
pub fn entry(c: &Candidate, off_s: i64) -> String {
    let b = c.bytes.map(human).unwrap_or_else(|| "size unknown".into());
    format!("{} ({}, {} \"{}\", frees {b})", c.id, local_time(c.created_s, off_s), c.kind, c.label)
}

/// The warning as one line (`None` below 80 % full).
pub fn warning_text(sp: &Space, off_s: i64) -> Option<String> {
    if !sp.warning {
        return None;
    }
    let head = format!(
        "the data pool is {} % full (warning above 80 %); snapshots taken by hand and before restores are never deleted automatically",
        pct(1000 - sp.free_permille.min(1000))
    );
    if sp.proposal.is_empty() {
        return Some(format!("{head}; there are none to propose: the data and the automatic snapshots fill it"));
    }
    let list: Vec<String> = sp.proposal.iter().map(|c| entry(c, off_s)).collect();
    Some(format!(
        "{head}; proposed for deletion, oldest first (together at least {}): {}; nothing is deleted without Georg (bitssnap delete ID, MCP snapshot_delete)",
        human(sp.proposal_bytes),
        list.join(", ")
    ))
}

/// The warning for the kernel log: short (the log splits lines at 240 bytes), the ids only; times and sizes are
/// in `bitssnap status` and the MCP answer.
pub fn log_text(sp: &Space) -> Option<String> {
    if !sp.warning {
        return None;
    }
    let head = format!("the data pool is {} % full (warning above 80 %)", pct(1000 - sp.free_permille.min(1000)));
    if sp.proposal.is_empty() {
        return Some(format!("{head}; no manual snapshots to propose: the data and the automatic snapshots fill it"));
    }
    let mut ids: Vec<String> = sp.proposal.iter().take(LOG_IDS).map(|c| c.id.to_string()).collect();
    if sp.proposal.len() > LOG_IDS {
        ids.push(format!("{} more", sp.proposal.len() - LOG_IDS));
    }
    Some(format!(
        "{head}; proposed for deletion: snapshots {} (at least {}; details: bitssnap status); nothing is deleted without Georg",
        ids.join(", "),
        human(sp.proposal_bytes)
    ))
}

/// Said in the log for the current crossing: one Warn line when the pool passes 80 %, one line when it is back
/// below 75 %.
static WARNED: AtomicBool = AtomicBool::new(false);
/// The last failed space check said in the log (each different failure once, not every round).
static FAILED: Mutex<String> = Mutex::new(String::new());

/// The line for a crossing, if this is one: (Warn?, text). `warned` remembers the current side; between 75 and
/// 80 % full nothing changes (hysteresis).
fn crossing(warned: &AtomicBool, sp: &Space) -> Option<(bool, String)> {
    if sp.warning {
        if warned.swap(true, Ordering::Relaxed) {
            return None;
        }
        log_text(sp).map(|t| (true, t))
    } else if sp.pool_bytes > 0 && sp.free_permille >= REARM_FREE_PERMILLE && warned.swap(false, Ordering::Relaxed) {
        Some((false, format!("the data pool is below 75 % full again ({} % free)", pct(sp.free_permille))))
    } else {
        None
    }
}

/// Whether this failure is new (`Some`: say it) or the same as the last one; `None` input: the check worked.
fn new_failure(last: &Mutex<String>, e: Option<&str>) -> bool {
    let mut l = last.lock().unwrap_or_else(|p| p.into_inner());
    match e {
        Some(e) if *l != e => {
            *l = e.to_string();
            true
        }
        Some(_) => false,
        None => {
            l.clear();
            false
        }
    }
}

/// Tells the kernel log about a crossing (the serve rounds and the MCP thread share it: one line per crossing).
pub fn notice(sp: &Space) {
    new_failure(&FAILED, None);
    match crossing(&WARNED, sp) {
        Some((true, t)) => log_warn(&t),
        Some((false, t)) => log(&t),
        None => {}
    }
}

/// A failed space check, in the log once per different failure.
pub fn failed(e: &str) {
    if new_failure(&FAILED, Some(e)) {
        log(&format!("space check failed: {e}"));
    }
}

/// `bitssnap status`: the pool, the snapshots by kind, the pauses and the space warning with its proposal.
pub fn status_lines(st: &Stats, list: &[Snap], sp: &Space) -> Vec<String> {
    let auto = list.iter().filter(|s| s.auto).count();
    let off = st.utc_offset_min * 60;
    let free = st.free_permille.min(1000);
    let mut out = vec![
        format!("data pool: {} % full, {} % free of {}", pct(1000 - free), pct(free), human(sp.pool_bytes)),
        format!("snapshots: {} ({auto} automatic, {} manual or before-restore, kept until deleted by hand)", list.len(), list.len() - auto),
        format!("freeze pause: last {} ms, longest {} ms (this boot)", st.last_pause_us / 1000, st.max_pause_us / 1000),
    ];
    match warning_text(sp, off) {
        None => out.push("space: fine (a warning comes above 80 % full)".into()),
        Some(w) => {
            out.push(format!("WARNING: {w}"));
            out.extend(sp.proposal.iter().map(|c| format!("  proposed: {}", entry(c, off))));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retention::{Freed, Item, prune_for_space};

    const MB: u64 = 1_000_000;
    const T0: u64 = 1_792_368_000;

    fn stats(free_permille: u64) -> Stats {
        // A 100 MB pool of 1 MB chunks: one per mille is 100 KB.
        Stats { free_permille, chunks: 100, chunk_size: MB, ..Stats::default() }
    }

    fn snap(id: u64, min: u64, auto: bool, label: &str, mb: u64) -> Snap {
        Snap { id, created_ns: (T0 + min * 60) * 1_000_000_000, auto, mounted: false, rollback: false, bytes: Some(mb * MB), label: label.into() }
    }

    fn ids(sp: &Space) -> Vec<u64> {
        sp.proposal.iter().map(|c| c.id).collect()
    }

    #[test]
    fn below_80_percent_no_warning() {
        let l = [snap(2, 0, false, "manual", 30)];
        for free in [200, 201, 500, 1000] {
            let sp = check(&stats(free), &l);
            assert!(!sp.warning && sp.proposal.is_empty() && warning_text(&sp, 0).is_none(), "{free}");
        }
        // No pool behind the answer.
        assert!(!check(&Stats::default(), &l).warning);
    }

    #[test]
    fn oldest_manual_first_until_back_below_80() {
        // 85 % full: 5 % (5 MB) to find. Autos are never proposed, mounted ones cannot be deleted now.
        let mut m = snap(6, 1, false, "manual", 9);
        m.mounted = true;
        let l = [snap(2, 0, true, "auto", 50), snap(9, 40, false, "x", 1), m, snap(4, 10, false, "before-restore", 3), snap(5, 20, false, "vs6", 1), snap(3, 30, false, "a", 1)];
        let sp = check(&stats(150), &l);
        assert!(sp.warning);
        assert_eq!(ids(&sp), [4, 5, 3], "3 + 1 + 1 MB reach 5 %");
        assert_eq!(sp.proposal_bytes, 5 * MB);
        assert_eq!(sp.proposal[0].kind, "before-restore");
        assert_eq!(sp.proposal[1].kind, "manual");
        let t = warning_text(&sp, 0).unwrap();
        assert!(t.contains("85.0 % full") && t.contains("4 (2026-10-19 00:10, before-restore \"before-restore\", frees 3.0 MB)"), "{t}");
        assert!(t.contains("at least 5.0 MB") && t.contains("without Georg"), "{t}");
        let l = log_text(&sp).unwrap();
        assert!(l.starts_with("the data pool is 85.0 % full (warning above 80 %)") && l.contains("snapshots 4, 5, 3 (at least 5.0 MB"), "{l}");
        assert!(l.len() < 240, "one log piece: {l}");
    }

    #[test]
    fn not_enough_proposes_all_and_says_so() {
        let l = [snap(2, 0, false, "m", 1), snap(3, 5, false, "n", 2)];
        let sp = check(&stats(10), &l);
        assert_eq!(ids(&sp), [2, 3]);
        let none = check(&stats(10), &[snap(2, 0, true, "auto", 1)]);
        assert!(none.warning && none.proposal.is_empty());
        assert!(warning_text(&none, 0).unwrap().contains("none to propose"));
    }

    #[test]
    fn one_line_per_crossing() {
        let w = AtomicBool::new(false);
        let (full, ok) = (check(&stats(150), &[]), check(&stats(400), &[]));
        assert!(crossing(&w, &ok).is_none(), "never warned: nothing to take back");
        assert!(matches!(crossing(&w, &full), Some((true, _))));
        assert!(crossing(&w, &full).is_none(), "the next round says nothing new");
        assert!(matches!(crossing(&w, &ok), Some((false, t)) if t.contains("40.0 % free")));
        assert!(crossing(&w, &ok).is_none());
        assert!(matches!(crossing(&w, &full), Some((true, _))), "a new crossing warns again");
    }

    #[test]
    fn unknown_sizes_count_as_nothing_freed() {
        let mut a = snap(2, 0, false, "m", 0);
        a.bytes = None;
        let sp = check(&stats(190), &[a, snap(3, 5, false, "n", 2)]);
        assert_eq!(ids(&sp), [2, 3]);
        assert!(warning_text(&sp, 0).unwrap().contains("size unknown"));
    }

    #[test]
    fn log_line_stays_one_piece_with_many_proposed() {
        let l: Vec<Snap> = (1..=100).map(|i| snap(1_000_000 + i, i, false, "manual-snapshot-with-a-long-name", 0)).collect();
        let sp = check(&stats(10), &l);
        assert_eq!(sp.proposal.len(), 100, "sizes 0: all proposed");
        let t = log_text(&sp).unwrap();
        assert!(t.len() < 240, "{} bytes: {t}", t.len());
        assert!(t.contains("snapshots 1000001, 1000002, 1000003, 1000004, 1000005, 95 more (at least"), "{t}");
        assert!(t.ends_with("nothing is deleted without Georg"), "{t}");
    }

    /// One round as the tests see it.
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Seen {
        warning: bool,
        /// The crossing line if one: Warn?
        line: Option<bool>,
        /// Snapshots the space rule removed this round.
        pruned: usize,
        /// Per mille free after the round.
        free: u64,
    }

    /// One simulated round per minute (all inside `keep_all`, so only the space rule removes): the data holds half
    /// the pool, each automatic snapshot keeps 0.7 % for itself, a manual one 0.9 % every `manual_every` rounds;
    /// `pre` automatic ones are there before the first round (an install boot). The round's own space rule
    /// (`prune_for_space`, as serve calls it) with `min_free`; then the warning check.
    /// Returns every round, the free per mille at the end and the snapshots left.
    fn pool_rounds(min_free: u64, manual_every: Option<u64>, rounds: u64, pre: u64) -> (Vec<Seen>, u64, Vec<Snap>) {
        const UNIT: u64 = 100_000; // one per mille of the 100 MB pool of `stats`
        let (auto_pm, manual_pm, data_pm) = (7, 9, 500);
        let free_of = |l: &[Snap]| 1000u64.saturating_sub(data_pm + l.iter().map(|s| s.bytes.unwrap_or(0) / UNIT).sum::<u64>());
        let w = AtomicBool::new(false);
        let (mut list, mut out, mut next) = (Vec::<Snap>::new(), Vec::new(), 1);
        for m in 0..pre {
            list.push(snap(next, m, true, "auto", 0));
            list.last_mut().unwrap().bytes = Some(auto_pm * UNIT);
            next += 1;
        }
        for r in 0..rounds {
            list.push(snap(next, pre + r, true, "auto", 0));
            list.last_mut().unwrap().bytes = Some(auto_pm * UNIT);
            next += 1;
            if manual_every.is_some_and(|k| r % k == k - 1) {
                list.push(snap(next, pre + r, false, "manual", 0));
                list.last_mut().unwrap().bytes = Some(manual_pm * UNIT);
                next += 1;
            }
            let its: Vec<Item> = list.iter().map(|s| Item { id: s.id, created_s: s.created_ns / 1_000_000_000, auto: s.auto }).collect();
            let now = free_of(&list);
            let (gone, free) = prune_for_space(&its, &mut Vec::new(), min_free, now, false, |id| {
                list.retain(|s| s.id != id);
                Ok::<_, ()>(Freed::Now(free_of(&list)))
            })
            .unwrap();
            assert_eq!(free, free_of(&list), "round {r}: the rule saw the pool as it is");
            let sp = check(&stats(free), &list);
            out.push(Seen { warning: sp.warning, line: crossing(&w, &sp).map(|c| c.0), pruned: gone.len(), free });
        }
        let free = free_of(&list);
        (out, free, list)
    }

    /// The rounds in which the space rule removed something.
    fn bursts(out: &[Seen]) -> Vec<usize> {
        out.iter().enumerate().filter(|(_, o)| o.pruned > 0).map(|(r, _)| r).collect()
    }

    #[test]
    fn automatic_snapshots_alone_never_warn() {
        // VS-6b: the default keeps 25 % free; VS-6c: below it the autos go down to 70 % full at once, so a pool of
        // autos moves between 70 % and 75 % full, below the warning.
        let min_free = crate::settings::Settings::default().policy(0).min_free_permille;
        assert_eq!(min_free, REARM_FREE_PERMILLE, "pruning starts where the warning is quiet again");
        let (out, free, list) = pool_rounds(min_free, None, 300, 0);
        assert!(out.iter().all(|o| !o.warning && o.line.is_none()), "never a warning, never a line");
        assert!((250..300 + 7).contains(&free), "between 75 % and 70 % full: {free}");
        assert!(out.iter().all(|o| o.free >= 250), "never more than 75 % full after a round");
        assert!(list.len() < 300 && list.iter().all(|s| s.auto), "autos were pruned: {}", list.len());
        // The Mini-PC before VS-6b (generation 19, 22.10.): with 15 % the pool comes back to 80 % full at best and
        // never below 75 %, so the warning is said once and never re-armed.
        let (out, free, _) = pool_rounds(150, None, 300, 0);
        assert!((150..207).contains(&free), "the old default: {free}");
        assert!(out.iter().any(|o| o.warning), "the old default warns");
        assert_eq!(out.iter().filter(|o| o.line.is_some()).count(), 1, "one Warn line, never re-armed");
    }

    #[test]
    fn space_rule_prunes_in_bursts_not_every_round() {
        // VS-6c (Mini-PC wave 13, FAIL 4): stopping right at 25 % free, the next auto (0.7 %) pushed the pool
        // below again and nearly every round pruned one. Now one burst down to 30 % and quiet for several rounds.
        let min_free = crate::settings::Settings::default().policy(0).min_free_permille;
        let (out, _, _) = pool_rounds(min_free, None, 300, 0);
        let b = bursts(&out);
        assert!(b.len() >= 5, "the pool did fill: {b:?}");
        for o in out.iter().filter(|o| o.pruned > 0) {
            assert!((300..307).contains(&o.free), "each burst ends between 70 % and 69.3 % full: {o:?}");
            assert!(o.pruned >= 7, "a burst is the whole band: {o:?}");
        }
        // 50 per mille band at 7 per mille an auto: a burst every 8 rounds; never two within 5 (5 min at 1/min).
        assert!(b.windows(2).all(|w| w[1] - w[0] >= 7), "bursts too close: {b:?}");
        let settled = b[0];
        assert!(b.len() <= (300 - settled) / 7 + 1, "{} bursts in {} rounds", b.len(), 300 - settled);
        assert!(out.iter().all(|o| !o.warning && o.line.is_none()), "the warning never sees it");
    }

    #[test]
    fn install_boot_prunes_once() {
        // The install boot of wave 13: many autos from the install, the pool already past 75 %. One round prunes
        // down to 70 % full; the next rounds (one auto each) prune nothing for a while.
        let min_free = crate::settings::Settings::default().policy(0).min_free_permille;
        let (out, _, list) = pool_rounds(min_free, None, 12, 70); // 71 * 0.7 % + 50 %: 0.3 % free
        assert!(out[0].pruned > 30 && (300..307).contains(&out[0].free), "one burst to 70 %: {:?}", out[0]);
        assert_eq!(bursts(&out[..6]), [0], "no second low-space round within 5 minutes: {out:?}");
        assert!(out.iter().all(|o| o.line.is_none()), "below 80 % full after the first round: no Warn line");
        assert!(list.iter().all(|s| s.auto));
    }

    #[test]
    fn manual_snapshots_filling_the_pool_warn_once() {
        let min_free = crate::settings::Settings::default().policy(0).min_free_permille;
        let (out, free, list) = pool_rounds(min_free, Some(10), 400, 0);
        let first = out.iter().position(|o| o.warning).expect("manual ones fill the pool: a warning");
        assert!(out[first..].iter().all(|o| o.warning), "they are never pruned: the warning stays until Georg acts");
        assert_eq!(out.iter().filter_map(|o| o.line).collect::<Vec<_>>(), [true], "one Warn line");
        assert!(free < WARN_FREE_PERMILLE);
        assert_eq!(list.iter().filter(|s| s.auto).count(), 1, "every auto but the newest went first");
        let sp = check(&stats(free), &list);
        assert!(!sp.proposal.is_empty() && sp.proposal.iter().all(|c| c.kind == "manual"), "only manual ones are proposed");
        // Georg deletes manual ones, oldest first, until 75 % full or less: one line, quiet again.
        let free_of = |l: &[Snap]| 1000 - 500 - l.iter().map(|s| s.bytes.unwrap() / 100_000).sum::<u64>();
        let mut l = list;
        while free_of(&l) < REARM_FREE_PERMILLE {
            let oldest = l.iter().position(|s| !s.auto).unwrap();
            l.remove(oldest);
        }
        let w = AtomicBool::new(true);
        let back = check(&stats(free_of(&l)), &l);
        assert!(matches!(crossing(&w, &back), Some((false, _))));
    }

    #[test]
    fn exactly_75_percent_is_quiet_on_both_sides() {
        // Pruning starts below 25 % free and the warning re-arms at >= 25 %: equality counts as quiet for both.
        let its = [Item { id: 1, created_s: T0, auto: true }, Item { id: 2, created_s: T0 + 60, auto: true }];
        let (gone, free) = prune_for_space(&its, &mut Vec::new(), 250, 250, false, |_| Ok::<_, ()>(Freed::Now(999))).unwrap();
        assert!(gone.is_empty() && free == 250);
        let (gone, _) = prune_for_space(&its, &mut Vec::new(), 250, 249, false, |_| Ok::<_, ()>(Freed::Now(260))).unwrap();
        assert_eq!(gone, [1], "the older auto goes, the newest stays");
        let w = AtomicBool::new(true);
        assert!(!check(&stats(250), &[]).warning);
        assert!(matches!(crossing(&w, &check(&stats(250), &[])), Some((false, _))));
    }

    #[test]
    fn hysteresis_between_75_and_80_percent() {
        let w = AtomicBool::new(false);
        assert!(matches!(crossing(&w, &check(&stats(199), &[])), Some((true, _))));
        // 79.9 % .. 75.1 % full: still the same crossing, no line either way.
        for free in [200, 230, 249] {
            assert!(crossing(&w, &check(&stats(free), &[])).is_none(), "{free}");
        }
        assert!(crossing(&w, &check(&stats(190), &[])).is_none(), "back above 80 % within the crossing: no new Warn");
        assert!(matches!(crossing(&w, &check(&stats(250), &[])), Some((false, t)) if t.contains("below 75 % full again")));
        assert!(matches!(crossing(&w, &check(&stats(199), &[])), Some((true, _))), "re-armed");
    }

    #[test]
    fn same_failure_said_once() {
        let m = Mutex::new(String::new());
        assert!(new_failure(&m, Some("busy")));
        assert!(!new_failure(&m, Some("busy")), "every round the same: once");
        assert!(new_failure(&m, Some("timeout")), "a different one is said");
        assert!(!new_failure(&m, None));
        assert!(new_failure(&m, Some("timeout")), "after a good check it is said again");
    }

    #[test]
    fn sizes_asked_only_for_the_proposed() {
        // Listed without sizes: 50 autos and 4 manual ones; 3 MB each brings 85 % back to 80 % after two.
        let mut l: Vec<Snap> = (0..50).map(|i| snap(100 + i, i, true, "auto", 0)).collect();
        l.extend((0..4).map(|i| snap(10 + i, 100 + i, false, "m", 0)));
        for s in &mut l {
            s.bytes = None;
        }
        let mut asked = Vec::new();
        let sp = check_sized(&stats(150), &l, |id| {
            asked.push(id);
            Some(3 * MB)
        });
        assert_eq!(asked, [10, 11], "no size command for autos or beyond the target");
        assert_eq!(ids(&sp), [10, 11]);
        assert_eq!(sp.proposal_bytes, 6 * MB);
        // Below 80 %: not a single one.
        asked.clear();
        assert!(!check_sized(&stats(300), &l, |id| {
            asked.push(id);
            None
        })
        .warning);
        assert!(asked.is_empty());
    }

    #[test]
    fn status_says_warning_and_proposal() {
        let l = [snap(2, 0, true, "auto", 50), snap(4, 10, false, "before-restore", 3), snap(5, 20, false, "vs6", 9)];
        let st = Stats { utc_offset_min: 120, last_pause_us: 4000, ..stats(150) };
        let out = status_lines(&st, &l, &check(&st, &l));
        assert_eq!(out[0], "data pool: 85.0 % full, 15.0 % free of 100.0 MB", "{out:?}");
        assert!(out[1].starts_with("snapshots: 3 (1 automatic, 2 manual"), "{out:?}");
        assert!(out[3].starts_with("WARNING: the data pool is 85.0 % full"), "{out:?}");
        assert_eq!(out[4], "  proposed: 4 (2026-10-19 02:10, before-restore \"before-restore\", frees 3.0 MB)");
        assert_eq!(out[5], "  proposed: 5 (2026-10-19 02:20, manual \"vs6\", frees 9.0 MB)");
        assert_eq!(out.len(), 6);
        let fine = status_lines(&stats(500), &l, &check(&stats(500), &l));
        assert_eq!(fine.last().unwrap(), "space: fine (a warning comes above 80 % full)");
    }
}
