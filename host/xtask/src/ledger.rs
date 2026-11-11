//! The ledger of `cargo xtask test` (TS-1; Georg 30.10., CONTRACTS section 4): from each run's result, which tests ran.
//! A test that ran in no run is RED; a test that belongs to a Mini-PC run counts through `host/tests/minipc/ran.txt`,
//! which the desk fills after the round (`host/tests/minipc/ktests.py --record`): its count is shown, a test without a
//! fresh record is a "stale" line, not red.
//!
//! The list of all tests is what the plain boot says about them, and it also names the runs: a test of another run
//! prints `KTEST skip <name>: class <class> belongs to another run`.
use crate::ktest::{self, Summary};
use crate::util::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The file the desk fills: `<test> <run> <yyyy-mm-dd> <generation>` per line, `#` comments.
pub const MINIPC_FILE: &str = "host/tests/minipc/ran.txt";
/// A record older than this is stale: the test may have changed since the machine last ran it.
const STALE_DAYS: i64 = 30;
/// Boots of the gate's own table (`Run("gate/...")`), not scripts.
const GATE_PREFIX: &str = "gate/";

/// Where a test belongs, as far as the ledger cares.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Home {
    /// Any class whose run is the plain or the release boot; it shows as `ok` or `FAIL` there, or a QEMU-only class.
    Gate,
    /// `Run`/`RunHw`: that boot.
    Named(String),
    /// Only the real machine runs it.
    MiniPc,
}

/// `class Run("nv/10-nvme") belongs to another run` -> where the test belongs. `None`: not such a line.
fn home_of(why: &str) -> Option<Home> {
    let class = why.strip_prefix("class ")?.strip_suffix(" belongs to another run")?;
    if let Some(r) = class.strip_prefix("Run(\"").or_else(|| class.strip_prefix("RunHw(\"")) {
        return Some(Home::Named(r.trim_end_matches("\")").to_string()));
    }
    Some(if matches!(class, "Program" | "Hardware" | "Picture" | "WithGeorg") { Home::MiniPc } else { Home::Gate })
}

/// True if `why` is the kernel's line for a test of another run; anything else is a test that skipped itself.
pub fn is_other_run(why: &str) -> bool {
    home_of(why).is_some()
}

fn dir(root: &Path) -> PathBuf {
    xdir(root).join("ledger")
}

/// A fresh ledger for this gate.
pub fn start(root: &Path) -> Result<(), String> {
    let d = dir(root);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))
}

/// Remembers the tests that passed in boot `name`, whoever started it (the gate, a scenario, a person by hand).
/// Nothing depends on this working: a boot whose names did not arrive shows in the ledger as a test that did not run.
pub fn record_boot(root: &Path, name: &str, log: &str) {
    let s = ktest::parse(log);
    if s.ok.is_empty() {
        return;
    }
    let d = dir(root);
    let _ = std::fs::create_dir_all(&d);
    let mut text = s.ok.join("\n");
    text.push('\n');
    // One write in append mode: two boots of one name in parallel (a script's two QEMUs) must not lose each other's.
    let f = std::fs::OpenOptions::new().append(true).create(true).open(d.join(format!("{name}.ok")));
    let _ = f.and_then(|mut f| f.write_all(text.as_bytes()));
}

/// The serial logs a scenario keeps in its own folder: a script that kills `xtask qemu` itself leaves no other trace.
pub fn scan_run_dir(root: &Path, script: &str, run_dir: &Path) {
    let mut names = String::new();
    let mut todo = vec![run_dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                todo.push(p);
            } else if p.extension().is_some_and(|x| x == "log" || x == "txt") && p.metadata().is_ok_and(|m| m.len() < 64 << 20) {
                let log = String::from_utf8_lossy(&std::fs::read(&p).unwrap_or_default()).into_owned();
                for n in ktest::parse(&log).ok {
                    names.push_str(&n);
                    names.push('\n');
                }
            }
        }
    }
    if !names.is_empty() {
        let d = dir(root);
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::write(d.join(format!("scenario-{}.ok", script.replace('/', "-"))), names);
    }
}

/// The scripts the plain boot's classes name: `host/tests/<name>.sh`.
pub fn scripts_named(plain: &Summary) -> BTreeSet<String> {
    plain
        .skips
        .iter()
        .filter_map(|(_, why)| match home_of(why) {
            Some(Home::Named(r)) if !r.starts_with(GATE_PREFIX) => Some(r),
            _ => None,
        })
        .collect()
}

/// One line of `ran.txt` as the ledger reads it: (test, date).
fn parse_ran(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let mut w = l.split_whitespace();
        if let (Some(t), Some(_run), Some(date)) = (w.next(), w.next(), w.next()) {
            let e = m.entry(t.to_string()).or_insert_with(String::new);
            if date > e.as_str() {
                *e = date.to_string();
            }
        }
    }
    m
}

/// Days since 1970 of a `yyyy-mm-dd` date (Howard Hinnant's days_from_civil).
fn days(date: &str) -> Option<i64> {
    let mut p = date.split('-');
    let (y, m, d): (i64, i64, i64) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    Some(era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468)
}

fn today() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| (d.as_secs() / 86400) as i64)
}

/// What the ledger found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub total: usize,
    pub ran: usize,
    /// Tests in no run: the gate is red.
    pub nowhere: Vec<String>,
    /// Tests of the Mini-PC runs (no gate boot can run them).
    pub minipc: usize,
    pub minipc_fresh: usize,
    /// Mini-PC tests no round has ever recorded: they have a run by their class only, not yet in fact.
    pub minipc_never: usize,
    /// Mini-PC tests with no record or an old one, and records of tests that no longer exist.
    pub stale: Vec<String>,
}

fn compute(universe: &[Summary], ran: &BTreeSet<String>, recorded: &BTreeMap<String, String>, now: i64) -> Report {
    let mut r = Report::default();
    let mut all: BTreeMap<&str, Option<Home>> = BTreeMap::new();
    for s in universe {
        for n in s.ok.iter().chain(s.fails.iter().map(|(n, _)| n)) {
            all.entry(n).or_insert(None);
        }
        for (n, why) in &s.skips {
            let e = all.entry(n).or_insert(None);
            if e.is_none() {
                *e = home_of(why);
            }
        }
    }
    r.total = all.len();
    for (n, home) in &all {
        if ran.contains(*n) {
            r.ran += 1;
        } else if *home == Some(Home::MiniPc) {
            r.minipc += 1;
            match recorded.get(*n) {
                Some(d) if days(d).is_some_and(|d| now - d <= STALE_DAYS) => r.minipc_fresh += 1,
                Some(d) => r.stale.push(format!("{n} (recorded {d})")),
                None => {
                    r.minipc_never += 1;
                    r.stale.push(n.to_string());
                }
            }
        } else {
            let run = match home {
                Some(Home::Named(run)) => format!(" (run {run})"),
                _ => String::new(),
            };
            r.nowhere.push(format!("{n}{run}"));
        }
    }
    for n in recorded.keys() {
        if !all.contains_key(n.as_str()) {
            r.stale.push(format!("{n} (no such test any more)"));
        }
    }
    r
}

/// Everything recorded by the boots since `start`.
fn ran_names(root: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(dir(root)) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "ok") {
                set.extend(std::fs::read_to_string(e.path()).unwrap_or_default().lines().map(String::from));
            }
        }
    }
    set
}

/// The gate's step: prints the report, `Err` when a test ran in no run.
pub fn check(root: &Path, universe: &[Summary]) -> Result<(), String> {
    if universe.is_empty() {
        return Err("ledger: the plain boot left no result to count".into());
    }
    let recorded = parse_ran(&std::fs::read_to_string(root.join(MINIPC_FILE)).unwrap_or_default());
    let r = compute(universe, &ran_names(root), &recorded, today());
    println!(
        "ledger: {} tests: {} ran in a gate boot, {} belong to the Mini-PC runs ({} recorded within {STALE_DAYS} days, {} declared but never run there), {} in no run",
        r.total,
        r.ran,
        r.minipc,
        r.minipc_fresh,
        r.minipc_never,
        r.nowhere.len()
    );
    if !r.stale.is_empty() {
        println!("ledger: Mini-PC stale ({}, {MINIPC_FILE}, not red): {}", r.stale.len(), r.stale.join(", "));
    }
    if r.nowhere.is_empty() {
        Ok(())
    } else {
        for n in &r.nowhere {
            println!("ledger: IN NO RUN: {n}");
        }
        Err(format!("{} test(s) ran in no run", r.nowhere.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &str = "KTEST start count=6 filter=\nKTEST ok a.plain (1 ms)\n\
        KTEST skip b.nv: class Run(\"nv/10-nvme\") belongs to another run\n\
        KTEST skip c.nv: class RunHw(\"nv/10-nvme\") belongs to another run\n\
        KTEST skip d.prog: class Program belongs to another run\n\
        KTEST skip e.hw: class Hardware belongs to another run\n\
        KTEST skip f.gate: class Run(\"gate/nmi-nested\") belongs to another run\n\
        KTEST done passed=1 failed=0 skipped=5\n";

    #[test]
    fn classes_are_read_from_the_skip_lines() {
        assert_eq!(home_of("class Run(\"nv/10-nvme\") belongs to another run"), Some(Home::Named("nv/10-nvme".into())));
        assert_eq!(home_of("class RunHw(\"a/b\") belongs to another run"), Some(Home::Named("a/b".into())));
        assert_eq!(home_of("class Program belongs to another run"), Some(Home::MiniPc));
        assert_eq!(home_of("class QemuOnly belongs to another run"), Some(Home::Gate));
        assert_eq!(home_of("no NVMe controller"), None);
        assert!(!is_other_run("no NVMe controller"));
    }

    #[test]
    fn the_plain_boot_names_the_scripts() {
        let s = ktest::parse(PLAIN);
        assert_eq!(scripts_named(&s).into_iter().collect::<Vec<_>>(), ["nv/10-nvme"]);
    }

    #[test]
    fn a_test_in_no_run_is_named_with_its_run() {
        let s = ktest::parse(PLAIN);
        let ran: BTreeSet<String> = ["a.plain", "c.nv"].map(String::from).into();
        let r = compute(std::slice::from_ref(&s), &ran, &BTreeMap::new(), 0);
        assert_eq!(r.total, 6);
        assert_eq!(r.ran, 2);
        assert_eq!(r.minipc, 2);
        assert_eq!(r.nowhere, ["b.nv (run nv/10-nvme)", "f.gate (run gate/nmi-nested)"]);
        assert_eq!(r.stale, ["d.prog", "e.hw"]);
    }

    #[test]
    fn a_fresh_record_counts_and_an_old_one_is_stale() {
        let s = ktest::parse(PLAIN);
        let ran: BTreeSet<String> = ["a.plain", "b.nv", "c.nv", "f.gate"].map(String::from).into();
        let rec = parse_ran("# round\nd.prog desktop 2026-11-03 85\ne.hw desktop 2026-01-01 70\ngone.test bare 2026-11-03 85\n");
        let r = compute(std::slice::from_ref(&s), &ran, &rec, days("2026-11-10").unwrap());
        assert!(r.nowhere.is_empty());
        assert_eq!(r.minipc_fresh, 1);
        assert_eq!(r.stale, ["e.hw (recorded 2026-01-01)", "gone.test (no such test any more)"]);
    }

    #[test]
    fn a_test_that_ran_both_ways_counts_once() {
        let a = ktest::parse("KTEST ok x (1 ms)\nKTEST skip y: class Safe belongs to another run\nKTEST done passed=1 failed=0 skipped=1\n");
        let b = ktest::parse("KTEST skip x: class QemuOnly belongs to another run\nKTEST ok y (1 ms)\nKTEST done passed=1 failed=0 skipped=1\n");
        let ran: BTreeSet<String> = ["x", "y"].map(String::from).into();
        let r = compute(&[a, b], &ran, &BTreeMap::new(), 0);
        assert_eq!((r.total, r.ran, r.nowhere.len()), (2, 2, 0));
    }

    /// `ktest::Class::ALL` through the kernel's rule and the ledger's reading of the kernel's line: a new class that
    /// the ledger files under the wrong home fails here.
    #[test]
    fn the_ledger_reads_every_class_as_the_kernel_runs_it() {
        for &c in ::ktest::Class::ALL {
            let line = format!("class {c:?} belongs to another run");
            let want = match c {
                ::ktest::Class::Run(r) | ::ktest::Class::RunHw(r) => {
                    assert!(c.runs_here(true, false, r) && !c.runs_here(true, false, ""), "{c:?}");
                    Home::Named(r.to_string())
                }
                _ if c.runs_here(true, false, "") || c.runs_here(true, true, "") => Home::Gate,
                _ => {
                    assert!(c.runs_here(false, false, "") || !c.runs_here(true, false, ""), "{c:?}");
                    Home::MiniPc
                }
            };
            assert_eq!(home_of(&line), Some(want), "{c:?}");
        }
    }

    fn scratch(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xtask-ledger-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn check_is_red_for_a_test_in_no_run_and_green_when_only_minipc_tests_are_stale() {
        let root = scratch("check");
        let s = ktest::parse(PLAIN);
        start(&root).unwrap();
        record_boot(&root, "plain", "KTEST ok a.plain (1 ms)\n");
        record_boot(&root, "nv", "KTEST ok c.nv (1 ms)\nKTEST ok b.nv (1 ms)\n");
        let e = check(&root, std::slice::from_ref(&s)).unwrap_err();
        assert!(e.starts_with("1 test(s)"), "{e}");
        record_boot(&root, "nmi", "KTEST ok f.gate (1 ms)\n");
        check(&root, std::slice::from_ref(&s)).unwrap();
        let r = compute(std::slice::from_ref(&s), &ran_names(&root), &BTreeMap::new(), 0);
        assert_eq!((r.minipc, r.minipc_fresh, r.minipc_never, r.stale.len()), (2, 0, 2, 2));
        assert!(check(&root, &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_boots_of_one_name_lose_nothing() {
        let root = scratch("append");
        start(&root).unwrap();
        std::thread::scope(|t| {
            for b in 0..4 {
                let root = &root;
                t.spawn(move || {
                    for i in 0..50 {
                        record_boot(root, "same", &format!("KTEST ok t{b}.{i} (1 ms)\n"));
                    }
                });
            }
        });
        assert_eq!(ran_names(&root).len(), 200);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dates() {
        assert_eq!(days("1970-01-01"), Some(0));
        assert_eq!(days("2026-11-03").unwrap() - days("2026-10-04").unwrap(), 30);
        assert_eq!(days("nonsense"), None);
    }
}
