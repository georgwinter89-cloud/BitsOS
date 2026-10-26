//! PC-4: one driver per function (Linux `driver_attach`/`device_release_driver`). The drivers that match a
//! function are tried in the registry's defined order (`registry::ordered`: exact ID before class, then the
//! narrower class, then the name); the first whose probe succeeds owns the function until `remove`. A
//! failed probe is cleaned up (the device switched off if the probe enabled it) and the next driver tries,
//! as in Linux. Untouched functions (R8d) are never offered to any driver; mass storage only to `nvme`
//! (R8a).
//!
//! The binding lives in the per-function record of `device.rs` (`pci.state`, a leaf lock held only for
//! short bookkeeping, never across a probe or a remove hook). While a probe or remove runs, the record
//! says so (`Probing`/`Removing`), so a second probe, a remove or a rescan of the same function is refused
//! with `Busy` instead of waiting (B4). `remove` calls the driver's hook (`PciFunction::set_remove`, set
//! during probe), switches the device off (`disable_device`) and frees the whole record.
//!
//! While a record says `Removing`, `enable_device` is refused (`Busy`), and the final switch-off and the
//! freeing of the record run under `pci.enable`: an enable from a driver thread the hook did not stop either
//! finishes before the switch-off or is refused, so bus master never stays on behind a freed record. A
//! function a rescan finds gone while its probe still runs is marked `gone`; the probe's end then runs the
//! hook and frees the record instead of binding.

use crate::device::STATE;
use crate::registry::{self, Candidate};
use crate::{Loc, Node, PciFunction};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use ksync::Once;

/// What a driver's remove hook gets: the function it was bound to. Called once, with no PCI lock held.
pub type RemoveHook = Box<dyn FnOnce(&PciFunction) + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindState {
    Probing,
    Bound,
    Removing,
}

/// The binding part of a function's record.
pub(crate) struct BindRec {
    pub(crate) state: BindState,
    pub(crate) since_ns: u64,
    pub(crate) rank: (u8, u8),
    /// Drivers ranked earlier whose probe failed on this function (tried before the one bound).
    pub(crate) failed: Vec<&'static str>,
    pub(crate) remove: Option<RemoveHook>,
    /// A rescan found the function gone while its probe ran: the probe's end removes instead of binding.
    pub(crate) gone: bool,
}

/// A binding as callers see it (`bindings`, hw_info, sysfs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub function: Loc,
    pub driver: &'static str,
    pub state: BindState,
    /// Bound through an exact vendor/device match (else a class match).
    pub by_id: bool,
    pub since_ns: u64,
    pub failed: Vec<&'static str>,
    /// The driver set a remove hook (`remove` is refused without one).
    pub has_remove: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindError {
    /// No such function in the tree.
    NotScanned,
    /// Left untouched until this CONFIG key is on (R8d): no driver is offered it.
    Untouched(&'static str),
    AlreadyBound(&'static str),
    /// A probe, remove or rescan is running for it.
    Busy,
    /// No driver matches, every match was refused, or every probe failed.
    NoDriver,
    NotBound,
    /// The bound driver set no remove hook: it cannot let go of the device.
    NoRemove(&'static str),
}

impl core::fmt::Display for BindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::NotScanned => write!(f, "no such function in the PCI tree"),
            BindError::Untouched(k) => write!(f, "untouched {} (R8d)", crate::registry::Until(k)),
            BindError::AlreadyBound(d) => write!(f, "already bound to {d}"),
            BindError::Busy => write!(f, "a probe, remove or rescan is running"),
            BindError::NoDriver => write!(f, "no driver took the function"),
            BindError::NotBound => write!(f, "no driver bound"),
            BindError::NoRemove(d) => write!(f, "driver {d} has no remove hook"),
        }
    }
}

fn view(at: Loc, driver: Option<&'static str>, b: &BindRec) -> Binding {
    Binding {
        function: at,
        driver: driver.unwrap_or("-"),
        state: b.state,
        by_id: b.rank.0 == 0,
        since_ns: b.since_ns,
        failed: b.failed.clone(),
        has_remove: b.remove.is_some(),
    }
}

/// The binding of one function, if a driver is probing, bound or being removed.
pub fn binding(at: Loc) -> Option<Binding> {
    let st = STATE.lock();
    let s = st.get(&at)?;
    s.bind.as_ref().map(|b| view(at, s.driver, b))
}

/// Every binding, in function order.
pub fn bindings() -> Vec<Binding> {
    STATE.lock().iter().filter_map(|(at, s)| s.bind.as_ref().map(|b| view(*at, s.driver, b))).collect()
}

/// The driver bound to a function (state `Bound` only).
pub fn bound_driver(at: Loc) -> Option<&'static str> {
    let st = STATE.lock();
    let s = st.get(&at)?;
    s.bind.as_ref().filter(|b| b.state == BindState::Bound).and(s.driver)
}

/// Marks the function as being probed by `name`. Refused when another binding exists.
fn claim(at: Loc, name: &'static str, rank: (u8, u8)) -> Result<(), BindError> {
    let mut st = STATE.lock();
    let s = st.entry(at).or_default();
    if let Some(b) = &s.bind {
        return Err(if b.state == BindState::Bound { BindError::AlreadyBound(s.driver.unwrap_or("-")) } else { BindError::Busy });
    }
    s.driver = Some(name);
    s.bind = Some(BindRec { state: BindState::Probing, since_ns: time::now_ns(), rank, failed: Vec::new(), remove: None, gone: false });
    Ok(())
}

/// The node is still in the tree (a rescan may have dropped it between our copy and the claim).
fn in_tree(n: &Node) -> bool {
    crate::NODES.lock().iter().any(|t| crate::rescan::same(t, n))
}

/// After a failed probe: switch the device off if the probe had enabled it, then free the record (the
/// hook the probe may have set is dropped unused, as Linux never calls remove after a failed probe).
fn release_failed(f: &PciFunction) {
    let at = f.loc();
    let gone = {
        let mut st = STATE.lock();
        let b = st.get_mut(&at).and_then(|s| s.bind.as_mut());
        b.map(|b| {
            // Refuses late enables from the failed driver's threads from here on.
            b.state = BindState::Removing;
            b.gone
        })
        .unwrap_or(false)
    };
    let fname = format!("{at}");
    let serial = crate::device::lock_enable(&fname);
    let enabled = STATE.lock().get(&at).is_some_and(|s| s.enabled || s.dma);
    // Nothing is written to a function that is gone.
    if enabled && !gone {
        f.disable_device_logged();
    }
    // Dropped after the lock: a hook's captures may run arbitrary drop code.
    let rec = STATE.lock().remove(&at);
    drop(serial);
    drop(rec);
}

static DUPLICATES_REPORTED: Once<()> = Once::new();

fn report_duplicates(cands: &[Candidate]) {
    DUPLICATES_REPORTED.get_or_init(|| {
        for d in registry::duplicates(cands) {
            klog::event!(Error, "pci", "driver.duplicate", driver = d; "two pci_driver! entries share a name: only the first is ever tried");
        }
    });
}

/// Tries the matching drivers of one function in the defined order until a probe succeeds.
pub(crate) fn bind_node(n: &Node, cands: &[Candidate]) -> Result<&'static str, BindError> {
    if let Some(b) = binding(n.loc) {
        return Err(if b.state == BindState::Bound { BindError::AlreadyBound(b.driver) } else { BindError::Busy });
    }
    let order = registry::ordered(cands, n.vendor, n.device, n.class);
    if order.is_empty() {
        return Err(BindError::NoDriver);
    }
    let loc = format!("{}", n.loc);
    if let Some(k) = n.untouched {
        for (c, _) in &order {
            klog::event!(Info, "pci", "opt_in.skip", driver = c.name, function = loc.as_str(), key = k);
        }
        return Err(BindError::Untouched(k));
    }
    let f = n.function();
    let mut failed: Vec<&'static str> = Vec::new();
    for (c, rank) in order {
        // The function itself is touched; an opt-in driver whose key is off still never probes (R8d).
        if let Some(k) = c.opt_in_key.filter(|k| !registry::opted_in(k)) {
            klog::event!(Info, "pci", "opt_in.skip", driver = c.name, function = loc.as_str(), key = k);
            continue;
        }
        if !crate::optin::may_bind(c.name, n) {
            klog::event!(Warn, "pci", "bind.refused", driver = c.name, function = loc.as_str(), class = klog::Hex(n.class as u64);
                "mass storage binds only to the NVMe driver (R8a)");
            continue;
        }
        claim(n.loc, c.name, rank)?;
        if !in_tree(n) {
            // A rescan dropped the function before the claim: nothing to probe.
            let rec = STATE.lock().remove(&n.loc);
            drop(rec);
            return Err(BindError::NotScanned);
        }
        let how = if rank.0 == 0 { "id" } else { "class" };
        match (c.probe)(&f) {
            Ok(()) => {
                let gone = match STATE.lock().get_mut(&n.loc).and_then(|s| s.bind.as_mut()) {
                    Some(b) => {
                        b.state = BindState::Bound;
                        b.failed = failed.clone();
                        b.gone
                    }
                    None => false,
                };
                if gone {
                    klog::event!(Warn, "pci", "bound.gone", driver = c.name, function = loc.as_str();
                        "the function vanished during its probe: removing the driver again");
                    let _ = remove_function(&f, true);
                    return Err(BindError::NotScanned);
                }
                klog::event!(Info, "pci", "bound", driver = c.name, function = loc.as_str(), matched = how, failed_before = failed.len());
                return Ok(c.name);
            }
            Err(e) => {
                klog::event!(Warn, "pci", "probe.failed", driver = c.name, function = loc.as_str(), matched = how, error = e);
                release_failed(&f);
                failed.push(c.name);
            }
        }
    }
    if !failed.is_empty() {
        klog::event!(Warn, "pci", "bind.none", function = loc.as_str(), failed = failed.join(","); "every matching driver's probe failed");
    }
    Err(BindError::NoDriver)
}

fn node(at: Loc) -> Option<Node> {
    crate::NODES.lock().iter().find(|n| n.loc == at).cloned()
}

/// Binds the registered drivers to every function that has none yet (the kernel calls it once after
/// `scan`; again later only binds what is still free). Returns (bound now, functions skipped as untouched).
pub fn bind_drivers() -> (usize, usize) {
    let cands = registry::registered();
    report_duplicates(&cands);
    let (mut bound, mut skipped) = (0, 0);
    for n in crate::tree() {
        match bind_node(&n, &cands) {
            Ok(_) => bound += 1,
            Err(BindError::Untouched(_)) => skipped += 1,
            Err(_) => {}
        }
    }
    (bound, skipped)
}

/// Offers one function to the registered drivers (after a `remove`, or for a function a rescan found).
pub fn probe(at: Loc) -> Result<&'static str, BindError> {
    let n = node(at).ok_or(BindError::NotScanned)?;
    let cands = registry::registered();
    report_duplicates(&cands);
    bind_node(&n, &cands)
}

/// `probe` with a driver list that is not the registry (ktests: their drivers must never reach real
/// hardware through `DRIVERS`).
pub(crate) fn probe_with(at: Loc, cands: &[Candidate]) -> Result<&'static str, BindError> {
    let n = node(at).ok_or(BindError::NotScanned)?;
    bind_node(&n, cands)
}

/// Unbinds the driver: its remove hook runs (no PCI lock held), then the device is switched off
/// (`disable_device`: MSI/MSI-X and bus master off) and the function's record is freed (binding, enable
/// state, saved header). Refused while a probe or remove runs (`Busy`), when nothing is bound, and when the
/// driver set no remove hook (it would keep using a device it no longer owns). `probe` binds it again.
pub fn remove(at: Loc) -> Result<&'static str, BindError> {
    let n = node(at).ok_or(BindError::NotScanned)?;
    remove_function(&n.function(), false)
}

/// What `remove_gone` did with a function's record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoneAction {
    /// No driver: the record (if any) was freed.
    Freed,
    /// A driver was bound: its hook ran and the record was freed.
    Removed,
    /// A probe or remove is still running: it frees the record when it ends.
    Deferred,
}

/// For a function a rescan no longer finds (call it after the node left the tree): the hook runs if a
/// driver is bound, nothing is written to the (absent) device, and the record is freed. While a probe or
/// a remove is running the record is marked gone and that path finishes the removal.
pub(crate) fn remove_gone(f: &PciFunction) -> GoneAction {
    let at = f.loc();
    let rec = {
        let mut st = STATE.lock();
        match st.get_mut(&at).map(|s| s.bind.as_mut()) {
            None => return GoneAction::Freed,
            Some(None) => st.remove(&at),
            Some(Some(b)) if b.state != BindState::Bound => {
                b.gone = true;
                let fname = format!("{at}");
                klog::event!(Warn, "pci", "remove.deferred", function = fname.as_str(), state = format!("{:?}", b.state).as_str();
                    "the function is gone while its probe or remove runs: that path frees the record");
                return GoneAction::Deferred;
            }
            Some(Some(_)) => None,
        }
    };
    if let Some(r) = rec {
        drop(r);
        return GoneAction::Freed;
    }
    match remove_function(f, true) {
        Ok(_) => GoneAction::Removed,
        // Became Removing in between (a concurrent `remove`): that path frees the record.
        Err(_) => GoneAction::Deferred,
    }
}

fn remove_function(f: &PciFunction, gone: bool) -> Result<&'static str, BindError> {
    let at = f.loc();
    let fname = format!("{at}");
    let taken = {
        let mut st = STATE.lock();
        match st.get_mut(&at) {
            None => Err(BindError::NotBound),
            Some(s) => {
                let driver = s.driver.unwrap_or("-");
                match s.bind.as_mut() {
                    None => Err(BindError::NotBound),
                    Some(b) if b.state != BindState::Bound => Err(BindError::Busy),
                    Some(b) if b.remove.is_none() && !gone => Err(BindError::NoRemove(driver)),
                    Some(b) => {
                        b.state = BindState::Removing;
                        Ok((driver, b.remove.take()))
                    }
                }
            }
        }
    };
    let (driver, hook) = match taken {
        Ok(t) => t,
        Err(e) => {
            if !matches!(e, BindError::NotBound) {
                let reason = format!("{e}");
                klog::event!(Warn, "pci", "remove.refused", function = fname.as_str(), reason = reason.as_str());
            }
            return Err(e);
        }
    };
    klog::event!(Info, "pci", "remove", function = fname.as_str(), driver = driver, gone = gone);
    match hook {
        Some(h) => h(f),
        None => klog::event!(Warn, "pci", "remove.no_hook", function = fname.as_str(), driver = driver;
            "the function is gone and its driver has no remove hook: the driver may still hold state for it"),
    }
    // Under pci.enable: an enable that started before Removing finishes first and is switched off here; a
    // later one is refused (`enable_device` checks for Removing). Bounded wait (B4); on timeout go on.
    let serial = crate::device::lock_enable(&fname);
    if serial.is_err() {
        klog::event!(Warn, "pci", "remove.enable_busy", function = fname.as_str(); "pci.enable stayed busy: switching off without it");
    }
    let d = if gone { None } else { Some(f.disable_device()) };
    let rec = STATE.lock().remove(&at);
    drop(serial);
    drop(rec);
    let (refused, incomplete) = d.map_or((false, false), |d| (d.refused, d.incomplete));
    klog::event!(Info, "pci", "removed", function = fname.as_str(), driver = driver, gone = gone, disable_refused = refused,
        disable_incomplete = incomplete);
    Ok(driver)
}

impl PciFunction {
    /// The driver's remove hook for this function (call it during probe). `remove` runs it once, with no
    /// PCI lock held, before switching the device off; the driver stops its threads, interrupts and DMA
    /// there and drops what it keeps for the function. False when no driver is probing or bound here.
    pub fn set_remove(&self, hook: impl FnOnce(&PciFunction) + Send + 'static) -> bool {
        let old = {
            let mut st = STATE.lock();
            match st.get_mut(&self.loc()).and_then(|s| s.bind.as_mut()) {
                Some(b) if b.state != BindState::Removing => b.remove.replace(Box::new(hook)),
                _ => return false,
            }
        };
        drop(old);
        true
    }

    /// The driver bound to this function, if any (`Bound` state only).
    pub fn driver(&self) -> Option<&'static str> {
        bound_driver(self.loc())
    }
}

/// For hw_info's text: "driver <name>" or nothing.
pub(crate) fn driver_note(at: Loc) -> Option<String> {
    binding(at).map(|b| match b.state {
        BindState::Bound => format!("driver {}", b.driver),
        BindState::Probing => format!("driver {} (probing)", b.driver),
        BindState::Removing => format!("driver {} (removing)", b.driver),
    })
}
