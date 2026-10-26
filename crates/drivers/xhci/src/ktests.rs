//! In-kernel tests (US-1) under QEMU's qemu-xhci (32-byte contexts, no scratchpads, one event ring segment).
//! `xtask qemu`/`ktest` always give the machine one qemu-xhci (the boot stick hangs on it); the scenario
//! `host/tests/us/10-xhci.sh` adds two more (qemu-xhci and nec-usb-xhci) for the several-controllers case.
//! A machine without any xHCI function skips. `contract.quiesce` only quiesces its own test device, but a test
//! here may leave a controller quiesced, so each test first restarts a controller that is not running.

use crate::driver::{controllers, is_xhci, Xhci};
use crate::hc::{CmdError, State};
use crate::regs::{cmd, sts, ContextSize};
use crate::trb::cc;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use ktest::{kassert, ktest, TestResult};

const QEMU_VENDOR: u16 = 0x1B36;

pub(crate) fn running() -> Result<Option<Vec<Arc<Xhci>>>, String> {
    let present = pci::devices().iter().filter(|f| is_xhci(f)).count();
    if present == 0 {
        return Ok(None);
    }
    let list = controllers();
    if list.len() != present {
        return Err(format!("{present} xHCI functions on the bus, the driver took {}", list.len()));
    }
    for c in &list {
        if c.state() != State::Running {
            c.restart("ktest").map_err(|e| format!("{}: restart: {e} (reason before: {})", c.name(), c.reason()))?;
        }
    }
    Ok(Some(list))
}

fn running_all() -> TestResult {
    let Some(list) = running()? else { return ktest::skip("no xHCI controller in this machine") };
    for c in &list {
        let name = c.name();
        kassert!(c.state() == State::Running, "{name}: state {}", c.state().as_str());
        let (uc, us) = (c.usbcmd(), c.usbsts());
        kassert!(uc & cmd::RS != 0, "{name}: USBCMD {uc:#x} without RS");
        kassert!(us & sts::HCH == 0, "{name}: USBSTS {us:#x} halted");
        kassert!(us & sts::HSE == 0, "{name}: USBSTS {us:#x} reports HSE");
        kassert!(c.env().pci().command() & 0x4 != 0, "{name}: bus master off while running");
        if c.env().pci().vendor == QEMU_VENDOR {
            kassert!(c.caps().csz == ContextSize::B32, "{name}: QEMU has 32-byte contexts");
        }
        let (_, _, segs) = c.ring_info();
        kassert!(segs >= 1 && segs <= 1usize << c.caps().erst_max, "{name}: {segs} event ring segments");
        // US-ev (B4): the event ring and input on KernelRt, transfer ends (storage callbacks) on Normal.
        for (thread, want) in [(format!("{name}-events"), sched::Class::KernelRt), (format!("{name}-done"), sched::Class::Normal)] {
            kassert!(sched::threads().iter().any(|(_, n, class, _)| *n == thread && *class == want),
                "{name}: no {want:?} thread {thread}");
        }
    }
    for c in &list {
        let (n, sum) = (c.stats.done_wakes.load(Ordering::Relaxed), c.stats.done_lag_sum.load(Ordering::Relaxed));
        klog::event!(Info, "usb", "ktest.xhci_done_lag", ctrl = c.name(), wakes = n, avg_us = sum / n.max(1) / 1000,
            max_us = c.stats.done_lag_max.load(Ordering::Relaxed) / 1000);
    }
    kassert!(hal::vectors::pollers().any(|p| p == "xhci"), "poller registered");
    klog::event!(Info, "usb", "ktest.controllers", running = list.len());
    Ok(())
}
ktest!("us.xhci.running", Safe, running_all);

fn noop() -> TestResult {
    let Some(list) = running()? else { return ktest::skip("no xHCI controller in this machine") };
    for c in &list {
        let before = c.stats.cmd_completions.load(Ordering::Relaxed);
        let r = c.noop().map_err(|e| format!("{}: No-Op: {e:?}", c.name()))?;
        kassert!(r.code == cc::SUCCESS);
        kassert!(c.stats.cmd_completions.load(Ordering::Relaxed) > before, "{}: completion not counted", c.name());
    }
    kassert!(klog::metrics::snapshot().any(|(n, v)| n == "usb.xhci.commands" && v > 0), "commands counter");
    Ok(())
}
ktest!("us.xhci.noop", Safe, noop);

/// The event thread (not the waiter's fallback) handles the completions when the scheduler runs it.
fn event_thread() -> TestResult {
    let Some(list) = running()? else { return ktest::skip("no xHCI controller in this machine") };
    let c = &list[0];
    let before = c.stats.by_thread.load(Ordering::Relaxed);
    for _ in 0..8 {
        c.noop().map_err(|e| format!("No-Op: {e:?}"))?;
    }
    let after = c.stats.by_thread.load(Ordering::Relaxed);
    kassert!(after > before, "the event thread handled no event ({} by waiters)", c.stats.by_waiter.load(Ordering::Relaxed));
    Ok(())
}
ktest!("us.xhci.event_thread", Safe, event_thread);

/// Enough No-Ops to drive the command ring past its Link TRB twice and the event ring around at least once.
fn wrap() -> TestResult {
    let Some(list) = running()? else { return ktest::skip("no xHCI controller in this machine") };
    let c = &list[0];
    let (cw0, ew0, _) = c.ring_info();
    let (t0, w0) = (c.stats.by_thread.load(Ordering::Relaxed), c.stats.by_waiter.load(Ordering::Relaxed));
    let n = 2 * crate::hc::SEGMENT_BYTES / 16 * 2 + 64;
    for i in 0..n {
        c.noop().map_err(|e| format!("No-Op {i} of {n}: {e:?}"))?;
    }
    let (cw1, ew1, segs) = c.ring_info();
    kassert!(cw1 >= cw0 + 2, "command ring wraps {cw0} -> {cw1}");
    kassert!(ew1 > ew0, "event ring ({segs} segments) wraps {ew0} -> {ew1}");
    kassert!(c.stats.ring_full.load(Ordering::Relaxed) == 0, "event ring ran full");
    kassert!(c.state() == State::Running && c.usbsts() & sts::HSE == 0);
    let (t1, w1) = (c.stats.by_thread.load(Ordering::Relaxed), c.stats.by_waiter.load(Ordering::Relaxed));
    klog::event!(Info, "usb", "ktest.wrap", ctrl = c.name(), commands = n, cmd_wraps = cw1 - cw0, event_wraps = ew1 - ew0,
        by_thread = t1 - t0, by_waiter = w1 - w0);
    kassert!(t1 - t0 >= n as u64 / 2, "the event thread handled only {} of {n} completions", t1 - t0);
    Ok(())
}
ktest!("us.xhci.wrap", Safe, wrap);

/// Quiesce halts the controller and clears bus master; commands are refused; restart brings it back.
fn quiesce() -> TestResult {
    let Some(list) = running()? else { return ktest::skip("no xHCI controller in this machine") };
    let c = &list[0];
    kassert!(devmodel::quiesce_matching(c.name()) == 1, "registered once under {}", c.name());
    kassert!(c.state() == State::Quiesced);
    let (uc, us) = (c.usbcmd(), c.usbsts());
    kassert!(uc & cmd::RS == 0, "USBCMD {uc:#x} still runs");
    kassert!(us & sts::HCH != 0, "USBSTS {us:#x} not halted");
    kassert!(c.env().pci().command() & 0x4 == 0, "bus master still on");
    kassert!(c.noop() == Err(CmdError::NotRunning), "command accepted while quiesced");
    kassert!(!c.event_pending());
    c.restart("ktest after quiesce")?;
    kassert!(c.state() == State::Running);
    kassert!(c.noop().is_ok(), "No-Op after restart");
    kassert!(c.env().pci().command() & 0x4 != 0, "bus master back on");
    Ok(())
}
ktest!("us.xhci.quiesce", Safe, quiesce);
