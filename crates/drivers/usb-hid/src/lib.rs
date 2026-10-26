//! USB HID class driver (owner IN, item IN-2a; spec usb.md §4, CONTRACTS "USB class drivers"): every HID
//! interface of the default alternate settings is bound, its report descriptor parsed by `hidparse` (report
//! protocol; boot protocol as fallback for boot keyboards and mice), each application becomes an input device
//! in `console::evdev` (Linux hid-generic + hid-input), reports arrive through US-3's interrupt pipes, an unplug
//! releases every held key at once, and keyboards show the console's Caps/Num/Scroll Lock (`leds`). MCP tool
//! `input` shows it all. CONFIG `usbhid_raw=on` (or US's `usb_xfer_test=on`/`usb_unplug_test=on`) leaves HID
//! devices unbound.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod class;
pub mod iface;
pub mod leds;
mod ktests;
mod ktests_fake;
mod tool;
mod tool_latency;
pub mod worker;

use alloc::string::String;
use alloc::sync::Arc;
use usb_core::{Device, Interface, UsbDriver, UsbMatch};

static MATCHES: [UsbMatch; 1] = [UsbMatch::Interface { class: 3, subclass: None, protocol: None }];

pub static DRIVER: UsbDriver = UsbDriver { name: "usb-hid", matches: &MATCHES, probe, disconnect };

usb_core::usb_driver!("usb-hid", DRIVER);

fn probe(dev: &Arc<Device>, f: &Interface) -> Result<(), String> {
    iface::probe(dev, f).map(|_| ())
}

fn disconnect(dev: &Arc<Device>, interface: u8) {
    if let Some(h) = iface::take(dev, interface) {
        h.shutdown();
        klog::event!(Info, "input", "hid.unbound", device = dev.path(), interface = interface, gone = dev.is_gone());
    }
}
