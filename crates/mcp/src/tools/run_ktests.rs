//! `ktest`: runs registered in-kernel tests by name prefix (`spec/net.md` §5.2, the replacement for V1's
//! `run_asm` probing) under R8e's class rules: on hardware `Safe` and `Display` tests run (R8c: display writes
//! need no switch since 21.10.); `Audible` is refused, `Wlan` runs only with `wlan=on` in CONFIG (WL-4 CR 3), and `QemuOnly` tests are skipped. Under QEMU (hypervisor bit) every class runs, as at a ktest boot.
//!
//! One run at a time (tests open ports and hold devices). The run goes in name order and writes each result
//! line to the call's output, so a long run that outlives its deadline can be followed with `output` and ended
//! between two tests with `stop`.

use super::{bad, opt_str, text};
use crate::{Json, ToolContext, ToolError, ToolResult, mcp_tool};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ktest::Class;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Run,
    /// Not run, reported as `skip` (QemuOnly on hardware).
    Skip(&'static str),
    /// Not run, reported as `refused` (R8e).
    Refuse(&'static str),
}

/// The class rule (pure): `hardware` = no hypervisor bit, `wlan_on` = CONFIG `wlan=on` (PC-4's opt-in).
pub fn verdict(class: Class, hardware: bool, wlan_on: bool) -> Verdict {
    if !hardware {
        return Verdict::Run;
    }
    match class {
        Class::Safe | Class::Display => Verdict::Run,
        Class::QemuOnly => Verdict::Skip("QemuOnly: runs only under QEMU"),
        Class::Audible => Verdict::Refuse("Audible: makes sound; not over MCP on hardware (R8e)"),
        Class::Wlan if wlan_on => Verdict::Run,
        Class::Wlan => Verdict::Refuse("wlan: needs wlan=on (R8d/R8e)"),
    }
}

static RUNNING: AtomicBool = AtomicBool::new(false);

/// A run is going on (a ktest that calls the tool itself checks this: inside a run over MCP the nested call
/// would only be refused).
pub(crate) fn run_active() -> bool {
    RUNNING.load(Ordering::Acquire)
}

/// Holds the calling thread on CPU 0 for a run and gives it its previous pin back: the tests were written for
/// the boot thread (pinned to CPU 0 at a ktest boot) and some check "the BSP" or per-CPU state of CPU 0.
struct OnBsp(Option<usize>);
impl OnBsp {
    fn new() -> Self {
        let old = sched::current_thread().and_then(|t| t.pinned());
        if !sched::pin_current(Some(0)) {
            klog::event!(Warn, "mcp", "ktest.pin_failed", cpu = sched::current_cpu());
        }
        OnBsp(old)
    }
}
impl Drop for OnBsp {
    fn drop(&mut self) {
        sched::pin_current(self.0);
    }
}

struct Busy;
impl Drop for Busy {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct Tally {
    passed: u32,
    failed: u32,
    skipped: u32,
    refused: u32,
    not_run: u32,
}

/// Runs the tests matching `filter` with `hardware` and `wlan_on` deciding the class rule; `ctx` gets every line
/// as output.
pub(crate) fn run(ctx: &ToolContext, filter: &str, hardware: bool, wlan_on: bool) -> ToolResult {
    let mut tests: Vec<&'static ktest::KtestEntry> = ktest::KTESTS.entries().iter().filter(|t| ktest::matches(filter, t.name)).collect();
    if tests.is_empty() {
        return Err(ToolError::Failed(format!("no ktest matches {filter:?}")));
    }
    tests.sort_by_key(|t| t.name);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return Err(ToolError::Refused(String::from("another ktest run is going on (one at a time: tests share ports and devices)")));
    }
    let _busy = Busy;
    let _bsp = OnBsp::new();
    // `ktest::skip` marks its message with a prefix the runner recognises; take it from there, not a copy.
    let skip_mark = ktest::skip("").err().unwrap_or_default();
    let mut lines = Vec::new();
    let mut t = Tally::default();
    klog::event!(Info, "mcp", "ktest.start", filter = filter, count = tests.len(), hardware = hardware);
    for e in &tests {
        if ctx.stop_requested() {
            t.not_run += 1;
            continue;
        }
        let line = match verdict(e.class, hardware, wlan_on) {
            Verdict::Skip(why) => {
                t.skipped += 1;
                format!("skip {}: {why}", e.name)
            }
            Verdict::Refuse(why) => {
                t.refused += 1;
                klog::event!(Info, "mcp", "ktest.refused", name = e.name, class = format!("{:?}", e.class));
                format!("refused {}: {why}", e.name)
            }
            Verdict::Run => {
                let t0 = ctx.now_ns();
                let r = (e.func)();
                let ms = ctx.now_ns().saturating_sub(t0) / 1_000_000;
                match r {
                    Ok(()) => {
                        t.passed += 1;
                        format!("ok {} ({ms} ms)", e.name)
                    }
                    Err(m) if m.starts_with(&skip_mark) => {
                        t.skipped += 1;
                        format!("skip {}: {}", e.name, m[skip_mark.len()..].replace('\n', " "))
                    }
                    Err(m) => {
                        t.failed += 1;
                        klog::event!(Warn, "mcp", "ktest.fail", name = e.name, ms = ms);
                        format!("FAIL {}: {}", e.name, m.replace('\n', " "))
                    }
                }
            }
        };
        ctx.output(&line);
        ctx.output("\n");
        lines.push(line);
    }
    let mut head = format!("ktest {filter:?}: passed={} failed={} skipped={} refused={}", t.passed, t.failed, t.skipped, t.refused);
    if t.not_run > 0 {
        head.push_str(&format!(" not_run={} (stopped)", t.not_run));
    }
    head.push_str(match (hardware, wlan_on) {
        (true, false) => " (hardware: only Safe and Display run)",
        (true, true) => " (hardware: Safe, Display and Wlan run)",
        (false, _) => " (QEMU: every class runs)",
    });
    klog::event!(Info, "mcp", "ktest.done", filter = filter, passed = t.passed, failed = t.failed, skipped = t.skipped, refused = t.refused, not_run = t.not_run);
    if t.passed + t.failed + t.skipped == 0 && t.refused > 0 {
        return Err(ToolError::Refused(format!("{head}\n{}", lines.join("\n"))));
    }
    lines.insert(0, head);
    text(lines)
}

fn ktest_tool(ctx: &ToolContext, args: &Json) -> ToolResult {
    let filter = opt_str(args, "filter")?.unwrap_or("");
    if filter.len() > 4096 || filter.chars().any(char::is_control) {
        return Err(bad("filter"));
    }
    let hardware = !hal::cpu::detect().hypervisor;
    run(ctx, filter, hardware, pci::registry::opted_in("wlan"))
}

mcp_tool!(
    name: "ktest",
    owner: "MC",
    safety: Change,
    description: "Runs in-kernel tests (ktests) whose names start with one of the comma-separated prefixes in filter (empty: all), in name order, one run at a time. On hardware classes Safe and Display run: QemuOnly is skipped, Audible is refused, Wlan runs only with wlan=on in CONFIG (R8e). Answers one line per test (ok/FAIL/skip/refused) under a summary; a run past its deadline goes on (follow it with output, end it between two tests with stop).",
    schema: r#"{"type":"object","properties":{"filter":{"type":"string","description":"comma-separated name prefixes, e.g. \"nw.stack,mcp.\""}}}"#,
    handler: ktest_tool,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_rules() {
        assert_eq!(verdict(Class::Safe, true, false), Verdict::Run);
        assert_eq!(verdict(Class::Display, true, false), Verdict::Run);
        assert!(matches!(verdict(Class::QemuOnly, true, false), Verdict::Skip(_)));
        for c in [Class::Audible, Class::Wlan] {
            assert!(matches!(verdict(c, true, false), Verdict::Refuse(_)), "{c:?}");
            assert_eq!(verdict(c, false, false), Verdict::Run, "{c:?} under QEMU");
        }
        // WL-4 CR 3: `wlan=on` lets the Wlan class run on hardware, never Audible.
        assert_eq!(verdict(Class::Wlan, true, true), Verdict::Run);
        assert!(matches!(verdict(Class::Audible, true, true), Verdict::Refuse(_)));
        assert_eq!(verdict(Class::QemuOnly, false, false), Verdict::Run);
        assert_eq!(verdict(Class::Display, false, false), Verdict::Run);
    }
}
