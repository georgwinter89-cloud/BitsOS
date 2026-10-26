//! In-kernel tests (desk; V2-PLAN §9.1 point 6). A kernel built with `--features ktest` runs every registered
//! test whose name matches the CONFIG filter, prints the protocol below on COM1 and leaves QEMU through
//! isa-debug-exit. Lanes register tests in their own crates with `ktest::ktest!`.
//!
//! Protocol (xtask parses it; CONTRACTS.md):
//!   KTEST start count=<n> filter=<f>
//!   KTEST ok <name> (<ms> ms) | KTEST FAIL <name>: <reason> | KTEST skip <name>: <reason>
//!   KTEST done passed=<p> failed=<f> skipped=<s>
//!   KTEST PANIC <message>            (from the kernel's panic handler)
//! Classes (R8e): on hardware only `Safe` runs unless the matching switch is on.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Safe,
    /// Makes sound.
    Audible,
    /// Writes display registers (no switch since 21.10., R8c): runs on hardware like `Safe` over MCP.
    Display,
    /// Touches the WLAN chip: runs on hardware only while `wlan=on` is in CONFIG (R8d/R8e; the desk may set it
    /// since 21.10.).
    Wlan,
    /// Only meaningful or only safe under QEMU (e.g. a real #DF); shows as `skip` on hardware.
    QemuOnly,
}

pub type TestResult = Result<(), String>;

const SKIP: &str = "\u{1}skip:";

/// Ends a test as `KTEST skip` (missing hardware, feature absent): `return ktest::skip("no igb card");`
pub fn skip(reason: &str) -> TestResult {
    Err(alloc::format!("{SKIP}{reason}"))
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
    ($name:literal, $class:ident, $func:path) => {
        #[cfg(not(bitsos_no_ktests))]
        $crate::__kreg::register!("ktest", $crate::KtestEntry, $crate::KtestEntry { name: $name, class: $crate::Class::$class, func: $func });
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

/// A filter: empty = all; otherwise comma-separated prefixes.
pub fn matches(filter: &str, name: &str) -> bool {
    let f = filter.trim();
    f.is_empty() || f.split(',').map(str::trim).filter(|p| !p.is_empty()).any(|p| name.starts_with(p))
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
            let _ = write!(s, "KTEST skip {}: class {:?} not allowed here", t.name, t.class);
            sum.skipped += 1;
        } else {
            let start = now_ns();
            match (t.func)() {
                Ok(()) => {
                    let _ = write!(s, "KTEST ok {} ({} ms)", t.name, (now_ns() - start) / 1_000_000);
                    sum.passed += 1;
                }
                Err(e) if e.starts_with(SKIP) => {
                    let _ = write!(s, "KTEST skip {}: {}", t.name, e[SKIP.len()..].replace('\n', " "));
                    sum.skipped += 1;
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
    fn filters() {
        assert!(matches("", "mm.x"));
        assert!(matches("mm.,net", "net.dhcp"));
        assert!(!matches("mm.", "net.dhcp"));
    }
}
