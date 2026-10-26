//! Stack guard reporting (SC-1): #PF and #DF hooks that recognise a hit on the running thread's guard page.
//!
//! A real overflow pushes into the guard; the CPU cannot deliver the #PF on that stack and raises #DF on its
//! IST1 stack, so the #DF hook is the one that normally reports. A load from the guard with a healthy stack (the
//! probe of the ktest) arrives as #PF. A hit is reported in the panic message (thread id and name, guard address,
//! stack range); klog is not used here because the overflowing thread may hold its lock (RB-1 adds try_event).
//! Both hooks pass every other fault on to the hook that was installed before them.

use hal::idt::{self, ExceptionHook, TrapFrame};
use ksync::Once;

/// What the probe's resumed load returns, so the test can tell the hook resumed it.
pub const PROBE_MARK: u64 = 0x5C1_6A2D_0000_0001;

static PREV_PF: Once<Option<ExceptionHook>> = Once::new();
static PREV_DF: Once<Option<ExceptionHook>> = Once::new();

klog::counter!(pub OVERFLOWS, "sched.stack_overflows");
klog::counter!(pub GUARD_PROBES, "sched.guard_probes");

pub fn install() {
    let pf = PREV_PF.get_or_init(|| idt::exception_hook(idt::PAGE_FAULT).filter(|h| *h as usize != pf_hook as *const () as usize));
    let df = PREV_DF.get_or_init(|| idt::exception_hook(idt::DOUBLE_FAULT).filter(|h| *h as usize != df_hook as *const () as usize));
    idt::set_exception_hook(idt::PAGE_FAULT, pf_hook);
    idt::set_exception_hook(idt::DOUBLE_FAULT, df_hook);
    klog::event!(Debug, "sched", "guard.hooks", chained_pf = pf.is_some(), chained_df = df.is_some());
}

fn chain(prev: &Once<Option<ExceptionHook>>, f: &mut TrapFrame) -> bool {
    match prev.get() {
        Some(Some(h)) => h(f),
        _ => false,
    }
}

fn pf_hook(f: &mut TrapFrame) -> bool {
    let cr2 = hal::cpu::read_cr2();
    if hal::switch::current_stack().in_guard(cr2) {
        if f.rip == hal::switch::guard_probe_site().0 {
            f.rip = hal::switch::guard_probe_site().1;
            f.rax = PROBE_MARK;
            GUARD_PROBES.inc();
            return true;
        }
        if hal::switch::take_overflow_landing(f, cr2) {
            OVERFLOWS.inc();
            return true;
        }
        overflow_panic(f, cr2, "page fault");
    }
    chain(&PREV_PF, f)
}

fn df_hook(f: &mut TrapFrame) -> bool {
    let cr2 = hal::cpu::read_cr2();
    let b = hal::switch::current_stack();
    if b.in_guard(cr2) || b.in_guard(f.rsp) {
        if hal::switch::take_overflow_landing(f, cr2) {
            OVERFLOWS.inc();
            return true;
        }
        overflow_panic(f, cr2, "double fault");
    }
    chain(&PREV_DF, f)
}

fn overflow_panic(f: &TrapFrame, cr2: u64, how: &str) -> ! {
    OVERFLOWS.inc();
    let b = hal::switch::current_stack();
    let id = hal::switch::current_id();
    // try_lock: the overflowing thread may hold the thread table's lock.
    let t = crate::table::try_get(id);
    let name = t.as_ref().map(|t| t.name.as_str()).unwrap_or("?");
    panic!(
        "kernel stack overflow ({how}): thread {id} \"{name}\" ran into its guard page at {cr2:#x} (stack {:#x}..{:#x}, guard {:#x}; rsp {:#x}, rip {:#x})",
        b.bottom, b.top, b.guard, f.rsp, f.rip
    );
}
