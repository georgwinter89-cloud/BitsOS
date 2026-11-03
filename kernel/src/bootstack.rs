//! INT-W28 (GX-7m CR): the in-kernel tests run on the boot thread's static stack, which has no guard page; an
//! overflow there runs silently over the statics below it (once it showed only as a second panic in the early
//! console). A canary fills the stack's lowest `CANARY` bytes before the tests; after each test, and first thing
//! in a panic, a damaged canary is reported as "stack overflow in <test>".
//!
//! Measuring (CONFIG `ktest_stack=<KiB>`, INT-W28 post-integration): the whole stack below the runner is filled
//! with the canary word before each test; after it, the deepest changed word gives the test's stack use, logged
//! (`ktest stack.use`) and noted when it is over the budget. On the Mini-PC MCP runs the Safe tests on its 64 KiB
//! request thread (CONTRACTS §5), so `cargo xtask ktest --release <filter> --config-extra
//! "ktest_hw=on;ktest_stack=48"` checks a release build's Safe tests against 64 KiB less MCP's own frames.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The boot stack: larger in a ktest build (the debug fake GPU chains need about 70 KiB of it, GX-7m).
#[cfg(feature = "ktest")]
pub const SIZE: usize = 512 * 1024;
#[cfg(not(feature = "ktest"))]
pub const SIZE: usize = 128 * 1024;

/// Bytes at the bottom that hold the canary: a test reaching into them is reported, before the stack ends.
const CANARY: usize = 16 * 1024;
const WORD: u64 = 0x5354_4b5f_4341_4e59; // "STK_CANY"

static ARMED: AtomicBool = AtomicBool::new(false);
/// Measuring: the budget in bytes (0 = off) and the top of the region filled before the running test.
static BUDGET: AtomicUsize = AtomicUsize::new(0);
static TOP: AtomicUsize = AtomicUsize::new(0);
/// Room left unfilled below the filler's own frame (it and its callers sit just under the runner's frame).
const GAP: usize = 512;

unsafe extern "C" {
    static boot_stack_bottom: u8;
}

fn bottom() -> usize {
    // Only the address of the linker symbol (the stack's lowest byte) is taken, no access.
    core::ptr::addr_of!(boot_stack_bottom) as usize
}

fn here() -> usize {
    let probe = 0u8;
    core::ptr::addr_of!(probe) as usize
}

/// Fills the canary; only from the boot thread while it runs well above the canary (the ktest runner's start).
pub fn arm() {
    let b = bottom();
    if here() < b + CANARY + 4096 || here() > b + SIZE {
        return; // not on the boot stack, or already deep in it: no canary
    }
    for i in 0..CANARY / 8 {
        // SAFETY: [b, b + CANARY) is the unused low end of the boot stack this thread runs on (checked above:
        // rsp is above it), aligned to 16 by `.p2align 4`; nothing else uses that memory.
        unsafe { core::ptr::write_volatile((b + i * 8) as *mut u64, WORD) };
    }
    ARMED.store(true, Ordering::Release);
}

/// Measuring from now on with `budget_kib` KiB per test (0 = only the canary); fills the stack like `arm`.
pub fn measure(budget_kib: usize) {
    BUDGET.store(budget_kib * 1024, Ordering::Relaxed);
    if budget_kib == 0 {
        arm();
    } else {
        fill();
    }
}

/// Fills the boot stack from its bottom up to `GAP` below the caller with the canary word (canary included).
fn fill() {
    let b = bottom();
    let at = here();
    if at < b + CANARY + GAP + 4096 || at > b + SIZE {
        TOP.store(0, Ordering::Relaxed);
        return;
    }
    let top = (at - GAP) & !7;
    for a in (b..top).step_by(8) {
        // SAFETY: [b, top) lies below this frame on the boot stack this thread runs on (checked above), at least
        // GAP bytes under the deepest live byte; nothing but a later call or an interrupt on this thread uses it.
        unsafe { core::ptr::write_volatile(a as *mut u64, WORD) };
    }
    TOP.store(top, Ordering::Relaxed);
    ARMED.store(true, Ordering::Release);
}

/// Bytes below the filled top that the last test (or an interrupt during it) changed; None when not measuring.
fn used() -> Option<usize> {
    let top = TOP.load(Ordering::Relaxed);
    if top == 0 {
        return None;
    }
    let b = bottom();
    for a in (b..top).step_by(8) {
        // SAFETY: as in `fill`: a read of the region filled there, below this frame.
        if unsafe { core::ptr::read_volatile(a as *const u64) } != WORD {
            return Some(top - a);
        }
    }
    Some(0)
}

/// Bytes of the canary that were overwritten (0 = intact or not armed).
pub fn damaged() -> usize {
    if !ARMED.load(Ordering::Acquire) {
        return 0;
    }
    let b = bottom();
    // The deepest overwritten word: the canary is used from its top down.
    for i in 0..CANARY / 8 {
        // SAFETY: as in `arm`: a read of the boot stack's low end, which was filled there.
        if unsafe { core::ptr::read_volatile((b + i * 8) as *const u64) } != WORD {
            return CANARY - i * 8;
        }
    }
    0
}

/// After a test: a note when the test reached into the canary, or (measuring) used more than the budget; then
/// the canary (measuring: the whole stack below) is filled again.
pub fn check_after(name: &str) -> Option<alloc::string::String> {
    let d = damaged();
    let mut note = None;
    if d != 0 {
        let total = SIZE / 1024;
        note = Some(alloc::format!(
            "stack overflow in {name}: the boot stack ({total} KiB) was used into its last {} KiB (canary {d} bytes)",
            CANARY / 1024
        ));
        klog::event!(Error, "ktest", "boot_stack.overflow", test = name, canary_bytes = d as u64);
    }
    let budget = BUDGET.load(Ordering::Relaxed);
    if budget != 0 {
        if let Some(u) = used() {
            klog::event!(Info, "ktest", "stack.use", test = name, kib = u.div_ceil(1024) as u64);
            if u > budget {
                let over = alloc::format!(
                    "stack use of {name}: about {} KiB, over the budget of {} KiB (ktest_stack: MCP's 64 KiB thread)",
                    u.div_ceil(1024),
                    budget / 1024
                );
                note = Some(match note {
                    Some(n) => alloc::format!("{n}; {over}"),
                    None => over,
                });
            }
        }
        fill();
    } else if d != 0 {
        arm();
    }
    note
}
