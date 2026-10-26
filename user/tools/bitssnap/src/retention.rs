//! Which automatic snapshots go (V2-PLAN §1a B1, settings `snap.*`): all of the last 24 h stay, then the newest
//! of each hour for 7 days, of each day for 30 days, of each week (Monday to Sunday) for 6 months; older ones
//! go. Hours, days and weeks are local time (the kernel's UTC offset), so "yesterday" is Georg's yesterday.
//! When the pool's free space is below the minimum, the oldest automatic snapshots go first, one at a time,
//! until 5 % more than the minimum is free (one burst, VS-6c), but never the newest one. Snapshots taken by
//! hand (and the "before restore" ones) are never touched here;
//! the pool itself only ever drops automatic ones too (bitsvol `make_room`).

/// A month for the weekly rule: the mean Gregorian month (365.2425 d / 12).
pub const MONTH_S: u64 = 2_629_746;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub keep_all_s: u64,
    pub hourly_s: u64,
    pub daily_s: u64,
    pub weekly_s: u64,
    /// Below this much free space (per mille of the pool) the oldest go first.
    pub min_free_permille: u64,
    /// Local time = UTC + this.
    pub utc_offset_s: i64,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { keep_all_s: 24 * 3600, hourly_s: 7 * 86400, daily_s: 30 * 86400, weekly_s: 6 * MONTH_S, min_free_permille: crate::settings::MIN_FREE_PCT.unsigned_abs() * 10, utc_offset_s: 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: u64,
    /// UTC seconds; 0 = taken without a wall clock (age unknown: kept by the age rules).
    pub created_s: u64,
    pub auto: bool,
}

fn local(p: &Policy, t: u64) -> u64 {
    (t as i64).saturating_add(p.utc_offset_s).max(0) as u64
}

fn hour(p: &Policy, t: u64) -> u64 {
    local(p, t) / 3600
}
fn day(p: &Policy, t: u64) -> u64 {
    local(p, t) / 86400
}
/// 1970-01-01 was a Thursday: day + 3 puts Monday at the start of each week.
fn week(p: &Policy, t: u64) -> u64 {
    (day(p, t) + 3) / 7
}

/// The automatic snapshots the age rules drop at `now_s`, oldest first.
pub fn expired(items: &[Item], now_s: u64, p: &Policy) -> Vec<u64> {
    let mut auto: Vec<&Item> = items.iter().filter(|i| i.auto).collect();
    auto.sort_by_key(|i| (i.created_s, i.id));
    // The newest of each bucket: later items overwrite earlier ones.
    let newest = |key: &dyn Fn(u64) -> u64| {
        let mut m = std::collections::BTreeMap::new();
        for i in auto.iter().filter(|i| i.created_s != 0) {
            m.insert(key(i.created_s), i.id);
        }
        m.into_values().collect::<std::collections::BTreeSet<u64>>()
    };
    let (h, d, w) = (newest(&|t| hour(p, t)), newest(&|t| day(p, t)), newest(&|t| week(p, t)));
    auto.iter()
        .filter(|i| {
            if i.created_s == 0 {
                return false;
            }
            // A clock set back: age 0, kept.
            let age = now_s.saturating_sub(i.created_s);
            let keep = age < p.keep_all_s || (age < p.hourly_s && h.contains(&i.id)) || (age < p.daily_s && d.contains(&i.id)) || (age < p.weekly_s && w.contains(&i.id));
            !keep
        })
        .map(|i| i.id)
        .collect()
}

/// 2026-10-01 00:00 UTC: no snapshot of V2 is older, so a clock before it is unset, not real.
pub const CLOCK_FLOOR_S: u64 = 1_790_812_800;

/// What the age rules delete in one round of `serve`, with the reason when that is less than `expired` says.
/// The wall clock is their only measure, so: nothing while it looks wrong (before `CLOCK_FLOOR_S`, or behind the
/// newest snapshot), and at most an eighth of the automatic snapshots (at least one) per round, oldest first,
/// so a clock set far ahead by mistake costs at most that much before it is corrected, while a real backlog
/// (the PC was off for a week) goes within a few rounds.
pub fn age_round(items: &[Item], now_s: u64, p: &Policy) -> (Vec<u64>, Option<String>) {
    let newest = items.iter().map(|i| i.created_s).max().unwrap_or(0);
    if now_s < CLOCK_FLOOR_S {
        return (Vec::new(), Some(format!("the clock says {now_s}, before 2026-10-01: no age rules until it is set")));
    }
    if now_s < newest {
        return (Vec::new(), Some(format!("the clock ({now_s}) is behind the newest snapshot ({newest}): no age rules this round")));
    }
    let mut gone = expired(items, now_s, p);
    let cap = (items.iter().filter(|i| i.auto).count() / 8).max(1);
    if gone.len() > cap {
        let why = format!("{} snapshots past their age, {cap} of them this round (the rest in the next rounds)", gone.len());
        gone.truncate(cap);
        return (gone, Some(why));
    }
    (gone, None)
}

/// The next automatic snapshot to drop for space (the oldest, unknown times first), never the newest one and
/// none of `gone`.
pub fn for_space(items: &[Item], gone: &[u64]) -> Option<u64> {
    let mut auto: Vec<&Item> = items.iter().filter(|i| i.auto && !gone.contains(&i.id)).collect();
    auto.sort_by_key(|i| (i.created_s, i.id));
    auto.pop();
    auto.first().map(|i| i.id)
}

/// What deleting one snapshot for space came to.
pub enum Freed {
    /// Deleted; this much (per mille) is free now.
    Now(u64),
    /// Mounted for a restore right now (EBUSY): it stays, the next one is tried.
    Busy,
}

/// How far below the minimum a space round prunes (per mille of the pool): the default 25 % free prunes to 30 %
/// (75 % -> 70 % full, VS-6c). Stopping right at the minimum left the pool at 74.7-75.1 % full, so the next
/// automatic snapshot pushed it below again and every few minutes a round pruned one more (Mini-PC wave 13).
/// 50 per mille is about seven automatic snapshots of an idle desktop: one burst, then quiet for a while.
pub const SPACE_BAND_PERMILLE: u64 = 50;

/// What a space round prunes up to once free space is below `min_free_permille`: the band above it, at most the
/// whole pool. 0 stays 0 (`snap.min_free_pct` 0 = no pruning for space).
pub fn space_target(min_free_permille: u64) -> u64 {
    if min_free_permille == 0 { 0 } else { min_free_permille.saturating_add(SPACE_BAND_PERMILLE).min(1000) }
}

/// The space rule of a round: below `min_free_permille` free, the oldest automatic snapshot goes, one at a time,
/// until `space_target` is free (hysteresis: one burst, not a snapshot every round) or only the newest automatic
/// one is left (manual and before-restore ones never go here; the space warning tells Georg about them). At or
/// above the minimum nothing goes. `free` is the per mille free now, `gone` what the round removed already.
/// Returns what went and the per mille free after it; a dry run names the first candidate only.
pub fn prune_for_space<E>(
    items: &[Item],
    gone: &mut Vec<u64>,
    min_free_permille: u64,
    mut free: u64,
    dry_run: bool,
    mut delete: impl FnMut(u64) -> Result<Freed, E>,
) -> Result<(Vec<u64>, u64), E> {
    let mut removed = Vec::new();
    if free >= min_free_permille {
        return Ok((removed, free));
    }
    let target = space_target(min_free_permille);
    while free < target {
        let Some(id) = for_space(items, gone) else { break };
        gone.push(id);
        if dry_run {
            removed.push(id);
            break;
        }
        if let Freed::Now(f) = delete(id)? {
            removed.push(id);
            free = f;
        }
    }
    Ok((removed, free))
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3600;
    const D: u64 = 86400;
    /// Monday 2026-10-19 00:00 UTC.
    const MON: u64 = 1_792_368_000;

    fn auto(id: u64, t: u64) -> Item {
        Item { id, created_s: t, auto: true }
    }

    #[test]
    fn monday_starts_the_week() {
        let p = Policy::default();
        assert_eq!(MON % D, 0);
        assert_eq!(week(&p, MON), week(&p, MON + 6 * D + 23 * H));
        assert_ne!(week(&p, MON - 1), week(&p, MON));
        // Local time: one hour east, Sunday 23:30 UTC is already Monday.
        let east = Policy { utc_offset_s: 3600, ..p };
        assert_eq!(week(&east, MON - 1800), week(&east, MON));
    }

    #[test]
    fn last_day_kept_whole_then_the_newest_per_hour() {
        let p = Policy::default();
        let now = MON + 10 * D;
        // Every 5 minutes for the last 48 hours.
        let items: Vec<Item> = (0..48 * 12).map(|k| auto(k + 1, now - k * 300)).collect();
        let gone = expired(&items, now, &p);
        for i in &items {
            let age = now - i.created_s;
            if age < D {
                assert!(!gone.contains(&i.id), "within 24 h: {i:?}");
            }
        }
        // Between 24 and 48 h: one per hour stays (the newest of the hour).
        let kept_old: Vec<&Item> = items.iter().filter(|i| now - i.created_s >= D && !gone.contains(&i.id)).collect();
        assert_eq!(kept_old.len(), 24, "{kept_old:?}");
        for k in kept_old {
            assert!(items.iter().all(|o| hour(&p, o.created_s) != hour(&p, k.created_s) || o.created_s <= k.created_s));
        }
    }

    #[test]
    fn manual_and_timeless_are_never_expired() {
        let p = Policy::default();
        let now = MON + 400 * D;
        let items = [Item { id: 1, created_s: MON, auto: false }, auto(2, 0), auto(3, MON)];
        assert_eq!(expired(&items, now, &p), vec![3]);
        assert_eq!(for_space(&items, &[]), Some(2), "unknown time counts as oldest");
        assert_eq!(for_space(&items, &[2]), None, "the newest automatic one stays");
    }

    #[test]
    fn clock_set_back_keeps_everything() {
        let p = Policy::default();
        let items: Vec<Item> = (0..50).map(|k| auto(k + 1, MON + k * 300)).collect();
        assert!(expired(&items, MON - 30 * D, &p).is_empty());
    }

    #[test]
    fn a_wrong_clock_deletes_little() {
        let p = Policy::default();
        let items: Vec<Item> = (0..400).map(|k| auto(k + 1, MON - 200 * D + k * 6 * H)).collect();
        let all = expired(&items, MON + 365 * D, &p);
        assert!(all.len() > 300, "a year ahead: {} expired", all.len());
        // A clock a year ahead: an eighth per round, oldest first.
        let (gone, why) = age_round(&items, MON + 365 * D, &p);
        assert_eq!(gone, all[..50].to_vec());
        assert!(why.is_some());
        // Unset (1970) or behind the newest snapshot: nothing.
        assert!(age_round(&items, 1000, &p).0.is_empty() && age_round(&items, 1000, &p).1.is_some());
        let newest = items.last().unwrap().created_s;
        assert!(age_round(&items, newest - 1, &p).0.is_empty());
        // Within the cap: everything `expired` says, no reason; one past its age still goes (at least one).
        let one = [auto(1, MON - 300 * D), auto(2, MON)];
        assert_eq!(age_round(&one, MON, &p), (vec![1], None));
    }

    #[test]
    fn for_space_goes_oldest_first() {
        let items: Vec<Item> = (0..5).map(|k| auto(10 - k, MON + k * 300)).collect();
        let mut gone = Vec::new();
        while let Some(v) = for_space(&items, &gone) {
            gone.push(v);
        }
        assert_eq!(gone, vec![10, 9, 8, 7]);
    }

    #[test]
    fn space_target_at_the_edges() {
        assert_eq!(space_target(250), 300, "the default: below 75 % full again means down to 70 %");
        assert_eq!(space_target(0), 0, "snap.min_free_pct 0: no pruning for space");
        assert_eq!(space_target(900), 950);
        assert_eq!(space_target(980), 1000, "never more than the whole pool");
        assert_eq!(space_target(u64::MAX), 1000);
    }

    /// Each delete frees 7 per mille. Returns (removed, free after).
    fn prune(items: &[Item], min_free: u64, free: u64, dry: bool, busy: &[u64]) -> (Vec<u64>, u64) {
        let mut f = free;
        prune_for_space(items, &mut Vec::new(), min_free, free, dry, |id| {
            if busy.contains(&id) {
                return Ok::<_, ()>(Freed::Busy);
            }
            f += 7;
            Ok(Freed::Now(f))
        })
        .unwrap()
    }

    #[test]
    fn space_rule_prunes_in_one_burst() {
        let items: Vec<Item> = (0..20).map(|k| auto(k + 1, MON + k * 60)).collect();
        // Just below 25 %: down to 30 % at once (249 + 8 * 7 = 305), oldest first.
        assert_eq!(prune(&items, 250, 249, false, &[]), ((1..=8).collect(), 305));
        // Between 25 % and 30 % nothing starts: the band is only the stopping point.
        assert_eq!(prune(&items, 250, 250, false, &[]), (vec![], 250));
        assert_eq!(prune(&items, 250, 299, false, &[]), (vec![], 299));
        // Off: nothing even on a full pool.
        assert_eq!(prune(&items, 0, 0, false, &[]), (vec![], 0));
        // Dry run: the first candidate only, as before.
        assert_eq!(prune(&items, 250, 100, true, &[]), (vec![1], 100));
        // A busy one stays and the next goes; the burst still reaches the target.
        assert_eq!(prune(&items, 250, 249, false, &[2]), (vec![1, 3, 4, 5, 6, 7, 8, 9], 305));
        // More than the autos can free: everything but the newest, then stop.
        assert_eq!(prune(&items, 900, 100, false, &[]), ((1..=19).collect(), 100 + 19 * 7));
        // Manual ones never go.
        let mixed = [Item { id: 1, created_s: MON, auto: false }, auto(2, MON + 60), auto(3, MON + 120)];
        assert_eq!(prune(&mixed, 250, 100, false, &[]), (vec![2], 107));
    }

    /// Eight months of a snapshot every 5 minutes with pruning after each (as the scheduler runs): every rule
    /// holds at every step, and the number kept stays bounded.
    #[test]
    fn eight_months_simulated() {
        let p = Policy { utc_offset_s: 7200, ..Policy::default() };
        let step = 300;
        let start = MON - 3 * H;
        let mut live: Vec<Item> = Vec::new();
        let mut all: Vec<Item> = Vec::new();
        let mut next = 1;
        let mut t = start;
        let end = start + 245 * D;
        let mut max_live = 0;
        while t < end {
            let it = auto(next, t);
            next += 1;
            live.push(it);
            all.push(it);
            // A manual one now and then.
            if next % 997 == 0 {
                let m = Item { id: next + 1_000_000, created_s: t, auto: false };
                live.push(m);
                all.push(m);
            }
            let gone = expired(&live, t, &p);
            live.retain(|i| !gone.contains(&i.id));
            max_live = max_live.max(live.len());
            // Check the rules every simulated 29 hours (the hour of day moves on; hundreds of checks).
            if (t - start) % (29 * H) == 0 {
                check(&all, &live, t, &p);
            }
            t += step;
        }
        check(&all, &live, t - step, &p);
        // 288 (24 h) + 6*24 (hourly) + 23 (daily) + ~26 (weekly) + a few at bucket edges + the manual ones.
        let manual = live.iter().filter(|i| !i.auto).count();
        assert!(max_live <= 288 + 144 + 23 + 27 + 5 + manual, "{max_live} kept at most ({manual} manual)");
    }

    fn check(all: &[Item], live: &[Item], now: u64, p: &Policy) {
        let has = |id| live.iter().any(|i| i.id == id);
        let ever: Vec<&Item> = all.iter().filter(|i| i.auto && i.created_s <= now).collect();
        for i in &ever {
            let age = now - i.created_s;
            if age < p.keep_all_s {
                assert!(has(i.id), "24 h rule at {now}: {i:?}");
            }
            if age >= p.weekly_s {
                assert!(!has(i.id), "older than 6 months kept: {i:?}");
            }
        }
        // For each bucket kind: a bucket whose newest snapshot is inside the span still has one kept.
        type Bucket = fn(&Policy, u64) -> u64;
        for (span, key) in [(p.hourly_s, hour as Bucket), (p.daily_s, day as Bucket), (p.weekly_s, week as Bucket)] {
            let mut newest = std::collections::BTreeMap::new();
            for i in &ever {
                let e = newest.entry(key(p, i.created_s)).or_insert(i.created_s);
                *e = (*e).max(i.created_s);
            }
            let kept: std::collections::BTreeSet<u64> = live.iter().filter(|i| i.auto).map(|i| key(p, i.created_s)).collect();
            for (b, t) in newest {
                if now - t < span {
                    assert!(kept.contains(&b), "bucket {b} empty at {now}");
                }
            }
        }
        // Manual ones always stay.
        for i in all.iter().filter(|i| !i.auto) {
            assert!(has(i.id));
        }
    }
}
