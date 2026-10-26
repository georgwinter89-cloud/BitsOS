//! In-kernel tests of IN-2a. Safe ones run on the Mini-PC too (pinned to CPU 0, possibly several times per boot:
//! they leave nothing behind); QemuOnly ones use the usb-kbd and usb-tablet every xtask ktest boot has.
//! The QMP-driven path (real key presses, mouse moves, unplug while a key is held) is host/tests/in/10-input.sh.

use crate::iface::{self, HidIface};
use crate::leds;
use alloc::format;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::evdev::{self, Caps, DeviceInfo, EvdevSink, InputEvent};
use core::sync::atomic::{AtomicU32, Ordering};
use hidparse::codes::*;
use ktest::{TestResult, kassert, ktest};
use usb_core::{EpKind, registry};

const MS: u64 = 1_000_000;
/// QEMU's HID devices (usb-kbd, usb-mouse, usb-tablet) are 0627:0001.
const QEMU_VENDOR: u16 = 0x0627;

fn until(ns: u64, f: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(ns);
    while time::now_ns() < deadline {
        if f() {
            return true;
        }
        sched::sleep_ns(10 * MS);
    }
    f()
}

/// Waits until no device came or went for 1 s (enumeration runs in the controllers' hotplug workers).
fn settled() -> bool {
    let deadline = time::now_ns().saturating_add(15_000 * MS);
    let mut last = (registry::generation(), time::now_ns());
    while time::now_ns() < deadline {
        sched::sleep_ns(50 * MS);
        let g = registry::generation();
        if g != last.0 {
            last = (g, time::now_ns());
        } else if time::now_ns().saturating_sub(last.1) >= 1000 * MS {
            return true;
        }
    }
    false
}

fn fake_keyboard(sink: &'static EvdevSink, name: &str) -> Arc<evdev::InputDevice> {
    let mut caps = Caps::default();
    caps.keys.extend([KEY_A, KEY_LEFTSHIFT, KEY_CAPSLOCK]);
    caps.leds.extend([LED_NUML, LED_CAPSL, LED_SCROLLL]);
    evdev::register(sink, DeviceInfo { name: name.to_string(), caps, ..DeviceInfo::default() })
}

/// The evdev layer on a private sink: filtering, the held-key union of two keyboards, releases on unplug and
/// the Caps Lock toggle with its LED listener. Nothing reaches the machine's real input.
fn evdev_union() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    static S: EvdevSink = EvdevSink::new();
    static HEARD: AtomicU32 = AtomicU32::new(u32::MAX);
    while S.pop().is_some() {}
    S.set_locks(0);
    let id = S.add_led_listener(Arc::new(|bits| HEARD.store(bits, Ordering::SeqCst)));
    let (a, b) = (fake_keyboard(&S, "ktest kbd a"), fake_keyboard(&S, "ktest kbd b"));
    let result = (|| {
        kassert!(a.emit(0, &[(EV_KEY, KEY_LEFTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]) == 2);
        kassert!(a.emit(0, &[(EV_KEY, KEY_LEFTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]) == 0, "unchanged key passed");
        b.emit(0, &[(EV_KEY, KEY_LEFTSHIFT, 1), (EV_SYN, SYN_REPORT, 0)]);
        a.emit(0, &[(EV_KEY, KEY_LEFTSHIFT, 0), (EV_SYN, SYN_REPORT, 0)]);
        kassert!(S.key_held_physical(KEY_LEFTSHIFT), "b still holds shift");
        b.emit(0, &[(EV_KEY, KEY_CAPSLOCK, 1), (EV_SYN, SYN_REPORT, 0)]);
        kassert!(S.locks() == 1 << LED_CAPSL && HEARD.load(Ordering::SeqCst) == 1 << LED_CAPSL, "caps lock toggled");
        b.unregister(time::now_ns());
        kassert!(S.held_keys().is_empty(), "held after unplug: {:?}", S.held_keys());
        let tail: Vec<(u32, u16, u16, i32)> = core::iter::from_fn(|| S.pop()).map(|e| (e.device, e.kind, e.code, e.value)).collect();
        let n = b.number;
        kassert!(tail.ends_with(&[(n, EV_KEY, KEY_LEFTSHIFT, 0), (n, EV_KEY, KEY_CAPSLOCK, 0), (n, EV_SYN, SYN_REPORT, 0)]),
            "unplug releases: {tail:?}");
        Ok(())
    })();
    S.remove_led_listener(id);
    a.unregister(0);
    b.unregister(0);
    S.set_locks(0);
    result
}
ktest!("in.evdev.union", Safe, evdev_union);

/// Every HID interface with an interrupt IN endpoint is bound to usb-hid with its input devices registered, or
/// refused for a reason (on the Mini-PC: Georg's receiver, whose vendor interface may legitimately be refused).
/// A boot keyboard or mouse must always bind (the boot protocol is its fallback), and an interface usb-hid
/// neither bound nor refused is an error. Skips when no HID device is plugged in or CONFIG leaves them raw.
fn devices() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    kassert!(settled(), "USB devices did not settle in 15 s");
    if let Some(key) = iface::left_raw() {
        return ktest::skip(&format!("CONFIG {key}=on leaves HID devices unbound"));
    }
    let (mut seen, mut refused) = (0, 0);
    let refusals = iface::refusals();
    for d in registry::devices() {
        let Some(cfg) = d.config.as_ref() else { continue };
        for f in cfg.defaults().filter(|f| f.class == 3) {
            if !f.endpoints.iter().any(|e| e.is_in() && e.kind() == EpKind::Interrupt) {
                continue;
            }
            seen += 1;
            let bound = registry::bindings_of(d.id).iter().any(|&(i, n)| i == f.number && n == "usb-hid");
            if !bound {
                let why = refusals.iter().find(|(x, i, _)| x.id == d.id && *i == f.number).map(|(_, _, w)| w.clone());
                let boot = f.subclass == crate::class::SUBCLASS_BOOT
                    && (f.protocol == crate::class::PROTOCOL_KEYBOARD || f.protocol == crate::class::PROTOCOL_MOUSE);
                let Some(why) = why else {
                    return Err(format!("{} interface {} ({:04x}:{:04x}) neither bound nor refused by usb-hid", d.path(), f.number,
                        d.vendor(), d.product()));
                };
                klog::event!(Warn, "input", "ktest.hid_refused", device = d.path(), interface = f.number, boot = boot,
                    vendor = klog::Hex(d.vendor() as u64), product = klog::Hex(d.product() as u64), reason = why.as_str());
                kassert!(!boot, "boot interface {} {} ({:04x}:{:04x}) refused: {why}", d.path(), f.number, d.vendor(), d.product());
                refused += 1;
                continue;
            }
            let h = iface::all().into_iter().find(|h| h.dev.id == d.id && h.iface == f.number).ok_or("bound but not listed")?;
            // An input application whose usages all map to nothing gets no input device (Linux neither); one that
            // maps to something must have one.
            let maps_input = h.map.groups.iter().any(|g| !g.is_empty());
            kassert!(!maps_input || !h.numbers().is_empty(), "{} interface {}: no input device", d.path(), f.number);
            let registered: Vec<u32> = evdev::devices().iter().map(|x| x.number).collect();
            kassert!(h.numbers().iter().all(|n| registered.contains(n)), "input devices not registered");
            klog::event!(Info, "input", "ktest.hid_device", device = d.path(), interface = f.number, protocol = h.protocol.as_str(),
                inputs = format!("{:?}", h.numbers()), reports = h.reports.load(Ordering::Relaxed),
                pipe = iface::pipe_state_name(h.pipe_state.load(Ordering::Acquire)));
        }
    }
    if seen == 0 {
        return ktest::skip("no USB HID device plugged in");
    }
    klog::event!(Info, "input", "ktest.hid_devices", interfaces = seen, refused = refused);
    Ok(())
}
ktest!("in.hid.devices", Safe, devices);

fn qemu_keyboard() -> Option<Arc<HidIface>> {
    iface::all().into_iter().find(|h| h.dev.vendor() == QEMU_VENDOR && h.leds.present())
}

/// QEMU's usb-kbd and usb-tablet in the report protocol: a keyboard with LEDs and an absolute pointer.
fn qemu_devices() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    kassert!(settled(), "USB devices did not settle");
    let found = until(5000 * MS, || qemu_keyboard().is_some());
    if !found && !registry::devices().iter().any(|d| d.vendor() == QEMU_VENDOR) {
        return ktest::skip("no QEMU usb-kbd (xtask ktest adds usb-kbd and usb-tablet)");
    }
    let k = qemu_keyboard().ok_or("usb-kbd not bound")?;
    kassert!(k.protocol == iface::Protocol::Report, "keyboard in {} protocol", k.protocol.as_str());
    let kd = k.inputs.iter().flatten().next().ok_or("keyboard without input device")?;
    let c = &kd.info.caps;
    kassert!(c.is_keyboard() && c.keys.contains(&KEY_A) && c.keys.contains(&KEY_LEFTMETA) && c.repeat, "keyboard caps");
    kassert!(c.leds.contains(&LED_CAPSL) && c.leds.contains(&LED_NUML), "keyboard LEDs");
    let tablet = iface::all().into_iter().find(|h| {
        h.dev.vendor() == QEMU_VENDOR && h.inputs.iter().flatten().any(|d| d.info.caps.abs.contains_key(&ABS_X))
    });
    let t = tablet.ok_or("usb-tablet not bound")?;
    let td = t.inputs.iter().flatten().next().ok_or("tablet without input device")?;
    let x = td.info.caps.abs.get(&ABS_X).ok_or("no ABS_X")?;
    kassert!(x.min == 0 && x.max == 0x7FFF && td.info.caps.abs.contains_key(&ABS_Y), "tablet axes {:?}", td.info.caps.abs);
    kassert!(td.info.caps.keys.contains(&BTN_LEFT) && td.info.caps.rel.contains(&REL_WHEEL), "tablet buttons and wheel");
    Ok(())
}
ktest!("in.hid.qemu_devices", QemuOnly, qemu_devices);

/// Caps Lock and Num Lock from the console state reach QEMU's usb-kbd (SET_REPORT output, no interrupt OUT).
fn leds_follow_locks() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    kassert!(settled(), "USB devices did not settle");
    let Some(k) = qemu_keyboard() else { return ktest::skip("no bound QEMU usb-kbd") };
    let sink = evdev::sink();
    let before = sink.locks();
    let result = (|| {
        for bits in [1u32 << LED_CAPSL, 1 << LED_CAPSL | 1 << LED_NUML, 0] {
            let sends = k.leds.sends.load(Ordering::Acquire);
            sink.set_locks(bits);
            let ok = until(3000 * MS, || {
                k.leds.sent.load(Ordering::Acquire) == bits && k.leds.sends.load(Ordering::Acquire) > sends
            });
            kassert!(ok, "LEDs {bits:#x} not sent (sent {:#x})", k.leds.sent.load(Ordering::Acquire));
            kassert!(k.leds.status.load(Ordering::Acquire) == leds::STATUS_OK, "SET_REPORT failed for {bits:#x}");
        }
        Ok(())
    })();
    sink.set_locks(before);
    result
}
ktest!("in.hid.leds", QemuOnly, leds_follow_locks);

/// The report path end to end without QMP: a boot-layout report (Shift + A) given to usb-kbd's interface turns
/// into Linux events on its input device; removing the driver (as an unplug) releases both keys at once; the
/// driver binds again afterwards.
fn report_and_unplug() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    kassert!(settled(), "USB devices did not settle");
    let Some(k) = qemu_keyboard() else { return ktest::skip("no bound QEMU usb-kbd") };
    let sink = evdev::sink();
    let n = k.numbers()[0];
    let from = sink.next_seq();
    k.on_report(&[0x02, 0, 0x04, 0, 0, 0, 0, 0]);
    let got = |since: u64| -> Vec<(u16, u16, i32)> {
        sink.recent_since(since).into_iter().map(|(_, e)| e).filter(|e: &InputEvent| e.device == n).map(|e| (e.kind, e.code, e.value)).collect()
    };
    let want = [(EV_MSC, MSC_SCAN, 0x700E1), (EV_KEY, KEY_LEFTSHIFT, 1), (EV_MSC, MSC_SCAN, 0x70004), (EV_KEY, KEY_A, 1), (EV_SYN, SYN_REPORT, 0)];
    let ev = got(from);
    kassert!(ev == want, "events {ev:?}");
    kassert!(sink.key_held_physical(KEY_A) && sink.key_held_physical(KEY_LEFTSHIFT), "keys not held");
    let at_unplug = sink.next_seq();
    registry::unregister_driver(crate::DRIVER.name);
    let rel = got(at_unplug);
    let released = rel == [(EV_KEY, KEY_A, 0), (EV_KEY, KEY_LEFTSHIFT, 0), (EV_SYN, SYN_REPORT, 0)];
    let gone = !evdev::devices().iter().any(|d| d.number == n);
    // Physical only: an injected press another test left in the union (contract.evdev_injected) is not ours.
    let clear = !sink.key_held_physical(KEY_A) && !sink.key_held_physical(KEY_LEFTSHIFT);
    registry::register_driver(&crate::DRIVER);
    kassert!(released, "releases at unplug: {rel:?}");
    kassert!(gone && clear, "device gone {gone}, keys clear {clear}");
    kassert!(until(5000 * MS, || qemu_keyboard().is_some()), "usb-kbd not bound again");
    Ok(())
}
ktest!("in.hid.report_and_unplug", QemuOnly, report_and_unplug);
