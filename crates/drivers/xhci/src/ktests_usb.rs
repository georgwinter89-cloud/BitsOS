//! In-kernel tests of US-2: what the hotplug workers enumerated at boot (every connected root port, QEMU's boot
//! stick), the topology as the controller keeps it (route strings, TT, hub slots), both rings driven past their
//! Link TRBs, a controller restart dropping and finding every device again, and unplugging a keyboard while a
//! key is held (with `host/tests/us/30-unplug.sh`, which plugs and pulls through QMP).

use crate::driver::{controllers, Xhci};
use crate::hc::{State, MS};
use crate::ktests::running;
use crate::port::decode;
use crate::slot::slot_state;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use ktest::{kassert, ktest, TestResult};
use usb_core::{hub, registry, Device, Interface, PipeEvent, Setup, UsbDriver, UsbMatch};

const SETTLE_NS: u64 = 15_000 * MS;
const QEMU_STICK: (u16, u16) = (0x46F4, 0x0001);
const QEMU_XHCI: u16 = 0x1B36;

fn wait_settled(c: &Xhci, ns: u64) -> bool {
    let deadline = time::now_ns().saturating_add(ns);
    while time::now_ns() < deadline {
        if c.settled() {
            return true;
        }
        sched::sleep_ns(20 * MS);
    }
    c.settled()
}

fn devices_of(c: &Xhci) -> Vec<Arc<Device>> {
    let mut v: Vec<_> = registry::devices().into_iter().filter(|d| d.host_name() == c.name()).collect();
    v.sort_by_key(|d| (d.place.root_port, d.place.route));
    v
}

fn settled_list() -> Result<Option<Vec<Arc<Xhci>>>, String> {
    let Some(list) = running()? else { return Ok(None) };
    for c in &list {
        if !wait_settled(c, SETTLE_NS) {
            return Err(format!("{}: hotplug not settled within {} s", c.name(), SETTLE_NS / 1_000_000_000));
        }
    }
    Ok(Some(list))
}

/// Every connected root port of every controller has its device; under QEMU the boot stick is there,
/// configured, with its mass storage interface.
fn boot() -> TestResult {
    let Some(list) = settled_list()? else { return ktest::skip("no xHCI controller in this machine") };
    let mut total = 0;
    for c in &list {
        let devs = devices_of(c);
        for p in 1..=c.caps().max_ports {
            let st = decode(c.rd(c.caps().portsc(p)));
            if !st.connected || c.protocol_of(p).is_none() {
                continue;
            }
            kassert!(devs.iter().any(|d| d.place.root_port == p && d.place.depth == 0),
                "{} port {p}: connected (PORTSC {:#x}) but no device enumerated", c.name(), st.raw);
        }
        for d in &devs {
            let i = c.slot_info(d.slot).ok_or_else(|| format!("{d:?}: slot gone"))?;
            kassert!(i.dcbaa_ok, "{d:?}: DCBAA entry is not its output context");
            // Configured with endpoints = Configured; a configuration without endpoints may stay Addressed.
            kassert!(i.state == slot_state::CONFIGURED || i.state == slot_state::ADDRESSED, "{d:?}: slot state {}", i.state);
            kassert!(i.address == d.address && i.address != 0, "{d:?}: address {} vs {}", i.address, d.address);
            klog::event!(Info, "usb", "ktest.device", device = d.path(), vendor = klog::Hex(d.vendor() as u64),
                product = klog::Hex(d.product() as u64), speed = d.speed().as_str(), slot = i.id, address = i.address,
                mps0 = i.mps0, drivers = registry::bindings_of(d.id).len());
        }
        total += devs.len();
    }
    if hal::cpu::detect().hypervisor && list.iter().any(|c| c.env().pci().vendor == QEMU_XHCI) {
        let stick = registry::devices().into_iter().find(|d| (d.vendor(), d.product()) == QEMU_STICK);
        let stick = stick.ok_or("QEMU's boot stick (46F4:0001) was not enumerated")?;
        let cfg = stick.config.as_ref().ok_or("boot stick not configured")?;
        kassert!(cfg.defaults().any(|f| f.class == 8 && f.subclass == 6 && f.protocol == 0x50), "no BOT mass storage interface");
    }
    klog::event!(Info, "usb", "ktest.enumerated", devices = total, controllers = list.len());
    Ok(())
}
ktest!("us.enum.boot", Safe, boot);

/// The controller's view of every device matches the core's: route string, root port, speed, TT of
/// low/full speed devices below high speed hubs, hub slots with their port count.
fn topology() -> TestResult {
    let Some(list) = settled_list()? else { return ktest::skip("no xHCI controller in this machine") };
    let mut seen = 0;
    for c in &list {
        for d in devices_of(c) {
            seen += 1;
            let i = c.slot_info(d.slot).ok_or_else(|| format!("{d:?}: slot gone"))?;
            kassert!(i.route == d.place.route && i.root_port == d.place.root_port, "{d:?}: route {:#x}/{}", i.route, i.root_port);
            kassert!(i.speed == d.speed().xhci_psiv(), "{d:?}: slot speed {}", i.speed);
            match d.place.tt {
                Some(tt) => kassert!(i.tt_slot as u64 == tt.hub.0 & 0xFF && i.tt_port == tt.port && i.mtt == tt.multi,
                    "{d:?}: TT {}/{} mtt {}", i.tt_slot, i.tt_port, i.mtt),
                None => kassert!(i.tt_slot == 0, "{d:?}: TT slot {} without a TT", i.tt_slot),
            }
            if let Some((parent, port)) = d.place.parent {
                let p = registry::find(parent).ok_or_else(|| format!("{d:?}: parent {parent} not in the registry"))?;
                let h = hub::hubs().into_iter().find(|h| h.dev.id == parent).ok_or_else(|| format!("{p:?} is no bound hub"))?;
                kassert!(h.child(port).is_some_and(|x| x.id == d.id), "{d:?} not on port {port} of {p:?}");
            }
        }
        for h in hub::hubs().into_iter().filter(|h| h.dev.host_name() == c.name()) {
            let i = c.slot_info(h.dev.slot).ok_or("hub slot gone")?;
            // QEMU keeps only Context Entries from a Configure Endpoint's slot context; hardware shows the hub.
            if c.env().pci().vendor != QEMU_XHCI {
                kassert!(i.hub && i.ports == h.ports(), "{:?}: slot hub={} ports={}", h.dev, i.hub, i.ports);
            }
            klog::event!(Info, "usb", "ktest.hub", hub = h.dev.path(), ports = h.ports(), mtt = h.multi_tt, speed = h.dev.speed().as_str());
        }
    }
    if seen == 0 {
        return ktest::skip("no USB device connected");
    }
    Ok(())
}
ktest!("us.enum.topology", Safe, topology);

/// 300 control transfers on endpoint 0 (its ring past its Link TRB three times) and 300 Evaluate Contexts with
/// the same packet size (the command ring past its Link TRB): every one answered, the device still works.
fn rings_wrap() -> TestResult {
    let Some(list) = settled_list()? else { return ktest::skip("no xHCI controller in this machine") };
    let devs: Vec<_> = list.iter().flat_map(|c| devices_of(c)).collect();
    // A mass storage device if there is one (on the Mini-PC the stick, not Georg's receiver).
    let storage = |d: &&Arc<Device>| d.config.as_ref().is_some_and(|c| c.defaults().any(|f| f.class == 8));
    let d = devs.iter().find(|d| (d.vendor(), d.product()) == QEMU_STICK).or(devs.iter().find(storage)).or(devs.first());
    let Some(d) = d else { return ktest::skip("no USB device connected") };
    let c = list.iter().find(|c| c.name() == d.host_name()).ok_or("controller")?;
    let mps0 = c.slot_info(d.slot).ok_or("slot")?.mps0;
    let (cw0, _, _) = c.ring_info();
    for i in 0..300 {
        let mut b = [0u8; 18];
        let n = d.control_in(Setup::get_descriptor(1, 0, 18), &mut b).map_err(|e| format!("transfer {i}: {e:?}"))?;
        kassert!(n == 18 && b[1] == 1, "transfer {i}: {n} bytes, type {}", b[1]);
        d.host().set_mps0(d.slot, mps0).map_err(|e| format!("Evaluate Context {i}: {e:?}"))?;
    }
    let (cw1, _, _) = c.ring_info();
    kassert!(cw1 > cw0, "command ring did not wrap ({cw0} -> {cw1})");
    kassert!(c.state() == State::Running);
    klog::event!(Info, "usb", "ktest.rings_wrap", device = d.path(), cmd_wraps = cw1 - cw0);
    Ok(())
}
ktest!("us.enum.rings_wrap", Safe, rings_wrap);

/// Quiesce and restart the controller with the most devices: every device detached (new ids afterwards), the
/// same set found again, no slot of the old epoch left.
fn restart() -> TestResult {
    let Some(list) = settled_list()? else { return ktest::skip("no xHCI controller in this machine") };
    let Some(c) = list.iter().max_by_key(|c| devices_of(c).len()) else { return ktest::skip("no controller") };
    let before = devices_of(c);
    if before.is_empty() {
        return ktest::skip("no USB device connected");
    }
    let key = |d: &Arc<Device>| (d.vendor(), d.product(), d.place.root_port, d.place.route);
    devmodel::quiesce_matching(c.name());
    c.restart("ktest us.enum.restart")?;
    sched::sleep_ns(50 * MS);
    kassert!(wait_settled(c, SETTLE_NS), "not settled after the restart");
    let after = devices_of(c);
    kassert!(before.iter().all(|d| d.is_gone()), "a device of the old epoch is not gone");
    let mut kb: Vec<_> = before.iter().map(key).collect();
    let mut ka: Vec<_> = after.iter().map(key).collect();
    kb.sort();
    ka.sort();
    kassert!(kb == ka, "before {kb:x?}, after {ka:x?}");
    kassert!(before.iter().all(|d| c.slot_info(d.slot).is_none()), "an old handle still reaches a slot");
    kassert!(c.live_slots().len() == after.len(), "{} slots for {} devices", c.live_slots().len(), after.len());
    Ok(())
}
ktest!("us.enum.restart", QemuOnly, restart);

// ---- unplug while a key is held (scenario host/tests/us/30-unplug.sh)

static ARMED: AtomicBool = AtomicBool::new(false);
static DEV: AtomicU64 = AtomicU64::new(0);
static KEY_DOWN: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicU64 = AtomicU64::new(0);
static DISCONNECTS: AtomicU64 = AtomicU64::new(0);
static LAST_KEY: AtomicU8 = AtomicU8::new(0);

static KBD: [UsbMatch; 1] = [UsbMatch::Interface { class: 3, subclass: Some(1), protocol: Some(1) }];
static TEST_DRIVER: UsbDriver = UsbDriver { name: "ktest-kbd", matches: &KBD, probe: kbd_probe, disconnect: kbd_disconnect };

fn kbd_probe(d: &Arc<Device>, f: &Interface) -> Result<(), String> {
    if !ARMED.load(Ordering::Acquire) {
        return Err("only while us.unplug.held runs".into());
    }
    let ep = f.endpoints.iter().find(|e| e.is_in()).ok_or("no IN endpoint")?;
    let sink = Arc::new(|e: PipeEvent<'_>| match e {
        PipeEvent::Data(r) => {
            if r.get(2).is_some_and(|&k| k != 0) {
                LAST_KEY.store(r[2], Ordering::Release);
                KEY_DOWN.store(true, Ordering::Release);
            }
        }
        PipeEvent::Error(_) => {}
        PipeEvent::Stopped(_) => {
            STOPPED.fetch_add(1, Ordering::AcqRel);
        }
    });
    d.interrupt_in(ep.address, ep.max_packet().max(8) as usize, sink).map_err(|e| String::from(e.as_str()))?;
    DEV.store(d.id, Ordering::Release);
    Ok(())
}

fn kbd_disconnect(d: &Arc<Device>, _f: u8) {
    if DEV.load(Ordering::Acquire) == d.id {
        DISCONNECTS.fetch_add(1, Ordering::AcqRel);
    }
}

fn until(ns: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(ns);
    while time::now_ns() < deadline {
        if f() {
            return true;
        }
        sched::sleep_ns(10 * MS);
    }
    f()
}

fn unplug_held() -> TestResult {
    if !pci::registry::opted_in("usb_unplug_test") {
        return ktest::skip("scenario only: host/tests/us/30-unplug.sh boots with usb_unplug_test=on and drives QMP");
    }
    ARMED.store(true, Ordering::Release);
    let r = unplug_rounds();
    ARMED.store(false, Ordering::Release);
    registry::unregister_driver(TEST_DRIVER.name);
    r
}

fn unplug_rounds() -> TestResult {
    let ctrls = controllers();
    for round in 1..=2 {
        DEV.store(0, Ordering::Release);
        KEY_DOWN.store(false, Ordering::Release);
        STOPPED.store(0, Ordering::Release);
        DISCONNECTS.store(0, Ordering::Release);
        registry::register_driver(&TEST_DRIVER);
        kassert!(until(30_000 * MS, || DEV.load(Ordering::Acquire) != 0), "round {round}: no boot keyboard bound within 30 s");
        let id = DEV.load(Ordering::Acquire);
        let d = registry::find(id).ok_or("keyboard left the registry before the test")?;
        let c = ctrls.iter().find(|c| c.name() == d.host_name()).ok_or("its controller")?.clone();
        let slot = d.slot;
        klog::event!(Info, "usb", "ktest.kbd_ready", round = round, device = d.path(), speed = d.speed().as_str(),
            depth = d.place.depth; "send a key down, then device_del");
        kassert!(until(60_000 * MS, || KEY_DOWN.load(Ordering::Acquire)), "round {round}: no key-down report within 60 s");
        klog::event!(Info, "usb", "ktest.key_held", round = round, key = LAST_KEY.load(Ordering::Acquire));
        drop(d);
        kassert!(until(30_000 * MS, || registry::find(id).is_none()), "round {round}: keyboard not detached within 30 s");
        kassert!(until(1000 * MS, || DISCONNECTS.load(Ordering::Acquire) == 1), "round {round}: disconnect called {} times",
            DISCONNECTS.load(Ordering::Acquire));
        kassert!(STOPPED.load(Ordering::Acquire) >= 1, "round {round}: the pipe was not stopped");
        kassert!(c.slot_info(slot).is_none(), "round {round}: the slot outlived its device");
        kassert!(!c.live_slots().contains(&((slot.0 & 0xFF) as u8)), "round {round}: slot still enabled");
        kassert!(c.state() == State::Running && c.noop().is_ok(), "round {round}: controller hurt by the unplug");
        klog::event!(Info, "usb", "ktest.unplug_done", round = round; "device_add it again");
        registry::unregister_driver(TEST_DRIVER.name);
        // The keyboard comes back (round 2 binds it again and pulls it once more).
        kassert!(until(30_000 * MS, || registry::devices().iter().any(|x| x.desc.vendor == 0x0627 && x.id != id)),
            "round {round}: no keyboard enumerated after the replug");
        klog::event!(Info, "usb", "ktest.replug_seen", round = round);
    }
    Ok(())
}
ktest!("us.unplug.held", QemuOnly, unplug_held);
