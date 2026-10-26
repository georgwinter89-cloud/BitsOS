//! PC-4 ktests. The `Safe` ones read, or call paths that must refuse before any access (a write to a
//! read-only ID register of a disk controller), or rescan (reads; it sizes only functions that are new);
//! they run on the Mini-PC. The `QemuOnly` ones bind their own test drivers to q35's ICH9 LPC bridge through
//! `bind::probe_with`: those drivers never enter the registry, so real hardware never sees them.

use crate::bind::{self, BindError, BindState};
use crate::registry::{self, Candidate};
use crate::sysfs::{self, SysKind};
use crate::{Loc, Match, Node, PciFunction};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("pc.optin.command_unchanged", Safe, optin_command_unchanged);
ktest!("pc.optin.net_drivers_off", Safe, net_drivers_off);
ktest!("pc.bind.one_per_function", Safe, one_per_function);
ktest!("pc.bind.order", QemuOnly, bind_order);
ktest!("pc.bind.remove_reprobe", QemuOnly, remove_reprobe);
ktest!("pc.bind.busy", QemuOnly, busy_refusals);
ktest!("pc.bind.gone_during_probe", QemuOnly, gone_during_probe);
ktest!("pc.rescan.keeps_bound", Safe, rescan_keeps_bound);
ktest!("pc.storage.raw_write_refused", Safe, storage_raw_write_refused);
ktest!("pc.sysfs.view", Safe, sysfs_view);

/// q35's ICH9 LPC bridge (00:1f.0, class 06 01 00): no BitsOS driver matches it.
const ICH9_LPC: (u16, u16) = (0x8086, 0x2918);

fn events(seq: u64, name: &str) -> Vec<String> {
    klog::read_since(seq, usize::MAX).into_iter().filter(|r| r.subsystem == "pci" && r.name == name).map(|r| r.text).collect()
}

/// R8d after boot: every function left untouched by its key (on the Mini-PC 04:00.0 and its bridge
/// 00:02.2, under QEMU the ICH9 SMBus and the q35 MCH with test keys) still has the command register the
/// firmware left, no BARs sized, no driver, no record, and is refused by `probe`.
fn optin_command_unchanged() -> R {
    let nodes = crate::tree();
    let opted_out: Vec<&Node> = nodes.iter().filter(|n| n.untouched.is_some()).collect();
    if opted_out.is_empty() {
        return ktest::skip("no function is untouched on this machine (no WLAN, no test key)");
    }
    for n in opted_out {
        let f = n.function();
        let now = f.command();
        kassert!(now == n.command_at_scan, "{} command {:#06x} at scan, {:#06x} after boot", n.loc, n.command_at_scan, now);
        kassert!(n.bars.is_none(), "{} was sized", n.loc);
        kassert!(bind::binding(n.loc).is_none(), "{} has a driver", n.loc);
        kassert!(!crate::device::has_record(n.loc), "{} has a record", n.loc);
        kassert!(!crate::dma_enabled().contains(&n.loc), "{} enabled for DMA", n.loc);
        let r = crate::probe(n.loc);
        kassert!(matches!(r, Err(BindError::Untouched(_)) | Err(BindError::NoDriver)), "{}: probe gave {:?}", n.loc, r);
        kassert!(f.command() == n.command_at_scan, "{} command changed by probe", n.loc);
        klog::event!(Info, "pci", "ktest.untouched", function = format!("{}", n.loc), key = n.untouched.unwrap_or("-"),
            command = klog::Hex(now as u64));
    }
    Ok(())
}

/// BT-5 safe mode: with `net_drivers=off` every network-class function (QEMU: the virtio-net card) is
/// untouched by that key, keeps the command register the firmware left and has no driver. Without the key
/// it skips; run it with `cargo xtask ktest pc.optin --config-extra net_drivers=off`.
fn net_drivers_off() -> R {
    if !registry::net_kept_off(0x02_00_00, registry::config_value("net_drivers").as_deref()) {
        return ktest::skip("net_drivers is not off in this boot's CONFIG");
    }
    let nodes = crate::tree();
    let nets: Vec<&Node> = nodes.iter().filter(|n| n.class >> 16 == 0x02).collect();
    if nets.is_empty() {
        return ktest::skip("no network-class function on this machine");
    }
    for n in nets {
        kassert!(n.untouched == Some("net_drivers") || n.untouched == Some("wlan"), "{} untouched by {:?}", n.loc, n.untouched);
        kassert!(n.function().command() == n.command_at_scan, "{} command changed", n.loc);
        kassert!(n.bars.is_none(), "{} was sized", n.loc);
        kassert!(bind::binding(n.loc).is_none(), "{} has a driver", n.loc);
    }
    Ok(())
}

/// Every eligible driver ranked before `b.driver` for this node must be in `failed`.
fn check_order(n: &Node, b: &crate::Binding, cands: &[Candidate]) -> R {
    let order = registry::ordered(cands, n.vendor, n.device, n.class);
    let pos = order.iter().position(|(c, _)| c.name == b.driver).ok_or_else(|| format!("{}: bound driver {} does not match it", n.loc, b.driver))?;
    for (c, _) in &order[..pos] {
        let eligible = c.opt_in_key.is_none_or(registry::opted_in) && crate::optin::may_bind(c.name, n);
        kassert!(!eligible || b.failed.contains(&c.name), "{}: {} ranks before {} and did not fail", n.loc, c.name, b.driver);
    }
    kassert!(b.by_id == (order[pos].1.0 == 0), "{}: by_id wrong", n.loc);
    Ok(())
}

/// One driver per function: each binding names a registered driver that matches the function, is the first
/// eligible one in the defined order (or every one before it failed its probe), mass storage is bound only to
/// `nvme`, untouched functions to none; hw_info's tree text names the driver.
fn one_per_function() -> R {
    let all = crate::bindings();
    if all.is_empty() {
        return ktest::skip("no driver bound on this machine");
    }
    let cands = registry::registered();
    kassert!(registry::duplicates(&cands).is_empty(), "duplicate driver names: {:?}", registry::duplicates(&cands));
    let nodes = crate::tree();
    let lines = crate::tree_lines();
    for b in &all {
        let n = nodes.iter().find(|n| n.loc == b.function).ok_or_else(|| format!("binding for {} which is not in the tree", b.function))?;
        kassert!(b.state == BindState::Bound, "{} is {:?}", n.loc, b.state);
        kassert!(n.untouched.is_none(), "{} is untouched but bound to {}", n.loc, b.driver);
        kassert!(!n.is_storage() || b.driver == crate::optin::STORAGE_DRIVER, "{} (storage) bound to {}", n.loc, b.driver);
        check_order(n, b, &cands)?;
        let want = format!("{} [{:04x}:{:04x}]", n.loc, n.vendor, n.device);
        let line = lines.iter().find(|l| l.trim_start().starts_with(&want)).ok_or("function missing in the tree text")?;
        kassert!(line.contains(&format!("driver {}", b.driver)), "tree line without its driver: {}", line);
        klog::event!(Info, "pci", "ktest.bound", function = format!("{}", n.loc), driver = b.driver, by_id = b.by_id);
    }
    let mut locs: Vec<Loc> = all.iter().map(|b| b.function).collect();
    locs.dedup();
    kassert!(locs.len() == all.len(), "a function bound twice");
    Ok(())
}

static ID_PROBES: AtomicU32 = AtomicU32::new(0);
static ID_REMOVES: AtomicU32 = AtomicU32::new(0);
static ID_FAILS: AtomicBool = AtomicBool::new(false);
static NARROW_PROBES: AtomicU32 = AtomicU32::new(0);
static NARROW_REMOVES: AtomicU32 = AtomicU32::new(0);
static WIDE_PROBES: AtomicU32 = AtomicU32::new(0);

fn id_probe(f: &PciFunction) -> Result<(), String> {
    ID_PROBES.fetch_add(1, Ordering::SeqCst);
    if ID_FAILS.load(Ordering::SeqCst) {
        return Err("ktest: told to fail".into());
    }
    f.set_remove(|_| {
        ID_REMOVES.fetch_add(1, Ordering::SeqCst);
    });
    Ok(())
}
fn narrow_probe(f: &PciFunction) -> Result<(), String> {
    NARROW_PROBES.fetch_add(1, Ordering::SeqCst);
    f.set_remove(|_| {
        NARROW_REMOVES.fetch_add(1, Ordering::SeqCst);
    });
    Ok(())
}
fn wide_probe(_: &PciFunction) -> Result<(), String> {
    WIDE_PROBES.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// The ICH9 LPC bridge if nothing is bound to it.
fn lpc() -> Option<Node> {
    crate::tree().into_iter().find(|n| (n.vendor, n.device) == ICH9_LPC && n.untouched.is_none() && bind::binding(n.loc).is_none())
}

fn reset() {
    for c in [&ID_PROBES, &ID_REMOVES, &NARROW_PROBES, &NARROW_REMOVES, &WIDE_PROBES] {
        c.store(0, Ordering::SeqCst);
    }
    ID_FAILS.store(false, Ordering::SeqCst);
}

/// Exact ID before class, a narrow class before a wide one, whatever order the list has; one driver only;
/// a second probe is refused without calling any driver; a failed probe hands over to the next driver.
fn bind_order() -> R {
    let Some(n) = lpc() else { return ktest::skip("no free ICH9 LPC bridge (QEMU q35 only)") };
    reset();
    let cands = [
        Candidate { name: "ktest-wide", matches: &[Match::Class { class: 0x06_00_00, mask: 0xFF_00_00 }], opt_in_key: None, probe: wide_probe },
        Candidate { name: "ktest-narrow", matches: &[Match::Class { class: 0x06_01_00, mask: 0xFF_FF_FF }], opt_in_key: None, probe: narrow_probe },
        Candidate { name: "ktest-id", matches: &[Match::Id { vendor: ICH9_LPC.0, device: ICH9_LPC.1 }], opt_in_key: None, probe: id_probe },
    ];
    let at = n.loc;
    let r = bind::probe_with(at, &cands);
    let res = (|| -> R {
        kassert!(r == Ok("ktest-id"), "first probe gave {:?}", r);
        kassert!(ID_PROBES.load(Ordering::SeqCst) == 1 && NARROW_PROBES.load(Ordering::SeqCst) == 0 && WIDE_PROBES.load(Ordering::SeqCst) == 0,
            "probes id/narrow/wide = {}/{}/{}", ID_PROBES.load(Ordering::SeqCst), NARROW_PROBES.load(Ordering::SeqCst), WIDE_PROBES.load(Ordering::SeqCst));
        let b = bind::binding(at).ok_or("no binding")?;
        kassert!(b.driver == "ktest-id" && b.by_id && b.state == BindState::Bound && b.failed.is_empty() && b.has_remove, "{:?}", b);
        kassert!(n.function().driver() == Some("ktest-id"));
        // One driver per function: nobody else is asked while it is bound.
        let again = bind::probe_with(at, &cands);
        kassert!(again == Err(BindError::AlreadyBound("ktest-id")), "second probe gave {:?}", again);
        kassert!(crate::probe(at) == Err(BindError::AlreadyBound("ktest-id")));
        kassert!(ID_PROBES.load(Ordering::SeqCst) == 1, "a bound function was probed again");
        kassert!(crate::remove(at) == Ok("ktest-id"));
        kassert!(ID_REMOVES.load(Ordering::SeqCst) == 1, "remove hook ran {} times", ID_REMOVES.load(Ordering::SeqCst));
        kassert!(bind::binding(at).is_none() && !crate::device::has_record(at), "record not freed on remove");
        // The ID driver fails: the narrow class driver gets it, the wide one is never asked.
        ID_FAILS.store(true, Ordering::SeqCst);
        let r = bind::probe_with(at, &cands);
        kassert!(r == Ok("ktest-narrow"), "after a failed probe: {:?}", r);
        let b = bind::binding(at).ok_or("no binding")?;
        kassert!(!b.by_id && b.failed == ["ktest-id"], "{:?}", b);
        kassert!(WIDE_PROBES.load(Ordering::SeqCst) == 0, "the wide class driver was asked");
        kassert!(crate::remove(at) == Ok("ktest-narrow") && NARROW_REMOVES.load(Ordering::SeqCst) == 1);
        kassert!(crate::remove(at) == Err(BindError::NotBound));
        Ok(())
    })();
    // Leave the LPC as found whatever happened.
    if bind::binding(at).is_some() {
        bind::remove_gone(&n.function());
    }
    reset();
    res
}

static EN_REMOVES: AtomicU32 = AtomicU32::new(0);

fn enabling_probe(f: &PciFunction) -> Result<(), String> {
    f.enable_device(false).map_err(|e| format!("{e}"))?;
    f.set_remove(|f| {
        EN_REMOVES.fetch_add(1, Ordering::SeqCst);
        kassert_quiet(f.driver().is_none());
    });
    Ok(())
}
/// Inside a remove hook the binding is `Removing`, so `driver()` (bound only) says none.
fn kassert_quiet(ok: bool) {
    if !ok {
        EN_REMOVES.store(1000, Ordering::SeqCst);
    }
}
fn nohook_probe(_: &PciFunction) -> Result<(), String> {
    Ok(())
}

/// Probe enables the device and sets a remove hook; remove runs the hook once, switches the device off and
/// frees the record; the function can be probed again (a fresh record); a driver without a remove hook
/// cannot be removed.
fn remove_reprobe() -> R {
    let Some(n) = lpc() else { return ktest::skip("no free ICH9 LPC bridge (QEMU q35 only)") };
    let at = n.loc;
    let f = n.function();
    let orig = f.command();
    EN_REMOVES.store(0, Ordering::SeqCst);
    let en = [Candidate { name: "ktest-enable", matches: &[Match::Id { vendor: ICH9_LPC.0, device: ICH9_LPC.1 }], opt_in_key: None, probe: enabling_probe }];
    let nohook = [Candidate { name: "ktest-nohook", matches: &[Match::Id { vendor: ICH9_LPC.0, device: ICH9_LPC.1 }], opt_in_key: None, probe: nohook_probe }];
    let res = (|| -> R {
        for round in 0..3 {
            let seq = klog::next_seq();
            kassert!(bind::probe_with(at, &en) == Ok("ktest-enable"), "round {round}: probe");
            kassert!(crate::device::is_enabled(at) && crate::device::has_record(at), "round {round}: not enabled");
            kassert!(crate::remove(at) == Ok("ktest-enable"), "round {round}: remove");
            kassert!(EN_REMOVES.load(Ordering::SeqCst) == round + 1, "round {round}: hook ran {} times", EN_REMOVES.load(Ordering::SeqCst));
            kassert!(!crate::device::has_record(at) && !crate::device::is_enabled(at), "round {round}: record kept after remove");
            kassert!(f.command() & crate::enable::CMD_MASTER == 0, "round {round}: bus master on after remove");
            kassert!(events(seq, "removed").iter().any(|t| t.contains("driver=ktest-enable")), "round {round}: no removed event");
        }
        kassert!(bind::probe_with(at, &nohook) == Ok("ktest-nohook"));
        let r = crate::remove(at);
        kassert!(r == Err(BindError::NoRemove("ktest-nohook")), "remove without hook gave {:?}", r);
        kassert!(bind::binding(at).is_some_and(|b| b.state == BindState::Bound), "refused remove changed the binding");
        Ok(())
    })();
    if bind::binding(at).is_some() {
        bind::remove_gone(&f);
    }
    f.set_command(orig);
    res
}

/// What a nested call inside a probe or a remove hook answered: 0 not called, 1 `Busy`, 2 anything else.
static IN_PROBE_REMOVE: AtomicU32 = AtomicU32::new(0);
static IN_PROBE_PROBE: AtomicU32 = AtomicU32::new(0);
static IN_HOOK_REMOVE: AtomicU32 = AtomicU32::new(0);
static IN_HOOK_ENABLE: AtomicU32 = AtomicU32::new(0);
static BUSY_HOOKS: AtomicU32 = AtomicU32::new(0);

fn code<T, E: PartialEq>(r: Result<T, E>, busy: E) -> u32 {
    match r {
        Err(e) if e == busy => 1,
        _ => 2,
    }
}

/// Calls `remove` and `probe` on its own function while it is probing, and `remove` and `enable_device`
/// from its remove hook: each must be refused at once with `Busy` (B4: never a wait on itself).
fn busy_probe(f: &PciFunction) -> Result<(), String> {
    IN_PROBE_REMOVE.store(code(crate::remove(f.loc()), BindError::Busy), Ordering::SeqCst);
    IN_PROBE_PROBE.store(code(crate::probe(f.loc()), BindError::Busy), Ordering::SeqCst);
    f.set_remove(|f| {
        BUSY_HOOKS.fetch_add(1, Ordering::SeqCst);
        IN_HOOK_REMOVE.store(code(crate::remove(f.loc()), BindError::Busy), Ordering::SeqCst);
        IN_HOOK_ENABLE.store(code(f.enable_device(false).map(|_| ()), crate::EnableError::Busy), Ordering::SeqCst);
    });
    Ok(())
}

/// The `Busy` contract: `remove` and `probe` of a function whose probe runs, `remove` and `enable_device`
/// from inside its remove hook, and a second `rescan` while one runs are all refused at once.
fn busy_refusals() -> R {
    let Some(n) = lpc() else { return ktest::skip("no free ICH9 LPC bridge (QEMU q35 only)") };
    let at = n.loc;
    let f = n.function();
    let orig = f.command();
    for c in [&IN_PROBE_REMOVE, &IN_PROBE_PROBE, &IN_HOOK_REMOVE, &IN_HOOK_ENABLE, &BUSY_HOOKS] {
        c.store(0, Ordering::SeqCst);
    }
    let busy = [Candidate { name: "ktest-busy", matches: &[Match::Id { vendor: ICH9_LPC.0, device: ICH9_LPC.1 }], opt_in_key: None, probe: busy_probe }];
    let res = (|| -> R {
        kassert!(bind::probe_with(at, &busy) == Ok("ktest-busy"));
        kassert!(IN_PROBE_REMOVE.load(Ordering::SeqCst) == 1, "remove during probe: {}", IN_PROBE_REMOVE.load(Ordering::SeqCst));
        kassert!(IN_PROBE_PROBE.load(Ordering::SeqCst) == 1, "probe during probe: {}", IN_PROBE_PROBE.load(Ordering::SeqCst));
        // A rescan already running (the flag held here): the second caller is refused, nothing is walked.
        let seq = klog::next_seq();
        if crate::rescan_flag_try_hold() {
            let r = crate::rescan();
            crate::rescan_flag_release();
            kassert!(r == Err(BindError::Busy), "rescan while one runs gave {:?}", r);
            kassert!(events(seq, "rescan").is_empty(), "the refused rescan walked");
        }
        kassert!(crate::remove(at) == Ok("ktest-busy"));
        kassert!(BUSY_HOOKS.load(Ordering::SeqCst) == 1);
        kassert!(IN_HOOK_REMOVE.load(Ordering::SeqCst) == 1, "remove inside the hook: {}", IN_HOOK_REMOVE.load(Ordering::SeqCst));
        kassert!(IN_HOOK_ENABLE.load(Ordering::SeqCst) == 1, "enable inside the hook: {}", IN_HOOK_ENABLE.load(Ordering::SeqCst));
        kassert!(!crate::device::has_record(at), "record kept");
        Ok(())
    })();
    if bind::binding(at).is_some() {
        bind::remove_gone(&f);
    }
    f.set_command(orig);
    res
}

static GONE_ACTION: AtomicU32 = AtomicU32::new(0);
static GONE_HOOKS: AtomicU32 = AtomicU32::new(0);

/// A rescan finds the function gone while this probe runs (`remove_gone` called from inside the probe).
fn vanishing_probe(f: &PciFunction) -> Result<(), String> {
    f.set_remove(|_| {
        GONE_HOOKS.fetch_add(1, Ordering::SeqCst);
    });
    let a = bind::remove_gone(f);
    GONE_ACTION.store(if a == bind::GoneAction::Deferred { 1 } else { 2 }, Ordering::SeqCst);
    Ok(())
}

/// A function that vanishes during its own probe: the rescan's removal is deferred, and the probe's end
/// runs the hook and frees the record instead of binding a driver to a function that is not there.
fn gone_during_probe() -> R {
    let Some(n) = lpc() else { return ktest::skip("no free ICH9 LPC bridge (QEMU q35 only)") };
    let at = n.loc;
    let f = n.function();
    GONE_ACTION.store(0, Ordering::SeqCst);
    GONE_HOOKS.store(0, Ordering::SeqCst);
    let c = [Candidate { name: "ktest-vanish", matches: &[Match::Id { vendor: ICH9_LPC.0, device: ICH9_LPC.1 }], opt_in_key: None, probe: vanishing_probe }];
    let seq = klog::next_seq();
    let r = bind::probe_with(at, &c);
    let res = (|| -> R {
        kassert!(GONE_ACTION.load(Ordering::SeqCst) == 1, "remove_gone during probe was not deferred");
        kassert!(r == Err(BindError::NotScanned), "probe of a vanished function gave {:?}", r);
        kassert!(GONE_HOOKS.load(Ordering::SeqCst) == 1, "hook ran {} times", GONE_HOOKS.load(Ordering::SeqCst));
        kassert!(bind::binding(at).is_none() && !crate::device::has_record(at), "record kept");
        kassert!(!events(seq, "bound.gone").is_empty(), "no bound.gone event");
        Ok(())
    })();
    if bind::binding(at).is_some() {
        bind::remove_gone(&f);
    }
    res
}

/// A rescan finds the same tree and changes nothing: no function new or gone, no BAR sized, every node
/// (BARs, untouched key, command at scan) and every binding as before, the command registers of untouched
/// functions unchanged.
fn rescan_keeps_bound() -> R {
    let before = crate::tree();
    let bound_before = crate::bindings();
    // Untouched functions only: a bound driver may change its own command register meanwhile (a restart).
    let cmds: Vec<(Loc, u16)> = before.iter().filter(|n| n.untouched.is_some()).map(|n| (n.loc, n.function().command())).collect();
    let seq = klog::next_seq();
    let r = crate::rescan().map_err(|e| format!("rescan: {e}"))?;
    kassert!(r.added.is_empty() && r.gone.is_empty() && r.held.is_empty(), "rescan added {:?}, lost {:?}, held {:?}", r.added, r.gone, r.held);
    kassert!(r.sized == 0, "{} functions sized again", r.sized);
    kassert!(r.kept == before.len(), "kept {} of {}", r.kept, before.len());
    let after = crate::tree();
    kassert!(after.len() == before.len());
    for (a, b) in after.iter().zip(&before) {
        kassert!(a.loc == b.loc && a.bars == b.bars && a.untouched == b.untouched && a.command_at_scan == b.command_at_scan, "{} changed", a.loc);
        kassert!(a.parent == b.parent, "{} parent index changed", a.loc);
    }
    kassert!(crate::bindings() == bound_before, "bindings changed");
    for (at, c) in cmds {
        let now = crate::function(at).ok_or("function lost")?.command();
        kassert!(now == c, "{} command {:#06x} -> {:#06x}", at, c, now);
    }
    kassert!(events(seq, "bar").is_empty(), "BAR events during the rescan");
    kassert!(events(seq, "rescan").len() == 1);
    Ok(())
}

/// R8a on the raw paths: a write to a mass-storage function not bound to `nvme` is refused before it
/// reaches the device. The write goes to the read-only ID register, so even a failure of the check could
/// not change anything (the Mini-PC's NVMe disks); under QEMU `set_command` with the current value too.
fn storage_raw_write_refused() -> R {
    let disks: Vec<Node> = crate::tree()
        .into_iter()
        .filter(|n| n.is_storage() && n.untouched.is_none() && bind::bound_driver(n.loc) != Some(crate::optin::STORAGE_DRIVER))
        .collect();
    if disks.is_empty() {
        return ktest::skip("no mass-storage function without the NVMe driver");
    }
    for n in disks {
        let f = n.function();
        let id = f.read32(0);
        let seq = klog::next_seq();
        kassert!(!f.write32_ext(0, id), "{}: raw write went through", n.loc);
        f.write32(0, id);
        kassert!(f.read32(0) == id);
        if hal::cpu::detect().hypervisor {
            let cmd = f.command();
            f.set_command(cmd);
            kassert!(f.command() == cmd);
        }
        let refused = events(seq, "write.refused");
        kassert!(refused.len() >= 2 && refused.iter().all(|t| t.contains("reason=storage")), "{}: {:?}", n.loc, refused);
    }
    Ok(())
}

/// Resolves a relative link target against the directory the link sits in.
fn resolve(link_path: &str, target: &str) -> String {
    let mut parts: Vec<&str> = link_path.split('/').collect();
    parts.pop();
    for c in target.split('/') {
        match c {
            ".." => {
                parts.pop();
            }
            "" | "." => {}
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// The /sys/bus/pci view: a directory per function with Linux's ID files, a link per function in
/// bus/pci/devices, a driver link for each bound function, no config file for an untouched one, and the
/// config file reads the live ID register.
fn sysfs_view() -> R {
    let e = sysfs::entries();
    let get = |p: &str| e.iter().find(|x| x.path == p).map(|x| &x.kind);
    let nodes = crate::tree();
    for (i, n) in nodes.iter().enumerate() {
        let dir = sysfs::dir(&nodes, i);
        kassert!(get(&dir) == Some(&SysKind::Dir), "no directory {}", dir);
        kassert!(get(&format!("{dir}/vendor")) == Some(&SysKind::Text(format!("0x{:04x}\n", n.vendor))), "{dir}/vendor");
        kassert!(get(&format!("{dir}/class")) == Some(&SysKind::Text(format!("0x{:06x}\n", n.class))), "{dir}/class");
        let link = format!("bus/pci/devices/{}", sysfs::name(n.loc));
        let Some(SysKind::Link(t)) = get(&link) else { return Err(format!("no link {link}")) };
        kassert!(resolve(&link, t) == dir, "{link} -> {t}");
        match get(&format!("{dir}/config")) {
            Some(SysKind::Config { loc, .. }) => {
                kassert!(n.untouched.is_none() && *loc == n.loc);
                let mut b = [0u8; 4];
                kassert!(sysfs::read_config(n.loc, 0, &mut b) == 4);
                kassert!(u32::from_le_bytes(b) == (n.vendor as u32 | (n.device as u32) << 16), "{}: config reads {:02x?}", n.loc, b);
            }
            None => kassert!(n.untouched.is_some(), "{} has no config file", n.loc),
            Some(k) => return Err(format!("{dir}/config is {k:?}")),
        }
        if n.untouched.is_some() {
            kassert!(sysfs::read_config(n.loc, 0, &mut [0u8; 4]) == 0, "untouched {} config readable", n.loc);
        }
        if let Some(d) = bind::bound_driver(n.loc) {
            let Some(SysKind::Link(t)) = get(&format!("{dir}/driver")) else { return Err(format!("{dir}/driver missing")) };
            kassert!(resolve(&format!("{dir}/driver"), t) == format!("bus/pci/drivers/{d}"), "{dir}/driver -> {t}");
            kassert!(matches!(get(&format!("bus/pci/drivers/{d}/{}", sysfs::name(n.loc))), Some(SysKind::Link(_))));
        }
    }
    Ok(())
}
