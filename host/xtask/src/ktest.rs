//! `xtask ktest`: boot the ktest kernel in QEMU (one instance per filter, in parallel) and read the COM1 protocol.
use crate::image::{self, BuildOpts, ImageOpts};
use crate::qemu::{self, QemuOpts};
use crate::util::*;
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub started: Option<(u32, String)>,
    pub ok: Vec<String>,
    pub fails: Vec<(String, String)>,
    pub skips: Vec<(String, String)>,
    pub done: Option<(u32, u32, u32)>,
    pub panic: Option<String>,
    /// `KTEST note <test>: <text>` lines: the kernel writes one only when something went wrong after a test
    /// that may still say `ok` (the boot-stack canary was reached, another CPU missed its NMI sample).
    pub notes: Vec<(String, String)>,
}

fn field(line: &str, key: &str) -> Option<u32> {
    let i = line.find(key)? + key.len();
    line[i..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Lines may carry a CR, a prefix from firmware output, or interleaved noise; only `KTEST ` lines count.
pub fn parse(log: &str) -> Summary {
    let mut s = Summary::default();
    for raw in log.lines() {
        let Some(at) = raw.find("KTEST ") else { continue };
        let l = raw[at + 6..].trim();
        if let Some(r) = l.strip_prefix("start") {
            let filter = r.split("filter=").nth(1).unwrap_or("").trim().to_string();
            s.started = Some((field(r, "count=").unwrap_or(0), filter));
        } else if let Some(r) = l.strip_prefix("ok ") {
            s.ok.push(r.split(" (").next().unwrap_or(r).trim().to_string());
        } else if let Some(r) = l.strip_prefix("FAIL ") {
            let (n, why) = r.split_once(": ").or_else(|| r.strip_suffix(':').map(|n| (n, ""))).unwrap_or((r, ""));
            s.fails.push((n.trim().to_string(), why.trim().to_string()));
        } else if let Some(r) = l.strip_prefix("skip ") {
            let (n, why) = r.split_once(": ").or_else(|| r.strip_suffix(':').map(|n| (n, ""))).unwrap_or((r, ""));
            s.skips.push((n.trim().to_string(), why.trim().to_string()));
        } else if let Some(r) = l.strip_prefix("note ") {
            let (n, why) = r.split_once(": ").unwrap_or((r, ""));
            s.notes.push((n.trim().to_string(), why.trim().to_string()));
        } else if l.starts_with("done") {
            s.done = Some((field(l, "passed=").unwrap_or(0), field(l, "failed=").unwrap_or(0), field(l, "skipped=").unwrap_or(0)));
        } else if let Some(r) = l.strip_prefix("PANIC") {
            s.panic = Some(r.trim().to_string());
        }
    }
    s
}

impl Summary {
    /// Reasons this run is red (empty = green).
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        for (n, w) in &self.fails {
            p.push(format!("FAIL {n}: {w}"));
        }
        // A note makes the run red: a stack overflow into the boot stack's canary (INT-W28) or a silent CPU
        // after a test is a defect the test's own `ok` does not see.
        for (n, w) in &self.notes {
            p.push(format!("NOTE {n}: {w}"));
        }
        if let Some(m) = &self.panic {
            p.push(format!("PANIC {m}"));
        }
        // Georg 30.10.: a test of another run is the only `skip` there is. Any other reason is a test that ended
        // itself as skipped: not passed, not a run (TS-1).
        for (n, w) in &self.skips {
            if !crate::ledger::is_other_run(w) {
                p.push(format!("SKIP {n}: {w} (a test that cannot do its work is FAIL)"));
            }
        }
        match self.done {
            None => p.push("no `KTEST done` line".into()),
            Some((_, f, _)) if f > 0 && self.fails.is_empty() => p.push(format!("done line says failed={f}")),
            _ => {}
        }
        p
    }
}

/// QEMU time for one ktest boot (`--timeout` changes it). INT-A2a2: 120 s was the A1 budget (427 tests, about 85 s
/// of QEMU); the A2 wave's 649 tests take about 118 s of QEMU on an idle Gaming-PC, so a little desktop load made the
/// gate red with "no KTEST done line" in whatever test ran at 120 s (us.msd.read/.reset/.write), a budget and not
/// a hang. 300 s leaves room for the next waves; a real hang still ends in it. INT-W21 (NW-10, AB-16): 939 tests
/// take about 285-296 s of QEMU on a loaded Gaming-PC (227 s for 875 on 26.10.), so 300 s was red with 0 failures;
/// 600 s, the same reasoning again.
pub const DEFAULT_TIMEOUT_S: u64 = 600;

pub struct KtestOpts {
    pub filters: Vec<String>,
    pub timeout: u64,
    pub build: BuildOpts,
    /// More CONFIG lines (`--config-extra "k=v;k2=v2"`), e.g. `ktest_hw=on;ktest_nmi_check=on`.
    pub config: Vec<String>,
    /// Network card for the boots (`none|user|virtio|igb`); the gate uses virtio so NW/MC tests see a card.
    pub net: String,
    /// QEMU arguments after `--` (extra devices for topology tests).
    pub extra: Vec<String>,
}

fn label(f: &str, i: usize, n: usize) -> String {
    if n == 1 {
        return "ktest".into();
    }
    let clean: String = f.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("ktest-{i}-{}", if clean.is_empty() { "all".into() } else { clean })
}

/// Two HDA controllers with a duplex codec each, playing into WAV files of the run (never the host's sound
/// devices), so the sound ktests (sn.hda.*, sn.mix.two/switch/resample) have their device (SN-2 CR 4, SN-3 CR 1).
/// Every `xtask ktest` boot has them: inside its run a test without its device is FAIL.
fn audio_args(root: &Path, name: &str) -> Vec<String> {
    let dir = root.join("target/xtask/runs/gate-audio");
    let _ = std::fs::create_dir_all(&dir);
    let mut v = Vec::new();
    for i in 0..2 {
        let wav = dir.join(format!("{name}{i}.wav"));
        let _ = std::fs::remove_file(&wav);
        v.extend([
            "-audiodev".to_string(),
            format!("wav,id=a{i},path={},out.frequency=48000,out.channels=2,out.format=s16", wav.display()),
            "-device".to_string(),
            format!("intel-hda,id=h{i}"),
            "-device".to_string(),
            format!("hda-duplex,bus=h{i}.0,audiodev=a{i},mixer=off"),
        ]);
    }
    v
}

/// One line per kind of class for the tests of other runs: they are many, and their names are in the log.
fn other_runs_line(s: &Summary) -> Option<String> {
    let mut m: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for (_, w) in &s.skips {
        if let Some(c) = w.strip_prefix("class ").and_then(|c| c.strip_suffix(" belongs to another run")) {
            *m.entry(c).or_insert(0) += 1;
        }
    }
    if m.is_empty() {
        return None;
    }
    Some(m.iter().map(|(c, n)| format!("{c} {n}")).collect::<Vec<_>>().join(", "))
}

/// Returns Ok when every instance is green.
pub fn run_ktests(root: &Path, o: &KtestOpts) -> Result<(), String> {
    run_ktests_summaries(root, o).1
}

/// `run_ktests`, with what each boot said (the gate's ledger counts them).
pub fn run_ktests_summaries(root: &Path, o: &KtestOpts) -> (Vec<Summary>, Result<(), String>) {
    let mut sums = Vec::new();
    let r = run_boots(root, o, &mut sums);
    (sums, r)
}

fn run_boots(root: &Path, o: &KtestOpts, sums: &mut Vec<Summary>) -> Result<(), String> {
    let mut b = o.build;
    b.ktest = true;
    let art = image::build(root, b)?;
    let n = o.filters.len();
    let mut handles = Vec::new();
    for (i, f) in o.filters.iter().enumerate() {
        let name = label(f, i, n);
        let io = ImageOpts { name: name.clone(), generation: 1, config_extra: [format!("ktest={f}")].into_iter().chain(o.config.iter().cloned()).collect(), build: b };
        image::make_image_from(root, &io, &art)?;
        let mut q = QemuOpts::new(&name);
        q.timeout = o.timeout;
        q.net = o.net.clone();
        // A USB keyboard and tablet on the stick's controller, so the US transfer ktests (us.xfer.timeout,
        // us.xfer.hid_pipes) run in every gate (US-3 change request); extra arguments come after them.
        q.extra = ["-device", "usb-kbd,bus=xhci.0", "-device", "usb-tablet,bus=xhci.0"].iter().map(|s| s.to_string()).collect();
        q.extra.extend(audio_args(root, &name));
        q.extra.extend(o.extra.iter().cloned());
        q.echo = n == 1;
        let root = root.to_path_buf();
        let filter = f.clone();
        handles.push(std::thread::spawn(move || -> (String, String, Result<qemu::RunResult, String>) {
            let r = qemu::run(&root, &q);
            (name, filter, r)
        }));
    }
    let mut red = false;
    for h in handles {
        let (name, filter, r) = h.join().map_err(|_| "ktest thread panicked".to_string())?;
        let r = r?;
        let log = std::fs::read_to_string(&r.log).unwrap_or_default();
        let s = parse(&log);
        let mut problems = s.problems();
        if r.timed_out {
            // Name where it stopped: the next test after this one hung (or the budget ran out there).
            let last = log.lines().rev().find(|l| l.starts_with("KTEST ok ") || l.starts_with("KTEST FAIL ") || l.starts_with("KTEST skip ")).unwrap_or("none");
            problems.push(format!("timeout after {} s; last result before it: {last}", o.timeout));
        } else if let Some(c @ (qemu::EXIT_FAIL | qemu::EXIT_PANIC)) = r.exit {
            problems.push(format!("QEMU exit code {c}"));
        } else if s.done.is_none() {
            problems.push(qemu::describe_exit(r.exit, false).1);
        }
        let fl = if filter.is_empty() { "(all)".to_string() } else { filter };
        match s.done {
            Some((p, f, k)) => println!("ktest {name} filter {fl}: passed={p} failed={f} skipped={k}"),
            None => println!("ktest {name} filter {fl}: ok={} fail={} skip={} (no done line)", s.ok.len(), s.fails.len(), s.skips.len()),
        }
        if let Some(l) = other_runs_line(&s) {
            println!("  belongs to another run: {l}");
        }
        for p in &problems {
            println!("  RED: {p}");
        }
        if !problems.is_empty() {
            println!("  log: {}", r.log.display());
            red = true;
        }
        sums.push(s);
    }
    if red { Err("ktest: red".into()) } else { println!("ktest: green"); Ok(()) }
}

pub fn cmd_ktest(root: &Path, mut a: Args) -> Result<(), String> {
    let timeout = a.num::<u64>("--timeout")?.unwrap_or(DEFAULT_TIMEOUT_S);
    let split = a.value("--split")?;
    let net = a.value("--net")?.unwrap_or_else(|| "virtio".into());
    let extra_cfg = a.value("--config-extra")?.unwrap_or_default();
    let config = extra_cfg.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    let build = BuildOpts::parse(&mut a);
    let pos = a.positionals()?;
    let filters: Vec<String> = match split {
        Some(s) => s.split(',').map(|x| x.trim().to_string()).collect(),
        None => vec![pos.first().cloned().unwrap_or_default()],
    };
    let extra = a.extra.clone();
    run_ktests(root, &KtestOpts { filters, timeout, build, net, extra, config })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "BdsDxe: loading\r\nKTEST start count=3 filter=\r\nKTEST ok mm::alloc (2 ms)\nKTEST skip hw::x: class Hardware belongs to another run\nKTEST FAIL sched::y: expected 3 got 4\nKTEST done passed=1 failed=1 skipped=1\n";

    #[test]
    fn parses_all_line_kinds() {
        let s = parse(GOOD);
        assert_eq!(s.started, Some((3, String::new())));
        assert_eq!(s.ok, vec!["mm::alloc"]);
        assert_eq!(s.skips, vec![("hw::x".to_string(), "class Hardware belongs to another run".to_string())]);
        assert_eq!(s.fails, vec![("sched::y".to_string(), "expected 3 got 4".to_string())]);
        assert_eq!(s.done, Some((1, 1, 1)));
        assert_eq!(s.problems(), vec!["FAIL sched::y: expected 3 got 4"]);
    }

    #[test]
    fn a_test_that_skips_itself_is_red() {
        let s = parse("KTEST start count=2 filter=\nKTEST ok a (1 ms)\nKTEST skip b: no igb card\nKTEST done passed=1 failed=0 skipped=1\n");
        let p = s.problems();
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("SKIP b: no igb card"), "{p:?}");
    }

    #[test]
    fn other_runs_are_counted_by_class() {
        let s = parse("KTEST skip a: class Program belongs to another run\nKTEST skip b: class Program belongs to another run\nKTEST skip c: class Run(\"x/y\") belongs to another run\nKTEST done passed=0 failed=0 skipped=3\n");
        assert!(s.problems().is_empty());
        assert_eq!(other_runs_line(&s).unwrap(), "Program 2, Run(\"x/y\") 1");
    }

    #[test]
    fn a_canary_note_after_an_ok_is_red() {
        let s = parse("KTEST start count=1 filter=\nKTEST ok x (1 ms)\nKTEST note x: stack overflow in x: the boot stack (256 KiB) was used into its last 16 KiB (canary 512 bytes)\nKTEST done passed=1 failed=0 skipped=0\n");
        assert_eq!(s.ok, vec!["x"]);
        assert_eq!(s.notes.len(), 1);
        let p = s.problems();
        assert_eq!(p.len(), 1);
        assert!(p[0].starts_with("NOTE x: stack overflow in x:"), "{p:?}");
    }

    #[test]
    fn an_nmi_note_is_red() {
        let s = parse("KTEST start count=1 filter=\nKTEST ok y (1 ms)\nKTEST note y: nmi sample: Timeout\nKTEST done passed=1 failed=0 skipped=0\n");
        assert_eq!(s.problems(), vec!["NOTE y: nmi sample: Timeout"]);
    }

    #[test]
    fn green_run_has_no_problems() {
        let s = parse("KTEST start count=1 filter=mm\nKTEST ok a (1 ms)\nKTEST done passed=1 failed=0 skipped=0\n");
        assert!(s.problems().is_empty());
        assert_eq!(s.started.unwrap().1, "mm");
    }

    #[test]
    fn panic_and_missing_done_are_red() {
        let s = parse("KTEST start count=2 filter=\nKTEST ok a (1 ms)\nKTEST PANIC page fault at 0x10\n");
        let p = s.problems();
        assert!(p.iter().any(|x| x.starts_with("PANIC page fault")));
        assert!(p.iter().any(|x| x.contains("no `KTEST done`")));
        assert!(!parse("").problems().is_empty());
    }

    #[test]
    fn labels() {
        assert_eq!(label("", 0, 1), "ktest");
        assert_eq!(label("mm::x", 1, 3), "ktest-1-mm__x");
        assert_eq!(label("", 0, 2), "ktest-0-all");
    }
}
