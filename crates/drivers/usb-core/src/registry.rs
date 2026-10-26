//! The device registry and class-driver matching. Drivers register at any time (`usb_driver!` from an initcall,
//! or at run time); each interface of a device's configuration (default alternate setting) is bound to at most
//! one driver: the hub driver first, then drivers matching the device's ID, then drivers matching the class.
//! Detach (V1 `usb_detach`, spec usb.md §3.3): marked gone at once, children first, the slot released (no
//! transfer brings anything any more), then every bound driver's `disconnect`, then out of the registry.

use crate::desc::Interface;
use crate::device::Device;
use crate::hub;
use crate::place::DeviceId;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;

klog::counter!(ATTACHED, "usb.devices_attached");
klog::counter!(DETACHED, "usb.devices_detached");
klog::counter!(BOUND, "usb.drivers_bound");
klog::counter!(PROBE_FAILED, "usb.driver_probe_failures");

#[derive(Clone, Copy, Debug)]
pub enum UsbMatch {
    Device { vendor: u16, product: u16 },
    /// Interface class with optional subclass and protocol.
    Interface { class: u8, subclass: Option<u8>, protocol: Option<u8> },
}

impl UsbMatch {
    fn hits(&self, dev: &Device, f: &Interface) -> bool {
        match *self {
            UsbMatch::Device { vendor, product } => dev.desc.vendor == vendor && dev.desc.product == product,
            UsbMatch::Interface { class, subclass, protocol } => {
                f.class == class && subclass.is_none_or(|s| s == f.subclass) && protocol.is_none_or(|p| p == f.protocol)
            }
        }
    }
}

pub struct UsbDriver {
    pub name: &'static str,
    pub matches: &'static [UsbMatch],
    /// Runs in the hotplug worker of the device's controller; may do control transfers and start pipes.
    pub probe: fn(&Arc<Device>, &Interface) -> Result<(), String>,
    /// The device is gone (its slot is already released): drop state, release held keys at once.
    pub disconnect: fn(&Arc<Device>, u8),
}

#[derive(Clone, Copy)]
struct Binding {
    device: DeviceId,
    interface: u8,
    driver: &'static UsbDriver,
}

static DEVICES: SpinLock<Vec<Arc<Device>>> = SpinLock::named("usb.devices", Vec::new());
static DRIVERS: SpinLock<Vec<&'static UsbDriver>> = SpinLock::named("usb.drivers", Vec::new());
static BINDINGS: SpinLock<Vec<Binding>> = SpinLock::named("usb.bindings", Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
/// Bumped at every attach and detach (tests and tools watch it).
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// `usb_core::usb_driver!("usb-hid", DRIVER);` registers a `static DRIVER: UsbDriver` from an initcall.
#[macro_export]
macro_rules! usb_driver {
    ($name:literal, $driver:path) => {
        const _: () = {
            fn __usb_driver_init() -> Result<(), &'static str> {
                $crate::registry::register_driver(&$driver);
                Ok(())
            }
            $crate::__kreg::initcall!($crate::__kreg::Level::Subsys, $name, __usb_driver_init);
        };
    };
}

pub(crate) fn next_id() -> DeviceId {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

pub fn devices() -> Vec<Arc<Device>> {
    DEVICES.lock().clone()
}

pub fn find(id: DeviceId) -> Option<Arc<Device>> {
    DEVICES.lock().iter().find(|d| d.id == id).cloned()
}

/// (interface, driver name) of every binding of a device.
pub fn bindings_of(id: DeviceId) -> Vec<(u8, &'static str)> {
    BINDINGS.lock().iter().filter(|b| b.device == id).map(|b| (b.interface, b.driver.name)).collect()
}

/// Registers a class driver and offers it every interface no driver has yet.
pub fn register_driver(d: &'static UsbDriver) {
    {
        let mut l = DRIVERS.lock();
        if l.iter().any(|x| x.name == d.name) {
            return;
        }
        l.push(d);
    }
    klog::event!(Info, "usb", "driver.registered", driver = d.name);
    for dev in devices() {
        bind(&dev);
    }
}

/// Removes a driver (tests): its bindings are disconnected first.
pub fn unregister_driver(name: &str) {
    DRIVERS.lock().retain(|d| d.name != name);
    let mut gone = Vec::new();
    BINDINGS.lock().retain(|b| {
        let keep = b.driver.name != name;
        if !keep {
            gone.push(*b);
        }
        keep
    });
    for b in gone {
        if let Some(dev) = find(b.device) {
            (b.driver.disconnect)(&dev, b.interface);
        }
    }
}

pub(crate) fn add(dev: Arc<Device>) {
    DEVICES.lock().push(dev);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    ATTACHED.inc();
}

fn bound(dev: DeviceId, interface: u8) -> bool {
    BINDINGS.lock().iter().any(|b| b.device == dev && b.interface == interface)
}

/// Offers every unbound interface of the device's configuration to the drivers.
pub fn bind(dev: &Arc<Device>) {
    let Some(cfg) = dev.config.as_ref() else { return };
    let _serial = dev.bind_lock.lock();
    for f in cfg.defaults() {
        if dev.is_gone() || bound(dev.id, f.number) {
            continue;
        }
        let candidates: Vec<&'static UsbDriver> = {
            let drivers = DRIVERS.lock();
            let by_id = drivers.iter().filter(|d| d.matches.iter().any(|m| matches!(m, UsbMatch::Device { .. }) && m.hits(dev, f)));
            let by_class = drivers.iter().filter(|d| d.matches.iter().any(|m| matches!(m, UsbMatch::Interface { .. }) && m.hits(dev, f)));
            let hub = Some(&hub::DRIVER).filter(|h| h.matches.iter().any(|m| m.hits(dev, f)));
            hub.into_iter().chain(by_id.copied()).chain(by_class.copied()).collect()
        };
        for d in candidates {
            match (d.probe)(dev, f) {
                Ok(()) => {
                    // `detach` sets `gone` before it collects the bindings under this lock, so a device gone by
                    // now never gets a binding nobody would disconnect: its driver hears `disconnect` here.
                    let pushed = {
                        let mut l = BINDINGS.lock();
                        let alive = !dev.is_gone();
                        if alive {
                            l.push(Binding { device: dev.id, interface: f.number, driver: d });
                        }
                        alive
                    };
                    if !pushed {
                        klog::event!(Debug, "usb", "driver.bound_to_gone", device = dev.path(), interface = f.number,
                            driver = d.name; "device detached during probe; disconnected at once");
                        (d.disconnect)(dev, f.number);
                        return;
                    }
                    BOUND.inc();
                    klog::event!(Info, "usb", "driver.bound", device = dev.path(), interface = f.number, driver = d.name,
                        class = klog::Hex(f.class as u64));
                    break;
                }
                Err(e) => {
                    PROBE_FAILED.inc();
                    klog::event!(Warn, "usb", "driver.probe_failed", device = dev.path(), interface = f.number,
                        driver = d.name, error = e.as_str());
                }
            }
        }
    }
}

/// Detaches a device and everything below it. Safe to call twice (the second call does nothing).
pub fn detach(dev: &Arc<Device>) {
    if dev.gone.swap(true, Ordering::AcqRel) {
        return;
    }
    // Children first: a hub's devices are gone with it.
    if let Some(h) = hub::remove(dev.id) {
        for c in h.take_children() {
            detach(&c);
        }
    }
    dev.host.release(dev.slot);
    let mine: Vec<Binding> = {
        let mut l = BINDINGS.lock();
        let v = l.iter().filter(|b| b.device == dev.id).copied().collect();
        l.retain(|b| b.device != dev.id);
        v
    };
    for b in &mine {
        (b.driver.disconnect)(dev, b.interface);
    }
    DEVICES.lock().retain(|d| d.id != dev.id);
    GENERATION.fetch_add(1, Ordering::AcqRel);
    DETACHED.inc();
    klog::event!(Info, "usb", "device.detached", device = dev.path(), id = dev.id, vendor = klog::Hex(dev.desc.vendor as u64),
        product = klog::Hex(dev.desc.product as u64), drivers = mine.len());
}

/// Detaches every device of a host (controller restarted or failed), deepest first.
pub fn detach_host(name: &str) -> usize {
    let mut v: Vec<Arc<Device>> = DEVICES.lock().iter().filter(|d| d.host_name() == name).cloned().collect();
    v.sort_by_key(|d| core::cmp::Reverse(d.place.depth));
    let n = v.len();
    for d in &v {
        detach(d);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desc::Interface;

    fn probe(_: &Arc<Device>, _: &Interface) -> Result<(), String> {
        Err("never".into())
    }
    fn disconnect(_: &Arc<Device>, _: u8) {}
    static M: [UsbMatch; 1] = [UsbMatch::Interface { class: 0xFE, subclass: None, protocol: None }];
    static MACRO_DRIVER: UsbDriver = UsbDriver { name: "macro-test", matches: &M, probe, disconnect };
    crate::usb_driver!("usb-macro-test", MACRO_DRIVER);

    #[test]
    fn usb_driver_macro_registers_through_an_initcall() {
        let e = kreg::INITCALLS.entries().iter().find(|i| i.name == "usb-macro-test").expect("initcall placed");
        assert_eq!(e.level, kreg::Level::Subsys);
        (e.func)().unwrap();
        assert!(DRIVERS.lock().iter().any(|d| d.name == "macro-test"));
        unregister_driver("macro-test");
        assert!(!DRIVERS.lock().iter().any(|d| d.name == "macro-test"));
    }
}
