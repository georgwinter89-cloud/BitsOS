//! In-kernel tests (desk; V2-PLAN §9.1 point 6). A kernel built with `--features ktest` runs every registered
//! test whose name matches the CONFIG filter, prints the protocol below on COM1 and leaves QEMU through
//! isa-debug-exit. Lanes register tests in their own crates with `ktest::ktest!`.
//!
//! Protocol (xtask parses it; CONTRACTS.md):
//!   KTEST start count=<n> filter=<f>
//!   KTEST ok <name> (<ms> ms) | KTEST FAIL <name>: <reason> | KTEST skip <name>: class <class> belongs to another run
//!   KTEST done passed=<p> failed=<f> skipped=<s>
//!   KTEST PANIC <message>            (from the kernel's panic handler)
//!
//! No skipped tests (Georg 30.10., CONTRACTS section 4): which run a test belongs to is its class, chosen up front.
//! A test inside its run that cannot do its work (no device, the screen held, a missing file) is FAIL; there is no
//! way for a test to end as `skip`. `KTEST skip` is only the line for a test of another run, and xtask's ledger
//! (`cargo xtask test`) turns red for a test that ran in no run at all.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write;

/// The run a test belongs to. "QEMU plain" is the gate's debug boot of all tests, "QEMU release" its release boot
/// with `ktest_hw=on` (every class that also runs over MCP on the Mini-PC, measured against MCP's stack), "Mini-PC"
/// the MCP `ktest` tool on the real machine (host/tests/minipc: with the desktop, and "bare": the desktop stopped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// QEMU plain and release, Mini-PC with or without the desktop.
    Safe,
    /// Makes sound. QEMU plain (the sound goes to a WAV file); refused over MCP on hardware (R8e).
    Audible,
    /// Writes display registers (no switch since 21.10., R8c): runs on hardware like `Safe` over MCP.
    Display,
    /// Touches the WLAN chip: runs on hardware only while `wlan=on` is in CONFIG (R8d/R8e; the desk may set it
    /// since 21.10.).
    Wlan,
    /// QEMU plain only (only meaningful or only safe under QEMU, e.g. a real #DF; or a debug kernel's own checks).
    QemuOnly,
    /// QEMU plain and release, never hardware: it needs a device or a state only QEMU has (the fake DRM device,
    /// a driverless function, nothing holding a card).
    QemuAll,
    /// A program from its own stick tree (Brave, LibreOffice, Cobalt, ...): runs only on the Mini-PC, never under
    /// QEMU (Georg 30.10.: "Programme werden nur noch am Mini PC getestet und nicht mehr in QEMU").
    Program,
    /// Only the real machine has what it tests (the 780M, RAPL, the firmware's EDID): Mini-PC, either run.
    Hardware,
    /// Needs the screen and the input for itself: QEMU plain and release (no desktop there), and on the Mini-PC
    /// only the run with the desktop stopped.
    Screen,
    /// Changes the running picture (a flip, a mode switch, display audio) on the real machine: Mini-PC with the
    /// desktop stopped and only while CONFIG `ktest_picture=on` lifts the hold (GX-D9: the running picture must not
    /// change). Refused otherwise.
    Picture,
    /// Georg's part is needed: a tone the machine really plays (the monitor's speakers) for him to hear, or a device
    /// he plugs in (the USB headset's microphone). Mini-PC only, while CONFIG `ktest_georg=on` says he is there for
    /// that round (nothing audible in the scripts, R8e). Refused otherwise.
    WithGeorg,
    /// One QEMU boot only, named here: `Run("nv/10-nvme")` is the scenario host/tests/nv/10-nvme.sh,
    /// `Run("gate/nmi-nested")` a boot of the gate's own table. It runs when CONFIG `ktest_run=` names it: that boot
    /// supplies the device or the CONFIG line the test needs. Never on hardware.
    Run(&'static str),
    /// As `Run` under QEMU, and on the Mini-PC like `Safe` (the machine has the device itself).
    RunHw(&'static str),
}

/// CONFIG `ktest_run=` (comma-separated run names) names `want`.
pub fn run_named(runs: &str, want: &str) -> bool {
    runs.split(',').map(str::trim).any(|r| r == want)
}

impl Class {
    /// One of every class (the two named ones with a sample name), for the host tests that keep the three copies of
    /// the class rule in step: `runs_here`, MCP's `verdict`, xtask's ledger. A test in this crate counts the enum.
    pub const ALL: &'static [Class] = &[
        Class::Safe,
        Class::Audible,
        Class::Display,
        Class::Wlan,
        Class::QemuOnly,
        Class::QemuAll,
        Class::Program,
        Class::Hardware,
        Class::Screen,
        Class::Picture,
        Class::WithGeorg,
        Class::Run("x/y"),
        Class::RunHw("x/y"),
    ];

    /// Whether a test of this class belongs to a boot on a machine with (`hypervisor`) or without the hypervisor
    /// bit; `hw` = run as on hardware (CONFIG ktest_hw=on), `runs` = CONFIG ktest_run. MCP's verdict has the further
    /// rules of hardware (Audible, Wlan, the desktop).
    pub fn runs_here(self, hypervisor: bool, hw: bool, runs: &str) -> bool {
        match self {
            Class::Safe | Class::Display | Class::Screen => true,
            Class::QemuAll => hypervisor,
            Class::Program | Class::Hardware | Class::Picture | Class::WithGeorg => !hypervisor,
            Class::QemuOnly | Class::Audible | Class::Wlan => hypervisor && !hw,
            Class::Run(r) => hypervisor && run_named(runs, r),
            Class::RunHw(r) => !hypervisor || run_named(runs, r),
        }
    }
}

pub type TestResult = Result<(), String>;

/// Ends a test as `KTEST FAIL` when it cannot do its work inside its run: `return ktest::fail("no igb card");`
/// A test of another run does not get here: its class keeps it out.
pub fn fail(reason: &str) -> TestResult {
    Err(String::from(reason))
}

pub struct KtestEntry {
    pub name: &'static str,
    pub class: Class,
    pub func: fn() -> TestResult,
}

kreg::declare!(pub KTESTS: KtestEntry = "ktest");

#[doc(hidden)]
pub use kreg as __kreg;

/// `ktest::ktest!("mm.bump_alloc", Safe, test_fn);`
#[macro_export]
macro_rules! ktest {
    ($name:literal, Run($run:literal), $func:path) => {
        $crate::ktest!(@entry $name, $crate::Class::Run($run), $func);
    };
    ($name:literal, RunHw($run:literal), $func:path) => {
        $crate::ktest!(@entry $name, $crate::Class::RunHw($run), $func);
    };
    ($name:literal, $class:ident, $func:path) => {
        $crate::ktest!(@entry $name, $crate::Class::$class, $func);
    };
    (@entry $name:literal, $class:expr, $func:path) => {
        #[cfg(not(bitsos_no_ktests))]
        $crate::__kreg::register!("ktest", $crate::KtestEntry, $crate::KtestEntry { name: $name, class: $class, func: $func });
        // The lean stick kernel (`--cfg bitsos_no_ktests`, desk 24.10.) registers nothing; the reference keeps the
        // test functions "used" so no crate warns, and the linker drops them.
        #[cfg(bitsos_no_ktests)]
        const _: fn() -> $crate::TestResult = $func;
    };
}

/// Fails the test with a message unless `cond` holds: `kassert!(x == 3, "x was {}", x);`
#[macro_export]
macro_rules! kassert {
    ($cond:expr) => {
        if !$cond {
            return Err(alloc::format!("assertion failed: {} ({}:{})", stringify!($cond), file!(), line!()));
        }
    };
    ($cond:expr, $($fmt:tt)+) => {
        if !$cond {
            return Err(alloc::format!("{} ({}:{})", alloc::format!($($fmt)+), file!(), line!()));
        }
    };
}

/// A filter: empty = all; otherwise comma-separated prefixes, and `-prefix` takes tests out (a boot whose CONFIG
/// line makes one test meaningless leaves that test out by name: `nv.,-nv.irq` when every controller is polled).
pub fn matches(filter: &str, name: &str) -> bool {
    let mut wants = false;
    let mut hit = false;
    for p in filter.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match p.strip_prefix('-') {
            Some(out) if name.starts_with(out) => return false,
            Some(_) => {}
            None => {
                wants = true;
                hit |= name.starts_with(p);
            }
        }
    }
    hit || !wants
}

pub struct Summary {
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
}

fn line(s: &str) {
    // One line, never interleaved with another CPU's log line: xtask parses these.
    hal::serial::write_line(s.as_bytes());
}

/// INT-W28 (GX-7m CR): which test runs now (index into `KTESTS` + 1; 0 = none), so the panic path can name it.
static CURRENT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The test running now, if any (the panic path's "stack overflow in <test>").
pub fn current() -> Option<&'static str> {
    let i = CURRENT.load(core::sync::atomic::Ordering::Relaxed);
    KTESTS.entries().get(i.checked_sub(1)?).map(|e| e.name)
}

/// Runs the matching tests in name order. `allowed(class)` decides which classes may run here.
pub fn run(filter: &str, allowed: impl Fn(Class) -> bool, now_ns: fn() -> u64) -> Summary {
    run_checked(filter, allowed, now_ns, |_| None)
}

/// `run`, with `after(name)` called after each test that ran: a `Some(text)` is printed as
/// `KTEST note <name>: <text>` (diagnostics between tests, e.g. whether every CPU still answers an NMI).
pub fn run_checked(filter: &str, allowed: impl Fn(Class) -> bool, now_ns: fn() -> u64, after: impl Fn(&str) -> Option<String>) -> Summary {
    let mut tests: alloc::vec::Vec<&KtestEntry> = KTESTS.entries().iter().filter(|t| matches(filter, t.name)).collect();
    tests.sort_by_key(|t| t.name);
    let mut s = String::new();
    let _ = write!(s, "KTEST start count={} filter={}", tests.len(), filter);
    line(&s);
    let mut sum = Summary { passed: 0, failed: 0, skipped: 0 };
    for t in tests {
        let mut s = String::new();
        if !allowed(t.class) {
            let _ = write!(s, "KTEST skip {}: class {:?} belongs to another run", t.name, t.class);
            sum.skipped += 1;
        } else {
            let start = now_ns();
            let idx = KTESTS.entries().iter().position(|e| core::ptr::eq(e, t)).map_or(0, |i| i + 1);
            CURRENT.store(idx, core::sync::atomic::Ordering::Relaxed);
            let r = (t.func)();
            CURRENT.store(0, core::sync::atomic::Ordering::Relaxed);
            match r {
                Ok(()) => {
                    let _ = write!(s, "KTEST ok {} ({} ms)", t.name, (now_ns() - start) / 1_000_000);
                    sum.passed += 1;
                }
                Err(e) => {
                    let _ = write!(s, "KTEST FAIL {}: {}", t.name, e.replace('\n', " "));
                    sum.failed += 1;
                }
            }
        }
        line(&s);
        if allowed(t.class)
            && let Some(note) = after(t.name)
        {
            let mut s = String::new();
            let _ = write!(s, "KTEST note {}: {}", t.name, note.replace('\n', " "));
            line(&s);
        }
    }
    let mut s = String::new();
    let _ = write!(s, "KTEST done passed={} failed={} skipped={}", sum.passed, sum.failed, sum.skipped);
    line(&s);
    sum
}

/// Every test name is unique (the registry contract: duplicates fail).
fn unique_names() -> TestResult {
    let mut names: alloc::vec::Vec<&str> = KTESTS.entries().iter().map(|t| t.name).collect();
    names.sort();
    for w in names.windows(2) {
        kassert!(w[0] != w[1], "duplicate ktest name {}", w[0]);
    }
    Ok(())
}
ktest!("kreg.ktest_names_unique", Safe, unique_names);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_class() {
        let src = include_str!("lib.rs");
        let body = src.split("pub enum Class {").nth(1).and_then(|r| r.split("\n}").next()).expect("the enum");
        let variants = body.lines().filter(|l| l.starts_with("    ") && l.chars().nth(4).is_some_and(char::is_uppercase)).count();
        assert_eq!(variants, Class::ALL.len(), "a new class belongs in Class::ALL");
        for (i, a) in Class::ALL.iter().enumerate() {
            assert!(!Class::ALL[i + 1..].contains(a), "{a:?} twice");
        }
    }

    #[test]
    fn filters() {
        assert!(matches("", "mm.x"));
        assert!(matches("mm.,net", "net.dhcp"));
        assert!(!matches("mm.", "net.dhcp"));
        assert!(!matches("mm.,-mm.x", "mm.x.y"));
        assert!(matches("mm.,-mm.x", "mm.y"));
        assert!(matches("-mm.x", "net.dhcp") && !matches("-mm.x", "mm.x"));
    }

    #[test]
    fn classes_name_their_runs() {
        let plain = |c: Class| c.runs_here(true, false, "");
        let release = |c: Class| c.runs_here(true, true, "");
        let minipc = |c: Class| c.runs_here(false, false, "");
        for c in [Class::Safe, Class::Display, Class::Screen] {
            assert!(plain(c) && release(c) && minipc(c), "{c:?}");
        }
        assert!(plain(Class::QemuAll) && release(Class::QemuAll) && !minipc(Class::QemuAll));
        for c in [Class::QemuOnly, Class::Audible, Class::Wlan] {
            assert!(plain(c) && !release(c) && !minipc(c), "{c:?}");
        }
        for c in [Class::Program, Class::Hardware, Class::Picture, Class::WithGeorg] {
            assert!(!plain(c) && !release(c) && minipc(c), "{c:?}");
        }
        assert!(!plain(Class::Run("nv/10")) && !minipc(Class::Run("nv/10")));
        assert!(Class::Run("nv/10").runs_here(true, false, "x, nv/10"));
        assert!(!Class::Run("nv/10").runs_here(true, false, "nv/100"));
        assert!(!Class::Run("nv/10").runs_here(false, false, "nv/10"));
        assert!(!plain(Class::RunHw("nv/10")) && minipc(Class::RunHw("nv/10")));
        assert!(Class::RunHw("nv/10").runs_here(true, false, "nv/10"));
    }
}
