//! SYS-K: `/sys/bus/usb/devices/<bus>-<path>` for every enumerated USB device (linux-7.3.1/drivers/usb/core/sysfs.c):
//! `idVendor`, `idProduct`, `bcdDevice` (`%04x`), `bDeviceClass`, `bDeviceSubClass`, `bDeviceProtocol` (`%02x`),
//! `bMaxPacketSize0`, `bNumConfigurations`, `busnum`, `devnum`, `devpath`, `speed` (Mbit/s as Linux words it) and
//! `version` (`%2x.%02x`), all 0444, from the descriptors the core read at enumeration (nothing is asked of the
//! device at a read). `manufacturer` and `product` exist on Linux only when the device's strings were read; BitsOS
//! does not read them at enumeration (that would be new requests to every device), so they are missing, as on Linux
//! for a device without those strings. No root hubs: an xHCI controller's USB 2 devices are on bus 2i+1, its
//! SuperSpeed ones on 2i+2 (Linux's two root hubs per controller), ports numbered as the controller numbers them.

use super::attr::{Live, PAGE};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::ENODEV;
use usb_core::device::Device;
use usb_core::speed::Speed;
use vfs::Errno;
use vfs::synth::{DirSource, SynthEntry};

/// Linux's `speed` words (sysfs.c `speed_show`).
pub fn speed_text(s: Speed) -> &'static str {
    match s {
        Speed::Low => "1.5",
        Speed::Full => "12",
        Speed::High => "480",
        Speed::Super => "5000",
        Speed::SuperPlus => "10000",
    }
}

/// The controller's number in its name (`xhci0` -> 0), else 0.
fn controller(name: &str) -> u32 {
    name.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().unwrap_or(0)
}

pub fn busnum(host: &str, s: Speed) -> u32 {
    2 * controller(host) + if s >= Speed::Super { 2 } else { 1 }
}

fn dir_name(d: &Device) -> String {
    format!("{}-{}", busnum(d.host_name(), d.speed()), d.place.path())
}

const ATTRS: [(&str, fn(&Device) -> String); 13] = [
    ("bDeviceClass", |d| format!("{:02x}\n", d.desc.class)),
    ("bDeviceProtocol", |d| format!("{:02x}\n", d.desc.protocol)),
    ("bDeviceSubClass", |d| format!("{:02x}\n", d.desc.subclass)),
    ("bMaxPacketSize0", |d| format!("{}\n", d.desc.max_packet0)),
    ("bNumConfigurations", |d| format!("{}\n", d.desc.num_configurations)),
    ("bcdDevice", |d| format!("{:04x}\n", d.desc.device)),
    ("busnum", |d| format!("{}\n", busnum(d.host_name(), d.speed()))),
    ("devnum", |d| format!("{}\n", d.address())),
    ("devpath", |d| format!("{}\n", d.place.path())),
    ("idProduct", |d| format!("{:04x}\n", d.desc.product)),
    ("idVendor", |d| format!("{:04x}\n", d.desc.vendor)),
    ("speed", |d| format!("{}\n", speed_text(d.speed()))),
    ("version", |d| format!("{:2x}.{:02x}\n", d.desc.usb >> 8, d.desc.usb & 0xff)),
];

fn live() -> Vec<Arc<Device>> {
    usb_core::registry::devices().into_iter().filter(|d| !d.is_gone()).collect()
}

struct DevDir(String);

impl DirSource for DevDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(ATTRS
            .iter()
            .enumerate()
            .map(|(i, (n, f))| {
                let (name, f) = (self.0.clone(), *f);
                let src = Live::new(PAGE, move || live().into_iter().find(|d| dir_name(d) == name).map(|d| f(&d)).ok_or(ENODEV));
                SynthEntry::file(n, 0o444, Arc::new(src)).cookie(i as u64 + 1)
            })
            .collect())
    }
}

struct Devices;

impl DirSource for Devices {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut names: Vec<String> = live().iter().map(|d| dir_name(d)).collect();
        names.sort();
        names.dedup();
        Ok(names.iter().enumerate().map(|(i, n)| SynthEntry::dir(n, Arc::new(DevDir(n.clone()))).with_perm(0o755).cookie(i as u64 + 1)).collect())
    }
}

struct Fixed(&'static str, fn() -> Arc<dyn DirSource>);

impl DirSource for Fixed {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(alloc::vec![SynthEntry::dir(self.0, (self.1)()).with_perm(0o755).cookie(1)])
    }
}

fn devices() -> Arc<dyn DirSource> {
    Arc::new(Devices)
}

fn usb() -> Arc<dyn DirSource> {
    Arc::new(Fixed("devices", devices))
}

fn bus() -> Arc<dyn DirSource> {
    Arc::new(Fixed("usb", usb))
}

fn root() -> Arc<dyn DirSource> {
    Arc::new(Fixed("bus", bus))
}
vfs::procdir!("/sys", root);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buses_and_speeds_as_linux_names_them() {
        assert_eq!((busnum("xhci0", Speed::High), busnum("xhci0", Speed::Super), busnum("xhci1", Speed::Full)), (1, 2, 3));
        assert_eq!((speed_text(Speed::Low), speed_text(Speed::High), speed_text(Speed::SuperPlus)), ("1.5", "480", "10000"));
        assert_eq!(format!("{:2x}.{:02x}", 0x0210u16 >> 8, 0x0210u16 & 0xff), " 2.10");
    }
}
