//! What a sampling profiler (SELFOPT-1, the perf events of `syscall::perf`) reads from inside an interrupt
//! handler: the interrupted instruction, its stack pointer and frame pointer, the privilege it ran at, and the
//! return addresses of an interrupted kernel stack (frame pointers, `panic`'s walker). Linux keeps the same in
//! `get_irq_regs()`.
//!
//! `vectors::dispatch` publishes the address of the trap frame it hands to a handler in this CPU's block and takes
//! it away again before it returns (`percpu::set_irq_frame`); outside a handler of `dispatch` everything here is
//! `None`/0. Interrupts are off in a handler, so no other `dispatch` on this CPU can replace the frame meanwhile; an
//! NMI never calls in here.

use crate::idt::TrapFrame;
use crate::panic::Step;

/// The interrupted code as its trap frame shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interrupted {
    pub rip: u64,
    pub rsp: u64,
    pub rbp: u64,
    /// The interrupt came from user mode (CPL 3).
    pub user: bool,
}

fn frame() -> Option<&'static TrapFrame> {
    let p = crate::percpu::irq_frame();
    if p == 0 {
        return None;
    }
    // SAFETY: a non-zero value is the address `dispatch` was given for the interrupt this CPU handles right now
    // (it lives on this CPU's stack until `dispatch` returns, and `dispatch` clears it first). The caller runs
    // inside that handler, so the frame outlives every use here; it is only read.
    Some(unsafe { &*(p as *const TrapFrame) })
}

/// The interrupted code, when called from a handler `vectors::dispatch` runs.
pub fn interrupted() -> Option<Interrupted> {
    frame().map(|f| Interrupted { rip: f.rip, rsp: f.rsp, rbp: f.rbp, user: f.cs & 3 != 0 })
}

/// Hands `visit` the return addresses of the interrupted kernel stack, innermost first, at most `max` of them:
/// a return address read at a function's first instructions, then the frame-pointer chain, crossing the trap
/// frames of nested exceptions (their interrupted rip counts as one entry) and ending at a trap from user mode.
/// Returns how many it handed over; 0 outside a handler or when user code was interrupted.
pub fn interrupted_kernel_chain(max: usize, visit: &mut dyn FnMut(u64)) -> usize {
    let Some(f) = frame() else { return 0 };
    if f.cs & 3 != 0 || max == 0 {
        return 0;
    }
    let mut n = 0;
    if let Some(Step::Call(a) | Step::Guess(a)) = crate::panic::entry_return(f.rip, f.rsp) {
        visit(a);
        n += 1;
    }
    let mut ended = false;
    crate::panic::walk_visit_from(f.rsp, f.rbp, max - n, &mut |s| {
        if ended {
            return;
        }
        match s {
            Step::Call(a) | Step::Guess(a) => {
                visit(a);
                n += 1;
            }
            Step::Trap { rip, user: false, .. } => {
                visit(rip);
                n += 1;
            }
            Step::Trap { user: true, .. } | Step::None => ended = true,
        }
    });
    n
}
