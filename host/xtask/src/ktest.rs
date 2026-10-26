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
        if let Some(m) = &self.panic {
            p.push(format!("PANIC {m}"));
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
/// a hang. 300 s leaves room for the next waves; a real hang still ends in it.
pub const DEFAULT_TIMEOUT_S: u64 = 300;

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

/// Returns Ok when every instance is green.
pub fn run_ktests(root: &Path, o: &KtestOpts) -> Result<(), String> {
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
        for (n, w) in &s.skips {
            println!("  skip {n}: {w}");
        }
        for p in &problems {
            println!("  RED: {p}");
        }
        if !problems.is_empty() {
            println!("  log: {}", r.log.display());
            red = true;
        }
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

    const GOOD: &str = "BdsDxe: loading\r\nKTEST start count=3 filter=\r\nKTEST ok mm::alloc (2 ms)\nKTEST skip hw::x: needs hardware\nKTEST FAIL sched::y: expected 3 got 4\nKTEST done passed=1 failed=1 skipped=1\n";

    #[test]
    fn parses_all_line_kinds() {
        let s = parse(GOOD);
        assert_eq!(s.started, Some((3, String::new())));
        assert_eq!(s.ok, vec!["mm::alloc"]);
        assert_eq!(s.skips, vec![("hw::x".to_string(), "needs hardware".to_string())]);
        assert_eq!(s.fails, vec![("sched::y".to_string(), "expected 3 got 4".to_string())]);
        assert_eq!(s.done, Some((1, 1, 1)));
        assert_eq!(s.problems(), vec!["FAIL sched::y: expected 3 got 4"]);
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
