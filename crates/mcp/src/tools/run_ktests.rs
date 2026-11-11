//! `ktest`: runs registered in-kernel tests by name prefix (`spec/net.md` §5.2, the replacement for V1's
//! `run_asm` probing) under R8e's class rules: on hardware `Safe`, `Display`, `Program`, `Hardware` and `RunHw` tests
//! run (R8c: display writes need no switch since 21.10.); `Audible` is refused, `Wlan` runs only with `wlan=on` in
//! CONFIG (WL-4 CR 3); `Screen` runs only while no program holds the screen, `Picture` also only with
//! `ktest_picture=on`; QEMU's classes belong to another run. Under QEMU (hypervisor bit) a class runs as at a ktest
//! boot, except that `ktest_hw` does not apply: the tool runs QemuOnly tests as well.
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
    /// Not run, reported as `skip`: the test belongs to another run (TS-1, CONTRACTS section 4).
    Skip(&'static str),
    /// Not run, reported as `refused` (R8e).
    Refuse(&'static str),
}

/// What the class rule looks at, apart from the class.
#[derive(Clone, Copy, Debug)]
pub struct Here<'a> {
    /// No hypervisor bit.
    pub hardware: bool,
    /// CONFIG `wlan=on` (PC-4's opt-in).
    pub wlan_on: bool,
    /// A program holds the screen now (the desktop on the Mini-PC).
    pub screen_held: bool,
    /// CONFIG `ktest_picture=on`: the hold on tests that change the picture is lifted (GX-D9).
    pub picture_on: bool,
    /// CONFIG `ktest_georg=on`: Georg is there for this round (`WithGeorg`).
    pub georg_on: bool,
    /// CONFIG `ktest_run=`.
    pub runs: &'a str,
}

/// The class rule (pure).
pub fn verdict(class: Class, h: Here) -> Verdict {
    if !h.hardware {
        return if class.runs_here(true, false, h.runs) { Verdict::Run } else { Verdict::Skip("belongs to another run: not this QEMU boot") };
    }
    match class {
        Class::Safe | Class::Display | Class::Program | Class::Hardware | Class::RunHw(_) => Verdict::Run,
        Class::Screen if h.screen_held => Verdict::Skip("belongs to the Mini-PC run without the desktop: a program holds the screen"),
        Class::Screen => Verdict::Run,
        Class::Picture if !h.picture_on => Verdict::Refuse("changes the picture: needs ktest_picture=on (GX-D9)"),
        Class::Picture if h.screen_held => Verdict::Skip("belongs to the Mini-PC run without the desktop: a program holds the screen"),
        Class::Picture => Verdict::Run,
        Class::QemuOnly | Class::QemuAll | Class::Run(_) => Verdict::Skip("belongs to a QEMU run"),
        Class::WithGeorg if h.georg_on => Verdict::Run,
        Class::WithGeorg => Verdict::Refuse("WithGeorg: a tone to hear or a device to plug in; needs ktest_georg=on (R8e)"),
        Class::Audible => Verdict::Refuse("Audible: makes sound; not over MCP on hardware (R8e)"),
        Class::Wlan if h.wlan_on => Verdict::Run,
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

/// Runs the tests matching `filter` with `here` deciding the class rule; `ctx` gets every line as output.
pub(crate) fn run(ctx: &ToolContext, filter: &str, here: Here) -> ToolResult {
    run_with(ctx, filter, here, false)
}

/// MC-8a: what the gate (`/dev/bitsos/mcp`) says for a test the service does not start.
pub(crate) const NOT_SAFE: &str = "not Safe: the user-space MCP service starts only Safe-class tests (MC-8a)";

/// The classes the user-space service starts: what writes no display register, makes no sound, leaves the WLAN chip
/// alone and is safe on a real machine. `Run` is QEMU's own (`RunHw` is `Safe` on the machine that has the device).
fn service_starts(c: Class) -> bool {
    matches!(c, Class::Safe | Class::Program | Class::Hardware | Class::Screen | Class::RunHw(_) | Class::QemuAll)
}

/// `run`; with `safe_only` (the gate) a test of any other class is skipped with `NOT_SAFE`, also under QEMU.
pub(crate) fn run_with(ctx: &ToolContext, filter: &str, here: Here, safe_only: bool) -> ToolResult {
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
    let mut lines = Vec::new();
    let mut t = Tally::default();
    klog::event!(Info, "mcp", "ktest.start", filter = filter, count = tests.len(), hardware = here.hardware);
    for e in &tests {
        if ctx.stop_requested() {
            t.not_run += 1;
            continue;
        }
        // Each test decides again: a test of the run may have started or ended the program that holds the screen.
        let here = Here { screen_held: console::vt::kd_owner() != 0, ..here };
        // Program tests read a stick tree and start its program as uid 1000, as the Safe ul.pkg tests did before
        // Georg's rule of 30.10. moved them to the Mini-PC: the user-space service starts them too.
        let v = if safe_only && !service_starts(e.class) { Verdict::Skip(NOT_SAFE) } else { verdict(e.class, here) };
        let line = match v {
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
    head.push_str(match (here.hardware, here.wlan_on) {
        (true, false) => " (hardware: skip = belongs to another run; Wlan refused)",
        (true, true) => " (hardware: skip = belongs to another run)",
        (false, _) => " (QEMU: skip = belongs to another run)",
    });
    klog::event!(Info, "mcp", "ktest.done", filter = filter, passed = t.passed, failed = t.failed, skipped = t.skipped, refused = t.refused, not_run = t.not_run);
    if t.passed + t.failed + t.skipped == 0 && t.refused > 0 {
        return Err(ToolError::Refused(format!("{head}\n{}", lines.join("\n"))));
    }
    lines.insert(0, head);
    text(lines)
}

fn ktest_tool(ctx: &ToolContext, args: &Json) -> ToolResult {
    ktest_call(ctx, args, false)
}

/// MC-8a: `ktest` as the gate runs it (Safe class only).
pub(crate) fn ktest_safe_tool(ctx: &ToolContext, args: &Json) -> ToolResult {
    ktest_call(ctx, args, true)
}

fn ktest_call(ctx: &ToolContext, args: &Json, safe_only: bool) -> ToolResult {
    let filter = opt_str(args, "filter")?.unwrap_or("");
    if filter.len() > 4096 || filter.chars().any(char::is_control) {
        return Err(bad("filter"));
    }
    let runs = pci::registry::config_value("ktest_run").unwrap_or_default();
    let here = Here {
        hardware: !hal::cpu::detect().hypervisor,
        wlan_on: pci::registry::opted_in("wlan"),
        screen_held: false,
        picture_on: pci::registry::config_value("ktest_picture").as_deref() == Some("on"),
        georg_on: pci::registry::config_value("ktest_georg").as_deref() == Some("on"),
        runs: &runs,
    };
    run_with(ctx, filter, here, safe_only)
}

mcp_tool!(
    name: "ktest",
    owner: "MC",
    safety: Change,
    description: "Runs in-kernel tests (ktests) whose names start with one of the comma-separated prefixes in filter (empty: all), in name order, one run at a time. Which tests run is decided by their class: a test of another run shows as skip (on hardware the QEMU classes; Screen tests while a program holds the screen), Audible is refused on hardware, Wlan runs only with wlan=on in CONFIG (R8e), Picture only with ktest_picture=on, WithGeorg only with ktest_georg=on. A test that runs and cannot do its work is FAIL. Answers one line per test (ok/FAIL/skip/refused) under a summary; a run past its deadline goes on (follow it with output, end it between two tests with stop).",
    schema: r#"{"type":"object","properties":{"filter":{"type":"string","description":"comma-separated name prefixes, e.g. \"nw.stack,mcp.\""}}}"#,
    handler: ktest_tool,
);

#[cfg(test)]
mod tests {
    use super::*;

    const QEMU: Here = Here { hardware: false, wlan_on: false, screen_held: false, picture_on: false, georg_on: false, runs: "" };
    const HW: Here = Here { hardware: true, ..QEMU };

    /// `verdict` and `Class::runs_here` are two copies of the rule, and host/tests/minipc/ktests.py reads the words.
    #[test]
    fn verdict_agrees_with_the_kernels_rule_and_the_scripts() {
        let lifted = Here { wlan_on: true, picture_on: true, georg_on: true, ..HW };
        for &c in Class::ALL {
            let (kernel, mcp) = (c.runs_here(false, false, ""), verdict(c, lifted) == Verdict::Run);
            // Wlan is the one the kernel's boot rule leaves to MCP: over MCP with `wlan=on`, in no ktest boot on hardware.
            assert_eq!(kernel, mcp && c != Class::Wlan, "{c:?} on hardware with every switch on");
            let named = if let Class::Run(r) | Class::RunHw(r) = c { r } else { "" };
            let here = Here { runs: named, ..QEMU };
            assert_eq!(c.runs_here(true, false, named), verdict(c, here) == Verdict::Run, "{c:?} under QEMU");
            match verdict(c, HW) {
                Verdict::Run => {}
                Verdict::Skip(why) => assert!(why.starts_with("belongs to"), "{c:?}: {why}"),
                Verdict::Refuse(why) => {
                    assert!(["Audible:", "WithGeorg:", "wlan:", "changes the picture:"].iter().any(|p| why.starts_with(p)), "{c:?}: {why}")
                }
            }
        }
    }

    #[test]
    fn class_rules() {
        assert_eq!(verdict(Class::Safe, HW), Verdict::Run);
        assert_eq!(verdict(Class::Display, HW), Verdict::Run);
        assert!(matches!(verdict(Class::QemuOnly, HW), Verdict::Skip(_)));
        assert!(matches!(verdict(Class::QemuAll, HW), Verdict::Skip(_)));
        assert_eq!(verdict(Class::QemuAll, QEMU), Verdict::Run);
        for c in [Class::Audible, Class::Wlan] {
            assert!(matches!(verdict(c, HW), Verdict::Refuse(_)), "{c:?}");
            assert_eq!(verdict(c, QEMU), Verdict::Run, "{c:?} under QEMU");
        }
        // WL-4 CR 3: `wlan=on` lets the Wlan class run on hardware, never Audible.
        let wlan = Here { wlan_on: true, ..HW };
        assert_eq!(verdict(Class::Wlan, wlan), Verdict::Run);
        assert!(matches!(verdict(Class::Audible, wlan), Verdict::Refuse(_)));
        assert_eq!(verdict(Class::QemuOnly, QEMU), Verdict::Run);
        assert_eq!(verdict(Class::Display, QEMU), Verdict::Run);
        // Georg 30.10.: a program tree's tests run on the Mini-PC only, never under QEMU.
        for c in [Class::Program, Class::Hardware] {
            assert_eq!(verdict(c, HW), Verdict::Run);
            assert!(matches!(verdict(c, QEMU), Verdict::Skip(_)));
        }
        // The kernel's ktest boot draws the same line.
        assert!(Class::Program.runs_here(false, false, "") && !Class::Program.runs_here(true, false, "") && !Class::Program.runs_here(true, true, ""));
        assert!(Class::QemuOnly.runs_here(true, false, "") && !Class::QemuOnly.runs_here(true, true, "") && !Class::QemuOnly.runs_here(false, false, ""));
        assert!(Class::Safe.runs_here(false, false, "") && Class::Safe.runs_here(true, true, ""));
    }

    #[test]
    fn the_desktop_decides_screen_tests() {
        let held = Here { screen_held: true, ..HW };
        assert_eq!(verdict(Class::Screen, HW), Verdict::Run);
        assert!(matches!(verdict(Class::Screen, held), Verdict::Skip(_)));
        assert_eq!(verdict(Class::Screen, Here { screen_held: true, ..QEMU }), Verdict::Run);
    }

    #[test]
    fn a_change_of_the_picture_is_held_back() {
        // GX-D9: never by accident, whatever else holds.
        assert!(matches!(verdict(Class::Picture, HW), Verdict::Refuse(_)));
        assert!(matches!(verdict(Class::Picture, QEMU), Verdict::Skip(_)));
        let lifted = Here { picture_on: true, ..HW };
        assert_eq!(verdict(Class::Picture, lifted), Verdict::Run);
        assert!(matches!(verdict(Class::Picture, Here { screen_held: true, ..lifted }), Verdict::Skip(_)));
        assert!(!service_starts(Class::Picture));
    }

    #[test]
    fn a_real_tone_only_with_georg() {
        assert!(matches!(verdict(Class::WithGeorg, HW), Verdict::Refuse(_)));
        assert!(matches!(verdict(Class::WithGeorg, QEMU), Verdict::Skip(_)));
        assert_eq!(verdict(Class::WithGeorg, Here { georg_on: true, ..HW }), Verdict::Run);
        assert!(!service_starts(Class::WithGeorg));
    }

    #[test]
    fn a_named_run() {
        let nv = Here { runs: "nv/10-nvme", ..QEMU };
        assert_eq!(verdict(Class::Run("nv/10-nvme"), nv), Verdict::Run);
        assert_eq!(verdict(Class::RunHw("nv/10-nvme"), nv), Verdict::Run);
        assert!(matches!(verdict(Class::Run("nv/10-nvme"), QEMU), Verdict::Skip(_)));
        assert!(matches!(verdict(Class::RunHw("nv/10-nvme"), QEMU), Verdict::Skip(_)));
        assert!(matches!(verdict(Class::Run("nv/10-nvme"), Here { runs: "nv/10-nvme", ..HW }), Verdict::Skip(_)));
        assert_eq!(verdict(Class::RunHw("nv/10-nvme"), HW), Verdict::Run);
    }
}
