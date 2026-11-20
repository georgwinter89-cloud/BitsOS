//! `xtask stable-list`: reads every kept log for test results and writes `host/tests/stable.txt`, the tests that were
//! never red in any of them and green in at least `--min-green` (CONTRACTS section 4 stays: the full gate runs them all).
use crate::stable::{self, FILE};
use crate::util::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const DEFAULT_MIN_GREEN: usize = 20;
/// The desk's and the lanes' worktrees; `$HOME/BitsOS/V2/target` is added at run time (no home path in the tree).
const SHARED_LOGS: &str = "/var/tmp/claude-v2";
const MAX_LOG_BYTES: u64 = 400 << 20;
/// Tests that pass only after the tests of a name group ran earlier in the same boot: while such a test is in the tree,
/// no test of that group is listed, so an interim boot keeps what it needs. Measured 13.11.: both fail in every boot
/// without the stable `fs.` tests (also with every `mm.` test kept) and pass with only those kept in (about 52 s of
/// the 291 s an interim plain boot saves). A test that sets up its own memory needs no line here.
const NEEDS_EARLIER: &[(&str, &str)] = &[("mm.compact.restore", "fs."), ("mm.migrate.compact", "fs.")];
const MIN_LOG_AGE: std::time::Duration = std::time::Duration::from_secs(600);
/// Deeper folders are build trees and copied stages, not logs.
const MAX_DEPTH_SLASHES: usize = 9;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    /// Distinct log files with a green result.
    pub green: usize,
    /// Red results, all files.
    pub red: usize,
}

/// One result of one log line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Result(String, bool),
    /// A host test whose binary names no crate: (target source if known, test path, green).
    Unplaced(Option<String>, String, bool),
    /// `KTEST skip <t>: class Run("<script>") belongs to another run`.
    Home(String, String),
    /// A ktest that skipped itself (no device, no CONFIG line): red in a gate's log since Georg's rule of 30.10.; before
    /// it, and in boots started by hand without what the test needs, it says nothing about the code.
    SelfSkip(String),
}

/// `[0.12] KTEST ok x (1 ms)` -> `KTEST ok x (1 ms)`: the scanner counts a KTEST line only at its line's start.
fn ktest_line(l: &str) -> Option<&str> {
    let l = l.trim_end_matches('\r');
    let l = match l.strip_prefix('[') {
        Some(r) => r.split_once(']')?.1.trim_start(),
        None => l,
    };
    l.strip_prefix("KTEST ")
}

fn ktest_seen(r: &str) -> Option<Seen> {
    let name_of = |s: &str| s.split([' ', ':']).next().unwrap_or("").to_string();
    let (n, green) = if let Some(s) = r.strip_prefix("ok ") {
        (name_of(s), true)
    } else if let Some(s) = r.strip_prefix("FAIL ").or_else(|| r.strip_prefix("fail ")).or_else(|| r.strip_prefix("note ")) {
        (name_of(s), false)
    } else if let Some(s) = r.strip_prefix("skip ") {
        let (n, why) = s.split_once(": ").unwrap_or((s, ""));
        if crate::ledger::is_other_run(why) {
            let run = why.split("Run(\"").nth(1).and_then(|r| r.split('"').next()).filter(|r| !r.starts_with("gate/"))?;
            return Some(Seen::Home(n.trim().to_string(), run.to_string()));
        }
        // `class X not allowed here`: the older wording of a test of another run.
        if why.starts_with("class ") {
            return None;
        }
        return Some(Seen::SelfSkip(name_of(n)));
    } else {
        return None;
    };
    stable::is_ktest_name(&n).then(|| Seen::Result(format!("ktest:{n}"), green))
}

/// `GREEN: host tests green, ktest RED, ...` (the gate's summary, also as `INTERIM GREEN (...): ...`) -> its steps.
fn summary_steps(l: &str) -> Option<Vec<(String, bool)>> {
    let l = l.strip_prefix("INTERIM ").unwrap_or(l);
    let r = l.strip_prefix("GREEN").or_else(|| l.strip_prefix("RED"))?;
    let r = if r.starts_with(" (") { r.split_once("): ")?.1 } else { r.strip_prefix(": ")? };
    let steps: Vec<(String, bool)> = r
        .split(", ")
        .map(|p| p.strip_suffix(" green").map(|n| (n.to_string(), true)).or_else(|| p.strip_suffix(" RED").map(|n| (n.to_string(), false))))
        .collect::<Option<_>>()?;
    steps.iter().any(|(n, _)| n == "host tests").then_some(steps)
}

/// `scenario boots: nv/10-nvme: ok ...`, `fullrun: x/y: FAIL ...`, `scenario x/y: ok ...`.
fn scenario_seen(l: &str) -> Option<Seen> {
    let r = l.strip_prefix("scenario boots: ").or_else(|| l.strip_prefix("fullrun: ")).or_else(|| l.strip_prefix("scenario "))?;
    let (s, rest) = r.split_once(": ")?;
    let green = if rest.starts_with("ok") { true } else if rest.starts_with("FAIL") { false } else { return None };
    (s.split_once('/').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains(' '))).then(|| Seen::Result(format!("scenario:{s}"), green))
}

/// The day Georg's rule made a test that skipped itself red in the gate (TS-1).
const SELF_SKIP_RED_FROM: &str = "2026-10-30";

/// Every result one log holds, and whether it is a gate's log (it has the gate's summary line).
fn scan_log(text: &str) -> (Vec<Seen>, bool) {
    let mut out = Vec::new();
    let mut gate_log = false;
    // The binary the `test ... ok` lines belong to: Some((Some(crate), target)) placed, Some((None, target)) old layout.
    let mut bin: Option<(Option<String>, String)> = None;
    for l in text.lines() {
        if let Some(r) = ktest_line(l) {
            out.extend(ktest_seen(r));
        } else if let Some((krate, _, target)) = stable::running_binary(l) {
            bin = Some((krate, target));
        } else if l.trim_start().starts_with("Doc-tests ") {
            bin = None;
        } else if let Some(r) = l.strip_prefix("test ") {
            let Some((path, res)) = r.split_once(" ... ") else { continue };
            let green = match res.trim_end() {
                "ok" => true,
                "FAILED" => false,
                _ => continue,
            };
            if path.contains(' ') {
                continue;
            }
            out.push(match &bin {
                Some((Some(k), t)) => Seen::Result(format!("host:{k}::{t}::{path}"), green),
                Some((None, t)) => Seen::Unplaced(Some(t.clone()), path.to_string(), green),
                None => Seen::Unplaced(None, path.to_string(), green),
            });
        } else if let Some(steps) = summary_steps(l) {
            gate_log = true;
            out.extend(steps.into_iter().map(|(n, g)| Seen::Result(format!("step:{n}"), g)));
        } else if let Some(s) = scenario_seen(l) {
            out.push(s);
        }
    }
    (out, gate_log)
}

#[derive(Debug, Default)]
pub struct History {
    pub tests: BTreeMap<String, Tally>,
    /// ktest -> the script its class names.
    pub homes: BTreeMap<String, String>,
    pub files: usize,
    /// Oldest and newest log, days since 1970.
    pub span: Option<(i64, i64)>,
    /// Red host tests whose crate no log line named: (target source if known, test path).
    pub(crate) unplaced_red: Vec<(Option<String>, String)>,
    /// The logs already counted (`digest`): a log read again, or a copy of it, counts once.
    pub(crate) logs: BTreeSet<u64>,
}

impl History {
    fn add(&mut self, (seen, gate_log): (Vec<Seen>, bool), mtime_days: i64) {
        let skip_is_red = gate_log && crate::ledger::days(SELF_SKIP_RED_FROM).is_some_and(|d| mtime_days >= d);
        if seen.is_empty() {
            return;
        }
        self.files += 1;
        self.span = Some(self.span.map_or((mtime_days, mtime_days), |(a, b)| (a.min(mtime_days), b.max(mtime_days))));
        let mut green_here = BTreeSet::new();
        for s in seen {
            match s {
                Seen::Result(k, true) => {
                    green_here.insert(k);
                }
                Seen::Result(k, false) => self.tests.entry(k).or_default().red += 1,
                Seen::Unplaced(t, p, false) => self.unplaced_red.push((t, p)),
                // A green result whose crate is unknown proves nothing about any one key.
                Seen::Unplaced(_, _, true) => {}
                Seen::Home(t, run) => {
                    self.homes.insert(t, run);
                }
                Seen::SelfSkip(n) if skip_is_red && stable::is_ktest_name(&n) => self.tests.entry(format!("ktest:{n}")).or_default().red += 1,
                Seen::SelfSkip(_) => {}
            }
        }
        for k in green_here {
            self.tests.entry(k).or_default().green += 1;
        }
    }

    /// Once every log is read: a red host test of unknown crate is red for every crate that has a test of that path.
    pub fn settle(&mut self) {
        let mut by_path: BTreeMap<&str, Vec<&Option<String>>> = BTreeMap::new();
        for (t, p) in &self.unplaced_red {
            by_path.entry(p.as_str()).or_default().push(t);
        }
        for (k, v) in self.tests.iter_mut() {
            let Some((_, kt, kp)) = k.strip_prefix("host:").and_then(stable::host_key_parts) else { continue };
            if let Some(ts) = by_path.get(kp) {
                v.red += ts.iter().filter(|t| t.as_deref().is_none_or(|t| t == kt)).count();
            }
        }
    }
}

/// The log files below `dirs`, the way the desk's scanner chose them (`*.log`, `*output*.txt`; no build trees).
fn log_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo: Vec<PathBuf> = dirs.to_vec();
    while let Some(d) = todo.pop() {
        let ds = d.to_string_lossy();
        if ["/ref", "/deps", "/build/", "/.git", "/incremental"].iter().any(|x| ds.contains(x)) || ds.matches('/').count() > MAX_DEPTH_SLASHES {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_dir() {
                todo.push(p);
            } else if ft.is_file() && (name.ends_with(".log") || (name.ends_with(".txt") && name.contains("output"))) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn days_of(t: std::time::SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| (d.as_secs() / 86400) as i64)
}

/// `yyyy-mm-dd` of days since 1970 (Howard Hinnant's civil_from_days).
pub fn date(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

/// FNV-1a over the bytes and their length: stable across Rust versions, so a cache's digests stay valid.
pub fn digest(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes.iter().chain(&(bytes.len() as u64).to_le_bytes()) {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Adds every log below `dirs` that `h` has not counted yet (a copy of a log counts once: same bytes). Not settled.
pub fn read_logs(dirs: &[PathBuf], mut h: History) -> History {
    for p in log_files(dirs) {
        let Ok(md) = std::fs::metadata(&p) else { continue };
        // A log still being written would count again, with another digest, once it is complete.
        let fresh = md.modified().ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age < MIN_LOG_AGE);
        if md.len() > MAX_LOG_BYTES || fresh {
            continue;
        }
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let has = |m: &[u8]| bytes.windows(m.len()).any(|w| w == m);
        if !(has(b"KTEST ") || has(b"test result") || has(b"scenario") || has(b"GREEN") || has(b"RED")) {
            continue;
        }
        if !h.logs.insert(digest(&bytes)) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        h.add(scan_log(&text), md.modified().map_or(0, days_of));
    }
    h
}

/// The list: never red, green in `min_green` logs or more, and runnable on its own in an interim run.
/// `source` is every ktest-name literal of the tree: a ktest the tree no longer has is not listed.
pub fn select(h: &History, min_green: usize, source: &BTreeSet<String>, script_exists: impl Fn(&str) -> bool) -> stable::Stable {
    let mut known = source.clone();
    known.extend(h.tests.keys().filter_map(|k| k.strip_prefix("ktest:")).map(String::from));
    let held: Vec<&str> = NEEDS_EARLIER.iter().filter(|(t, _)| source.contains(*t)).map(|(_, g)| *g).collect();
    let ok = |k: &str| h.tests.get(k).is_some_and(|t| t.red == 0 && t.green >= min_green);
    let mut st = stable::Stable::default();
    for k in h.tests.keys().filter(|k| ok(k)) {
        let (kind, name) = k.split_once(':').unwrap_or(("", ""));
        match kind {
            "host" if stable::host_key_parts(name).is_some() => st.host.insert(name.to_string()),
            "scenario" if script_exists(name) => st.scenario.insert(name.to_string()),
            "step" if stable::step_can_be_left_out(name) => st.step.insert(name.to_string()),
            _ => false,
        };
    }
    // A ktest whose class names a script goes only with its script: the interim gate starts the scripts that the boot's
    // tests name, and a script that is not stable itself must still run.
    for k in h.tests.keys().filter(|k| ok(k)) {
        if let Some(n) = k.strip_prefix("ktest:")
            && source.contains(n)
            && !held.iter().any(|g| n.starts_with(g))
            && h.homes.get(n).is_none_or(|s| st.scenario.contains(s))
        {
            st.ktest.insert(n.to_string());
        }
    }
    let keep: BTreeSet<String> = stable::ktest_exclude(&st, &known).into_iter().collect();
    st.ktest = keep;
    st
}

pub fn render(st: &stable::Stable, header: &[String]) -> String {
    let mut s = String::new();
    for l in header {
        s.push_str(&format!("# {l}\n"));
    }
    for (kind, set) in [("host", &st.host), ("ktest", &st.ktest), ("scenario", &st.scenario), ("step", &st.step)] {
        for n in set {
            s.push_str(&format!("{kind}:{n}\n"));
        }
    }
    s
}

pub fn cmd_stable_list(root: &Path, mut a: Args) -> Result<(), String> {
    let min_green = a.num::<usize>("--min-green")?.unwrap_or(DEFAULT_MIN_GREEN);
    let mut dirs: Vec<PathBuf> = a.values("--logs")?.into_iter().map(PathBuf::from).collect();
    let out = a.value("--out")?.map_or_else(|| root.join(FILE), PathBuf::from);
    let dry = a.flag("--dry-run");
    let cache = a.value("--cache")?.map_or_else(|| xdir(root).join("stable-cache.tsv"), PathBuf::from);
    a.positionals()?;
    let home = std::env::var("HOME").unwrap_or_default();
    if dirs.is_empty() {
        dirs.push(PathBuf::from(SHARED_LOGS));
        if home.starts_with('/') {
            dirs.push(Path::new(&home).join("BitsOS/V2/target"));
        }
    }
    // Scripts and boots overwrite their run folders, so most logs live until the next run: what was read once stays
    // counted through the cache (`--cache`), and a red result stays red after its log is gone.
    let old = std::fs::read_to_string(&cache).map(|t| History::parse_cache(&t)).unwrap_or_else(|_| Ok(History::default()))?;
    let known_before = old.files;
    let mut h = read_logs(&dirs, old);
    if !dry {
        let _ = std::fs::create_dir_all(cache.parent().unwrap_or(Path::new(".")));
        std::fs::write(&cache, h.cache_text()).map_err(|e| format!("{}: {e}", cache.display()))?;
    }
    println!("stable-list: {} logs counted, {} of them new; cache {}", h.files, h.files - known_before, cache.display());
    h.settle();
    let st = select(&h, min_green, &stable::source_names(root), |s| root.join("host/tests").join(format!("{s}.sh")).is_file());
    let shown: Vec<String> = dirs.iter().map(|d| d.to_string_lossy().replacen(&home, "$HOME", usize::from(home.len() > 1))).collect();
    let (from, to) = h.span.map_or(("-".into(), "-".into()), |(a, b)| (date(a), date(b)));
    let header = vec![
        "Stable tests: never red in any kept log, and green in at least {min} distinct logs (copies count once)."
            .replace("{min}", &min_green.to_string()),
        "`cargo xtask test --interim` leaves them to the integrator's full run; `cargo xtask test` runs every test.".into(),
        format!(
            "Made by `cargo xtask stable-list --min-green {min_green}` on {} from {} logs ({from} to {to}) below {}, with the ones counted earlier (--cache).",
            date(days_of(std::time::SystemTime::now())),
            h.files,
            shown.join(", ")
        ),
        "Regenerate after the integrator's full run (host/tests/README.md, \"Interim runs\"); do not edit by hand.".into(),
        format!("{} host, {} ktest, {} scenario, {} step.", st.host.len(), st.ktest.len(), st.scenario.len(), st.step.len()),
    ];
    let red: Vec<(&String, &Tally)> = h.tests.iter().filter(|(_, t)| t.red > 0).collect();
    println!("stable-list: {} logs, {} tests seen, {} ever red; listed {} host, {} ktest, {} scenario, {} step", h.files, h.tests.len(), red.len(), st.host.len(), st.ktest.len(), st.scenario.len(), st.step.len());
    let hist = xdir(root).join("stable-history.txt");
    let _ = std::fs::create_dir_all(xdir(root));
    let table: String = h.tests.iter().map(|(k, t)| format!("{k}\t{}\t{}\n", t.green, t.red)).collect();
    let _ = std::fs::write(&hist, table);
    println!("stable-list: every test with its green logs and red results: {}", hist.display());
    if dry {
        return Ok(());
    }
    std::fs::write(&out, render(&st, &header)).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("stable-list: wrote {}", out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATE: &str = "xtask: cargo test (host crates)\n     Running unittests src/lib.rs (target/debug/build/hidparse/1/out/hidparse-1)\n\
        test tests::a ... ok\ntest tests::b ... FAILED\ntest tests::c ... ignored\n   Doc-tests hidparse\ntest src/lib.rs - f (line 3) ... ok\n\
        KTEST start count=3 filter=\n[1.0] KTEST ok mm.alloc (3 ms)\nKTEST FAIL mm.free: 3 != 4\nKTEST note mm.map: nmi sample: Timeout\n\
        KTEST skip nv.irq: class Run(\"nv/10-nvme\") belongs to another run\nKTEST skip nv.x: no card\n\
        KTEST skip rb.nest: class Run(\"gate/nmi-nested\") belongs to another run\nKTEST skip old.x: class QemuOnly not allowed here\n\
        scenario boots: nv/10-nvme: ok exit status: 0 (log x)\nfullrun: fs/30-fat: FAIL timeout\n\
        RED: mm-tlb16 not green\nGREEN: host tests green, wy gles green, ktest RED\nINTERIM GREEN (7 stable tests left to the integrator's full run): host tests green, ds look green\n";

    #[test]
    fn a_log_gives_every_kind_of_result() {
        let (s, gate) = scan_log(GATE);
        assert!(gate);
        let r = |k: &str, g: bool| Seen::Result(k.to_string(), g);
        for want in [
            r("host:hidparse::src/lib.rs::tests::a", true),
            r("host:hidparse::src/lib.rs::tests::b", false),
            r("ktest:mm.alloc", true),
            r("ktest:mm.free", false),
            r("ktest:mm.map", false),
            Seen::SelfSkip("nv.x".into()),
            Seen::Home("nv.irq".into(), "nv/10-nvme".into()),
            r("scenario:nv/10-nvme", true),
            r("scenario:fs/30-fat", false),
            r("step:wy gles", true),
            r("step:ktest", false),
            r("step:ds look", true),
        ] {
            assert!(s.contains(&want), "{want:?} in {s:?}");
        }
        assert!(!s.iter().any(|x| matches!(x, Seen::Result(k, _) if k.contains("tests::c") || k.contains("mm-tlb16") || k.contains("src/lib.rs - f"))));
        assert!(!s.iter().any(|x| matches!(x, Seen::Home(t, _) if t == "rb.nest")));
        assert!(!s.iter().any(|x| format!("{x:?}").contains("old.x")));
    }

    #[test]
    fn a_self_skip_is_red_only_in_a_gate_log_since_the_rule() {
        let rule = crate::ledger::days(SELF_SKIP_RED_FROM).unwrap();
        let gate = "KTEST skip nv.x: no card\nRED: host tests green, ktest RED\n";
        for (log, day, red) in [("KTEST skip nv.x: no card\n", rule, 0), (gate, rule - 1, 0), (gate, rule, 1)] {
            let mut h = History::default();
            h.add(scan_log(log), day);
            assert_eq!(h.tests.get("ktest:nv.x").map_or(0, |t| t.red), red, "{log} {day}");
        }
    }

    fn hist(logs: &[&str]) -> History {
        let mut h = History::default();
        for l in logs {
            h.add(scan_log(l), 0);
        }
        h.settle();
        h
    }

    #[test]
    fn the_cache_reads_back_and_a_log_counts_once() {
        let mut h = History::default();
        h.add(scan_log("KTEST ok a.b (1 ms)\nKTEST skip a.c: class Run(\"x/y\") belongs to another run\ntest t ... FAILED\n"), 5);
        h.logs.insert(digest(b"one"));
        let back = History::parse_cache(&h.cache_text()).unwrap();
        assert_eq!(back.cache_text(), h.cache_text());
        assert_eq!((back.files, back.span, back.tests["ktest:a.b"].green), (1, Some((5, 5)), 1));
        assert_eq!(back.homes["a.c"], "x/y");
        assert_eq!(back.unplaced_red, [(None, "t".to_string())]);
        assert!(History::parse_cache("v0\n").is_err());
        let dir = std::env::temp_dir().join(format!("xtask-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old = std::time::SystemTime::now() - 2 * MIN_LOG_AGE;
        for f in ["a.log", "copy.log", "running.log"] {
            std::fs::write(dir.join(f), if f == "running.log" { "KTEST FAIL a.b: x\n" } else { "KTEST ok a.b (1 ms)\n" }).unwrap();
            if f != "running.log" {
                std::fs::File::options().write(true).open(dir.join(f)).unwrap().set_modified(old).unwrap();
            }
        }
        let h = read_logs(std::slice::from_ref(&dir), History::default());
        assert_eq!((h.files, h.tests["ktest:a.b"].red), (1, 0));
        // Read again with the cache: nothing new; the log deleted afterwards still counts.
        let h = read_logs(std::slice::from_ref(&dir), h);
        std::fs::remove_dir_all(&dir).unwrap();
        let h = read_logs(std::slice::from_ref(&dir), h);
        assert_eq!((h.files, h.tests["ktest:a.b"].green), (1, 1));
    }

    #[test]
    fn green_counts_logs_and_one_red_keeps_a_test_out() {
        let ok = "KTEST ok a.b (1 ms)\nKTEST ok a.b (1 ms)\nKTEST ok c.d (1 ms)\n";
        let h = hist(&[ok, ok, ok, "KTEST FAIL c.d: no\n"]);
        assert_eq!(h.tests["ktest:a.b"], Tally { green: 3, red: 0 });
        let known: BTreeSet<String> = ["a.b", "c.d"].map(String::from).into();
        let st = select(&h, 3, &known, |_| true);
        assert_eq!(st.ktest.into_iter().collect::<Vec<_>>(), ["a.b"]);
        assert!(select(&h, 4, &known, |_| true).ktest.is_empty());
    }

    #[test]
    fn a_red_host_test_of_unknown_crate_is_red_for_every_crate_with_it() {
        let placed = "     Running unittests src/lib.rs (target/debug/build/x/1/out/x-1)\ntest t::a ... ok\n\
            \x20    Running tests/i.rs (target/debug/build/y/2/out/i-2)\ntest t::a ... ok\ntest t::b ... ok\n";
        let old = "     Running tests/i.rs (/w/target/debug/deps/i-3)\ntest t::a ... FAILED\ntest t::b ... ok\n";
        let h = hist(&[placed, old]);
        assert_eq!(h.tests["host:x::src/lib.rs::t::a"].red, 0);
        assert_eq!(h.tests["host:y::tests/i.rs::t::a"].red, 1);
        assert_eq!(h.tests["host:y::tests/i.rs::t::b"], Tally { green: 1, red: 0 });
        let h = hist(&[placed, "test t::a ... FAILED\n"]);
        assert_eq!((h.tests["host:x::src/lib.rs::t::a"].red, h.tests["host:y::tests/i.rs::t::a"].red), (1, 1));
    }

    #[test]
    fn a_ktest_goes_only_with_its_script_and_steps_that_always_run_never_go() {
        let log = "KTEST skip nv.a: class Run(\"nv/10-nvme\") belongs to another run\nKTEST ok nv.a (1 ms)\nKTEST ok nv.b (1 ms)\n\
            KTEST skip nv.b: class Run(\"nv/20-more\") belongs to another run\nscenario boots: nv/10-nvme: ok\nscenario boots: nv/20-more: ok\n\
            GREEN: host tests green, warnings green, wy gles green\n";
        let h = hist(&[log, log, "scenario boots: nv/20-more: FAIL\n"]);
        let known: BTreeSet<String> = ["nv.a", "nv.b"].map(String::from).into();
        let st = select(&h, 2, &known, |_| true);
        assert_eq!(st.ktest.iter().collect::<Vec<_>>(), ["nv.a"]);
        assert_eq!(st.scenario.iter().collect::<Vec<_>>(), ["nv/10-nvme"]);
        assert_eq!(st.step.iter().collect::<Vec<_>>(), ["wy gles"]);
        assert!(select(&h, 2, &known, |s| s != "nv/10-nvme").ktest.is_empty());
    }

    #[test]
    fn a_group_an_order_dependent_test_needs_stays_in_and_gone_tests_go() {
        let log = "KTEST ok fs.a (1 ms)\nKTEST ok fs.b (1 ms)\nKTEST ok mm.compact.restore (1 ms)\nKTEST ok gone.x (1 ms)\n";
        let h = hist(&[log]);
        let with: BTreeSet<String> = ["fs.a", "fs.b", "mm.compact.restore"].map(String::from).into();
        assert_eq!(select(&h, 1, &with, |_| true).ktest.into_iter().collect::<Vec<_>>(), ["mm.compact.restore"]);
        let without: BTreeSet<String> = ["fs.a", "fs.b"].map(String::from).into();
        assert_eq!(select(&h, 1, &without, |_| true).ktest.into_iter().collect::<Vec<_>>(), ["fs.a", "fs.b"]);
    }

    #[test]
    fn the_rendered_list_reads_back() {
        let log = "     Running unittests src/lib.rs (target/debug/build/x/1/out/x-1)\ntest t::a ... ok\nKTEST ok a.b (1 ms)\nGREEN: host tests green, ds look green\n";
        let h = hist(&[log]);
        let known: BTreeSet<String> = ["a.b"].map(String::from).into();
        let st = select(&h, 1, &known, |_| true);
        let text = render(&st, &["made in a test".into()]);
        assert!(text.starts_with("# made in a test\nhost:x::src/lib.rs::t::a\n"), "{text}");
        assert_eq!(stable::parse(&text).unwrap(), st);
    }

    #[test]
    fn dates_both_ways() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20405), "2025-11-13");
        assert_eq!(date(-1), "1969-12-31");
    }
}
