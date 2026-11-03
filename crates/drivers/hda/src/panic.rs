//! SN-12: HDMI/analog sound stopped on the panic path (granted to RB's `kexec::crash::clean`, change request).
//! A panic resets with whatever streams: before, a panic during HDMI sound reset with the stream running and the
//! display's audio on, the state that preceded the reboot sync floods (SN-7). The same Quiesce as a reboot, but
//! without the registry lock (a CPU the panic stopped may hold `hda.devices` for good) and on the panic's clock.

use crate::Hda;
use crate::ctrl::{Kind, QuiesceResult};
use crate::hdmi::{self, QuiesceNote};
use alloc::sync::Arc;

/// One controller on the panic path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanicStop {
    pub ctrl: QuiesceResult,
    /// The GPU controller with this kernel's display writes in place: the undo's note (None: nothing to undo,
    /// or not the GPU controller).
    pub display: Option<QuiesceNote>,
}

/// `Controller::quiesce` (streams stopped, codecs' shutdown, chip stopped, bus master off) and for the GPU
/// controller the display side's undo, on every controller in probe order; `each` gets the name and the result;
/// returns how many. `now` is the panic path's TSC clock in ns. No lock wait, heap, klog or sleep. The locks
/// the codec verbs took are released again (the ktest brings the controllers up afterwards); [`PANIC_HOOK`], the
/// real panic path, keeps them instead (no release may wake a waiter through the scheduler).
pub fn panic_stop_all(now: &(dyn Fn() -> u64 + Sync), each: &mut dyn FnMut(&str, PanicStop)) -> usize {
    stop_each(now, false, each)
}

fn stop_each(now: &(dyn Fn() -> u64 + Sync), terminal: bool, each: &mut dyn FnMut(&str, PanicStop)) -> usize {
    PANIC_LIST.for_each(&mut |d| {
        let ctrl = d.ctrl.quiesce_with(Some(now), terminal);
        let mut display = None;
        if d.ctrl.kind() == Kind::AtiHdmi && hdmi::display_armed() {
            hdmi::quiesce_display(Some(now));
            display = hdmi::quiesce_note();
        }
        each(d.ctrl.name(), PanicStop { ctrl, display });
    })
}

/// The panic path's hook as RB's `kexec::crash::clean` would call it (change request, SN-12): `now` the panic
/// clock in ns, `line` one line per controller for the log and the panic page; returns how many. Only core types,
/// so kexec can hold it without depending on hda (hda -> amdgpu -> kexec already).
pub type PanicHook = fn(now: &(dyn Fn() -> u64 + Sync), line: &mut dyn FnMut(core::fmt::Arguments<'_>)) -> usize;

/// [`panic_stop_all`] as a [`PanicHook`], terminal (the verbs' locks are kept); the kernel installs `&PANIC_HOOK`
/// once RB's setter exists.
pub static PANIC_HOOK: PanicHook = panic_report;

fn panic_report(now: &(dyn Fn() -> u64 + Sync), line: &mut dyn FnMut(core::fmt::Arguments<'_>)) -> usize {
    stop_each(now, true, &mut |name, r| report_line(name, r, line))
}

/// One controller's panic line (no heap: `format_args!` straight into the sink).
pub(crate) fn report_line(name: &str, r: PanicStop, line: &mut dyn FnMut(core::fmt::Arguments<'_>)) {
    let (undo, writes, mismatched, pme) = match r.display {
        None => ("none", 0, 0, "none"),
        Some(n) if n.refused => (if n.pair_busy { "pair_busy" } else { "refused" }, n.writes, n.mismatched, n.pme_name()),
        Some(n) => ("done", n.writes, n.mismatched, n.pme_name()),
    };
    line(format_args!(
        "{name}: was={} stuck={} codec={} display_undo={undo} writes={writes} mismatched={mismatched} pme={pme}",
        r.ctrl.was.name(),
        r.ctrl.stuck,
        r.ctrl.codec.name()
    ));
}

/// Every probed controller, filled next to `DEVICES` (lock-free walk for the panic path).
pub(crate) static PANIC_LIST: PanicList<Arc<Hda>> = PanicList::new();

/// An append-only list walked without a lock (xHCI's RB-13 pattern, `xhci::hc::PanicList`).
pub(crate) struct PanicList<T: 'static> {
    head: ksync::OnceRef<PanicNode<T>>,
}

struct PanicNode<T: 'static> {
    item: T,
    next: ksync::OnceRef<PanicNode<T>>,
}

impl<T: Sync + 'static> PanicList<T> {
    pub(crate) const fn new() -> Self {
        PanicList { head: ksync::OnceRef::new() }
    }

    /// Appends `item` (probe order). The first `set` on the last link wins; a loser follows the winner's node; a
    /// link being set right now is waited for (its setter is inside two stores).
    pub(crate) fn add(&self, item: T) {
        let node: &'static PanicNode<T> = alloc::boxed::Box::leak(alloc::boxed::Box::new(PanicNode { item, next: ksync::OnceRef::new() }));
        let mut link = &self.head;
        loop {
            if link.set(node) {
                return;
            }
            match link.get() {
                Some(n) => link = &n.next,
                None => core::hint::spin_loop(),
            }
        }
    }

    /// `f` on every item in order; returns how many. No lock, no heap: a link still being set ends the walk.
    pub(crate) fn for_each(&self, f: &mut dyn FnMut(&'static T)) -> usize {
        let mut n = 0;
        let mut cur = self.head.get();
        while let Some(node) = cur {
            f(&node.item);
            n += 1;
            cur = node.next.get();
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::PanicList;
    use alloc::vec::Vec;

    #[test]
    fn panic_list_keeps_probe_order() {
        static L: PanicList<u32> = PanicList::new();
        assert_eq!(L.for_each(&mut |_| {}), 0);
        for i in 0..5 {
            L.add(i);
        }
        let mut seen = Vec::new();
        assert_eq!(L.for_each(&mut |&v| seen.push(v)), 5);
        assert_eq!(seen, [0, 1, 2, 3, 4]);
    }
}
