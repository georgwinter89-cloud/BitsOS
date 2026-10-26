//! `bitssnap serve`: the scheduler init starts (uid 0). Every `snap.interval_min` minutes (on the wall clock's
//! multiples: :00, :05, ...) an automatic snapshot of /data, skipped when nothing was written since the newest
//! one (the kernel checks while frozen); then the retention rules and the free-space rule. Beside it a thread
//! serves the MCP tools `snapshots` (read), `snapshot_take` (change), `snapshot_delete` and `restore`
//! (delete_overwrite; restore brings a file or folder of /data back from a snapshot). MCP's own `run` starts
//! programs as uid 1000, which /dev/bitssnap refuses: these tools (bitssnap runs as root) are Claude's way in.
//! Threads stay in the normal class: the snapshot thread holds /data frozen for its pause, and a
//! background-class holder would make every writer wait behind it (priority inversion).

use crate::dev::{Dev, Snap};
use crate::retention::{self, Item};
use crate::sched::{self, Look};
use crate::settings::{self, Settings};
use crate::space::{self, Space};
use crate::{local_time, log, restore};
use mcpreg::{Json, Reply, Safety, Service, Tool};
use std::time::Duration;

/// Where the kernel says what became of /data (`fs_ext4::data::STATE_FILE`).
pub const DATA_STATE: &str = "/run/bitsos/data-state";

#[derive(Debug, PartialEq, Eq)]
pub enum DataState {
    /// A writable pool: snapshots possible.
    Ready,
    /// The boot is still making or mounting it: look again soon.
    Pending,
    /// No snapshots on this boot (plain image, read-only, off, failed): the reason.
    Never(String),
}

pub fn data_state(text: Option<&str>) -> DataState {
    let Some(t) = text.map(str::trim) else { return DataState::Pending };
    if t.starts_with("mounted pool rw") {
        DataState::Ready
    } else if t.is_empty() || t == "waiting" || t == "creating" {
        DataState::Pending
    } else {
        DataState::Never(t.to_string())
    }
}

/// What one round did.
#[derive(Debug, Default)]
pub struct Round {
    pub taken: Option<(u64, bool, u64)>,
    pub deleted_age: Vec<u64>,
    pub deleted_space: Vec<u64>,
    pub free_permille: u64,
    /// The space rule's trigger (`snap.min_free_pct` in per mille) and where it prunes to.
    pub min_free_permille: u64,
}

/// The candidates of the retention rules: never the snapshot a rollback is scheduled for (the pool refuses to
/// delete it; listed, it would take a slot of every round's bounded pruning until the next start).
fn items(l: &[Snap]) -> Vec<Item> {
    l.iter().filter(|s| !s.rollback).map(|s| Item { id: s.id, created_s: s.created_ns / 1_000_000_000, auto: s.auto }).collect()
}

/// Takes the automatic snapshot (unless `take` is false) and prunes.
pub fn round(dev: &mut Dev, s: &Settings, take: bool, dry_run: bool) -> Result<Round, String> {
    let mut r = Round::default();
    if take {
        // EBUSY: writers did not drain within 10 s (a long write); one more try a moment later.
        let t = match dev.take("auto", true, true) {
            Err(e) if e.errno == 16 => {
                log(&format!("snapshot busy ({e}); trying again in 20 s"));
                std::thread::sleep(Duration::from_secs(20));
                dev.take("auto", true, true)
            }
            other => other,
        }
        .map_err(|e| format!("snapshot: {e}"))?;
        r.taken = Some((t.id, t.new, t.pause_us));
    }
    let st = dev.stats().map_err(|e| e.to_string())?;
    let p = s.policy(st.utc_offset_min * 60);
    let now = if st.wall_ns > 0 { st.wall_ns / 1_000_000_000 } else { restore::now_s() };
    let list = dev.list(false).map_err(|e| e.to_string())?;
    let its = items(&list);
    let (aged, why) = retention::age_round(&its, now, &p);
    if let Some(w) = why {
        log(&w);
    }
    for id in aged {
        if !dry_run {
            match dev.delete(id) {
                Ok(()) => {}
                // Mounted for a restore right now: next round.
                Err(e) if e.errno == 16 => continue,
                Err(e) => return Err(format!("delete {id}: {e}")),
            }
        }
        r.deleted_age.push(id);
    }
    let mut gone = r.deleted_age.clone();
    let (free_now, warn) = free_for_space(!r.deleted_age.is_empty(), dry_run, st.free_permille, || dev.stats().map(|s| s.free_permille));
    if let Some(w) = warn {
        log(&w);
    }
    r.min_free_permille = p.min_free_permille;
    let (removed, free) = retention::prune_for_space(&its, &mut gone, p.min_free_permille, free_now, dry_run, |id| match dev.delete(id) {
        Ok(()) => dev.stats().map(|s| retention::Freed::Now(s.free_permille)).map_err(|e| e.to_string()),
        Err(e) if e.errno == 16 => Ok(retention::Freed::Busy),
        Err(e) => Err(format!("delete {id} for space: {e}")),
    })?;
    r.deleted_space = removed;
    r.free_permille = free;
    Ok(r)
}

/// The free space (per mille) the space rule starts from, and a line to log if the fresh reading failed. What the
/// age rules freed counts: with the band below the minimum (VS-6c) a stale reading would start a burst the pool
/// no longer needs. A failed re-read keeps the earlier reading instead of failing the round, so the age deletes
/// already done still reach the journal.
fn free_for_space<E: std::fmt::Display>(aged_any: bool, dry_run: bool, before: u64, reread: impl FnOnce() -> Result<u64, E>) -> (u64, Option<String>) {
    if !aged_any || dry_run {
        return (before, None);
    }
    match reread() {
        Ok(f) => (f, None),
        Err(e) => (before, Some(format!("stats after the age rules failed ({e}); space rule uses the reading from before them"))),
    }
}

fn describe(r: &Round) -> Option<String> {
    let mut parts = Vec::new();
    if let Some((id, true, pause)) = r.taken {
        parts.push(format!("snapshot {id} (pause {} ms)", pause / 1000));
    }
    if !r.deleted_age.is_empty() {
        parts.push(format!("retention removed {:?}", r.deleted_age));
    }
    if !r.deleted_space.is_empty() {
        let (f, t, d) = (r.free_permille, retention::space_target(r.min_free_permille), &r.deleted_space);
        // A burst can be a hundred ids (install boot) and the kernel log cuts lines at 240 bytes: numbers first.
        let ids = if d.len() <= 5 { format!("{d:?}") } else { format!("{} snapshots ({} .. {})", d.len(), d[0], d[d.len() - 1]) };
        parts.push(format!("low space (below {} % free): {}.{} % free now (target {} %), removed {ids}", r.min_free_permille / 10, f / 10, f % 10, t / 10));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The scheduler; `once`: one round (with the settings as they are) and back.
pub fn serve(once: bool) -> i32 {
    // Hidden copies of restores a crash, kill or Reload cut off.
    match restore::sweep() {
        Ok(g) if !g.is_empty() => log(&format!("removed left-over restore copies: {}", g.join(", "))),
        _ => {}
    }
    if let Ok((_, Some(id))) = Dev::open().and_then(|mut d| d.rollback_status()) {
        log(&format!("WARNING: this start could not roll /data back to snapshot {id} (klog volume.rollback_failed): it stays scheduled for the next start; cancel it with restore kind volume cancel=true"));
    }
    if !once {
        std::thread::Builder::new().name("bitssnap-mcp".into()).spawn(mcp_loop).ok();
        log("started: snapshots of /data every snap.interval_min minutes");
    }
    let mut said_never = String::new();
    let mut said_settings = None;
    let (mut migrated, mut said_migrate) = (false, None::<String>);
    // Settings and data state at a wake-up; the round uses the settings it fell due under.
    let mut look = || {
        let sock = std::env::var("SETTINGS_SOCKET").unwrap_or_else(|_| settings::SOCKET.into());
        let (mut s, why) = settings::load(&sock);
        // VS-6b: the old default 15 moves to 25 once per start (the journal keeps it from coming back); only
        // here, so `bitssnap status` never writes a setting behind Georg's back.
        if !migrated && why.is_none() {
            match settings::migrate(&sock) {
                Ok(line) => {
                    migrated = true;
                    if let Some(l) = line {
                        log(&l);
                        s = settings::load(&sock).0;
                    }
                }
                Err(e) if said_migrate.as_deref() != Some(e.as_str()) => {
                    log(&format!("settings: snap.min_free_pct not moved to the new default yet ({e}); trying again"));
                    said_migrate = Some(e);
                }
                Err(_) => {}
            }
        }
        if why.is_some() != said_settings.is_some() {
            if let Some(w) = &why {
                log(&format!("settings: the defaults hold ({w})"));
            }
            said_settings = why.clone();
        }
        let state = data_state(std::fs::read_to_string(DATA_STATE).ok().as_deref());
        if let (DataState::Never(why), true) = (&state, s.enabled)
            && *why != said_never
        {
            log(&format!("/data has no snapshots on this boot: {why}"));
            said_never = why.clone();
        }
        (look_of(&state, &s), s, state)
    };
    if once {
        let (l, s, state) = look();
        if !matches!(l, Look::Rounds(_)) {
            println!("no round: {state:?} (snap.enabled={})", s.enabled);
            return 1;
        }
        return if one_round(&s, true) { 0 } else { 1 };
    }
    // A failed round is not retried at the next wake-up: the next one comes at its time.
    auto_rounds(&mut sched::RealClock::new(), || {
        let (l, s, _) = look();
        (l, s)
    }, |s| {
        one_round(s, false);
    });
    0
}

/// What a wake-up finds: the loop's answer for this data state and these settings.
fn look_of(state: &DataState, s: &Settings) -> Look {
    match (state, s.enabled) {
        (_, false) => Look::Wait(60),
        (DataState::Pending, _) => Look::Wait(10),
        (DataState::Never(_), _) => Look::Wait(300),
        (DataState::Ready, true) => Look::Rounds(s.interval_min.saturating_mul(60)),
    }
}

/// `serve`'s loop: `look` at every wake-up, `round` with the settings the round fell due under, only when
/// `sched::run` says it is due (a host test drives it with a fake clock).
fn auto_rounds(clock: &mut impl sched::Clock, mut look: impl FnMut() -> (Look, Settings), mut round: impl FnMut(&Settings)) {
    let latest = std::cell::Cell::new(Settings::default());
    sched::run(clock, || {
        let (l, s) = look();
        latest.set(s);
        l
    }, || round(&latest.get()));
}

fn one_round(s: &Settings, once: bool) -> bool {
    let r = Dev::open().map_err(|e| e.to_string()).and_then(|mut d| Ok((round(&mut d, s, true, false)?, space::read(&mut d))));
    match &r {
        Ok((r, sp)) => {
            if let Some(t) = describe(r) {
                log(&t);
            }
            // The space warning after the pruning (VS-6); a failed look is said, the round still counts.
            match sp {
                Ok((_, sp)) => space::notice(sp),
                Err(e) => space::failed(e),
            }
            if once {
                println!("{}", describe(r).unwrap_or_else(|| "nothing changed since the newest snapshot".into()));
                if let Ok((st, sp)) = sp
                    && let Some(w) = space::warning_text(sp, st.utc_offset_min * 60)
                {
                    println!("warning: {w}");
                }
            }
        }
        Err(e) => log(&format!("round failed: {e}")),
    }
    r.is_ok()
}

const SNAPSHOTS_SCHEMA: &str = r#"{"type":"object","properties":{"sizes":{"type":"boolean","description":"also each snapshot's own size (bytes a delete would free); default true"}}}"#;
const TAKE_SCHEMA: &str = r#"{"type":"object","properties":{"label":{"type":"string","description":"name of the snapshot (letters, digits, . _ : -), default manual"}}}"#;
const DELETE_SCHEMA: &str = r#"{"type":"object","properties":{"snapshot":{"type":"integer","description":"snapshot id (tool snapshots)"}},"required":["snapshot"]}"#;

pub fn tools() -> Vec<Tool> {
    let t = |n, d, s, safety| Tool::new(n, d, s, safety).expect("schema is JSON");
    vec![
        t("snapshots", "Snapshots of /data (every 5 minutes, B1): id, local time, kind (auto, manual, before-restore, before-rollback), own size, rollback (true on the one /data goes back to at the next start; rollback_not_done names it when this start could not carry it out, it is tried again at the next one), plus free space and freeze pauses. Above 80 % full: warning true and a proposal (the oldest manual and before-restore snapshots, which are never deleted automatically, with the space each frees); delete only what Georg agrees to, with snapshot_delete.", SNAPSHOTS_SCHEMA, Safety::Read),
        t("snapshot_take", "Take a snapshot of /data now (kept until deleted; answers its id and the freeze pause).", TAKE_SCHEMA, Safety::Change),
        t("snapshot_delete", "Delete one snapshot of /data (what only it holds is gone).", DELETE_SCHEMA, Safety::DeleteOverwrite),
        t("restore", crate::restore_tool::DESCRIPTION, crate::restore_tool::SCHEMA, Safety::DeleteOverwrite),
    ]
}

/// The `snapshots` answer (JSON text).
pub fn snapshots_json(dev: &mut Dev, sizes: bool) -> Result<String, String> {
    let st = dev.stats().map_err(|e| e.to_string())?;
    let l = dev.list(sizes).map_err(|e| e.to_string())?;
    let off = st.utc_offset_min * 60;
    // The proposal needs sizes: without them in the listing, a `size` command only for each proposed one.
    let sp = space::check_sized(&st, &l, |id| dev.size(id).ok());
    space::notice(&sp);
    let snaps: Vec<Json> = l
        .iter()
        .map(|s| {
            let mut v = vec![
                ("id", Json::Num(s.id as f64)),
                ("time", Json::str(&local_time(s.created_ns / 1_000_000_000, off))),
                ("created_s", Json::Num((s.created_ns / 1_000_000_000) as f64)),
                ("kind", Json::str(s.kind())),
                ("label", Json::str(&s.label)),
                ("mounted", Json::Bool(s.mounted)),
                ("rollback", Json::Bool(s.rollback)),
            ];
            if let Some(b) = s.bytes {
                v.push(("bytes", Json::Num(b as f64)));
            }
            Json::obj(v)
        })
        .collect();
    let pending = l.iter().find(|s| s.rollback).map_or(Json::Null, |s| Json::Num(s.id as f64));
    // Scheduled before this start, which could not carry it out (klog volume.rollback_failed).
    let not_done = match dev.rollback_status() {
        Ok((_, Some(id))) => Json::Num(id as f64),
        _ => Json::Null,
    };
    Ok(Json::obj(vec![
        ("snapshots", Json::Arr(snaps)),
        ("rollback_pending", pending),
        ("rollback_not_done", not_done),
        ("free_percent", Json::Num(st.free_permille as f64 / 10.0)),
        ("last_pause_us", Json::Num(st.last_pause_us as f64)),
        ("max_pause_us", Json::Num(st.max_pause_us as f64)),
        ("used_percent", Json::Num((1000 - st.free_permille.min(1000)) as f64 / 10.0)),
        ("warning", Json::Bool(sp.warning)),
        ("warning_text", space::warning_text(&sp, off).map_or(Json::Null, |t| Json::str(&t))),
        ("proposal", proposal_json(&sp, off)),
        ("proposal_bytes", Json::Num(sp.proposal_bytes as f64)),
    ])
    .to_string())
}

/// The proposal's entries: what Georg may delete (with snapshot_delete) to bring the pool below 80 % again.
fn proposal_json(sp: &Space, off: i64) -> Json {
    Json::Arr(
        sp.proposal
            .iter()
            .map(|c| {
                let mut v = vec![
                    ("id", Json::Num(c.id as f64)),
                    ("time", Json::str(&local_time(c.created_s, off))),
                    ("created_s", Json::Num(c.created_s as f64)),
                    ("kind", Json::str(c.kind)),
                    ("label", Json::str(&c.label)),
                ];
                if let Some(b) = c.bytes {
                    v.push(("bytes", Json::Num(b as f64)));
                }
                Json::obj(v)
            })
            .collect(),
    )
}

pub fn answer(name: &str, args: &Json) -> Reply {
    let mut dev = match Dev::open() {
        Ok(d) => d,
        Err(e) => return Reply::error(e.to_string()),
    };
    match name {
        "snapshots" => {
            let sizes = args.get("sizes").and_then(Json::as_bool).unwrap_or(true);
            match snapshots_json(&mut dev, sizes) {
                Ok(t) => Reply::text(t),
                Err(e) => Reply::error(e),
            }
        }
        "snapshot_take" => {
            let label = args.get("label").and_then(Json::as_str).unwrap_or("manual");
            match dev.take(label, false, false) {
                Ok(t) => Reply::text(Json::obj(vec![("id", Json::Num(t.id as f64)), ("pause_us", Json::Num(t.pause_us as f64))]).to_string()),
                Err(e) if e.errno == 22 => Reply::bad_argument(format!("label {label:?}: letters, digits and . _ : - only, at most 64")),
                Err(e) => Reply::error(e.to_string()),
            }
        }
        "snapshot_delete" => {
            let Some(id) = args.get("snapshot").and_then(Json::as_u64) else { return Reply::bad_argument("want snapshot (an id)") };
            match dev.delete(id) {
                Ok(()) => {
                    log(&format!("MCP deleted snapshot {id}"));
                    Reply::text(Json::obj(vec![("deleted", Json::Num(id as f64))]).to_string())
                }
                Err(e) => Reply::error(e.to_string()),
            }
        }
        "restore" => crate::restore_tool::answer(&mut dev, args),
        other => Reply::bad_argument(format!("unknown tool {other}")),
    }
}

/// Registers the tools and answers them; reconnects with a back-off when the kernel's registry goes away.
fn mcp_loop() {
    let mut delay = 1;
    let mut said = String::new();
    loop {
        let r = Service::connect("bitssnap").and_then(|mut s| {
            s.register(&tools())?;
            log("MCP tools snapshots, snapshot_take, snapshot_delete and restore registered");
            delay = 1;
            s.serve(|c, _| answer(&c.name, &c.arguments))
        });
        if let Err(e) = r {
            let m = e.to_string();
            if m != said {
                log(&format!("MCP: {m}; retrying"));
                said = m;
            }
        }
        std::thread::sleep(Duration::from_secs(delay));
        delay = (delay * 2).min(60);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_space_line_names_the_target() {
        let r = |d: Vec<u64>| Round { deleted_space: d, free_permille: 301, min_free_permille: 250, ..Round::default() };
        assert_eq!(describe(&r(vec![7, 8])).unwrap(), "low space (below 25 % free): 30.1 % free now (target 30 %), removed [7, 8]");
        let t = describe(&r((297..=398).collect())).unwrap();
        assert_eq!(t, "low space (below 25 % free): 30.1 % free now (target 30 %), removed 102 snapshots (297 .. 398)");
        assert!(describe(&Round::default()).is_none());
    }

    #[test]
    fn space_rule_starts_from_a_fresh_reading_after_the_age_rules() {
        let flag = std::cell::Cell::new(false);
        let called = &flag;
        let ok = move |v| {
            move || {
                called.set(true);
                Ok::<u64, String>(v)
            }
        };
        // Nothing aged out: the reading from before stands, no second stats call.
        assert_eq!(free_for_space(false, false, 240, ok(310)), (240, None));
        assert!(!called.get());
        // Dry run: nothing was really deleted, so nothing to re-read.
        assert_eq!(free_for_space(true, true, 240, ok(310)), (240, None));
        assert!(!called.get());
        // Aged out: the fresh reading, so a pool the age rules already freed starts no burst.
        assert_eq!(free_for_space(true, false, 240, ok(310)), (310, None));
        assert!(called.get());
        // The re-read fails: the round goes on with the earlier reading and says so.
        let (f, w) = free_for_space(true, false, 240, || Err::<u64, _>("EIO"));
        assert_eq!(f, 240);
        assert!(w.unwrap().contains("EIO"));
        // The space rule with that fresh reading: age-deleted ids in `gone`, free already at 31 %: nothing goes.
        let its: Vec<Item> = (1..=10).map(|id| Item { id, created_s: id * 60, auto: true }).collect();
        let mut gone = vec![1, 2, 3];
        let (removed, free) = retention::prune_for_space(&its, &mut gone, 250, 310, false, |_| Ok::<_, ()>(retention::Freed::Now(0))).unwrap();
        assert_eq!((removed, free, gone), (vec![], 310, vec![1, 2, 3]));
        // The stale reading (24 %) would prune on top of the age deletes; those ids are never counted twice.
        let mut gone = vec![1, 2, 3];
        let mut f = 240;
        let (removed, _) = retention::prune_for_space(&its, &mut gone, 250, 240, false, |_| {
            f += 7;
            Ok::<_, ()>(retention::Freed::Now(f))
        })
        .unwrap();
        assert_eq!(removed[0], 4);
    }

    #[test]
    fn retention_never_sees_the_rollback_target() {
        let s = |id, rollback| Snap { id, created_ns: id * 1_000_000_000, auto: true, mounted: false, rollback, bytes: None, label: String::new() };
        let its = items(&[s(1, false), s(2, true), s(3, false)]);
        assert_eq!(its.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 3]);
    }

    #[test]
    fn data_states() {
        assert_eq!(data_state(Some("mounted pool rw\n")), DataState::Ready);
        assert_eq!(data_state(Some("mounted pool rw created\n")), DataState::Ready);
        assert_eq!(data_state(Some("waiting\n")), DataState::Pending);
        assert_eq!(data_state(None), DataState::Pending);
        assert_eq!(data_state(Some("mounted plain rw\n")), DataState::Never("mounted plain rw".into()));
        assert_eq!(data_state(Some("mounted pool ro\n")), DataState::Never("mounted pool ro".into()));
        assert!(matches!(data_state(Some("skipped no room\n")), DataState::Never(_)));
    }

    #[test]
    fn tool_schemas() {
        let t = tools();
        let names: Vec<(&str, Safety)> = t.iter().map(|t| (t.name.as_str(), t.safety)).collect();
        assert_eq!(names, [("snapshots", Safety::Read), ("snapshot_take", Safety::Change), ("snapshot_delete", Safety::DeleteOverwrite), ("restore", Safety::DeleteOverwrite)]);
    }

    /// The serve loop as `serve` wires it (look_of + auto_rounds + sched::run): two hours at 5 minutes are
    /// 24 rounds, not one per wake-up (desk-minipc-w11.md S3), each with the settings it fell due under.
    #[test]
    fn serve_takes_a_round_per_interval_not_per_wakeup() {
        struct Fake(u64, u64);
        impl sched::Clock for Fake {
            fn now_s(&mut self) -> u64 {
                self.0
            }
            fn sleep_s(&mut self, s: u64) -> bool {
                assert!((1..=sched::WAKE_S).contains(&s));
                self.0 += s;
                self.0 < self.1
            }
        }
        const T0: u64 = 1_800_000_000;
        let (mut wakeups, mut rounds) = (0, Vec::new());
        let s = Settings::default();
        auto_rounds(&mut Fake(T0 + 17, T0 + 7200), || {
            wakeups += 1;
            (look_of(&DataState::Ready, &s), s)
        }, |got| rounds.push(got.interval_min));
        assert_eq!(rounds.len(), 24, "{wakeups} wake-ups");
        assert!(wakeups >= 120 && rounds.iter().all(|&i| i == 5));
        let off = Settings { enabled: false, ..s };
        let mut n = 0;
        auto_rounds(&mut Fake(T0, T0 + 3600), || (look_of(&DataState::Ready, &off), off), |_| n += 1);
        auto_rounds(&mut Fake(T0, T0 + 3600), || (look_of(&DataState::Pending, &s), s), |_| n += 1);
        assert_eq!(n, 0, "disabled or pending: no rounds");
    }
}
