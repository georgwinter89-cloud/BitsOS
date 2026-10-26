//! Enumeration of a device that was just reset and enabled (V1 `usb_setup`/`usb_config`, spec usb.md §3.3):
//! slot + address with endpoint 0 at the speed's guess, the first 8 bytes of the device descriptor, endpoint 0's
//! real size told to the controller if it differs (cheap full speed devices have 8), the whole descriptor, the
//! first configuration read and set (its endpoints given to the controller first), then the drivers.
//! `Scheme::Deferred` (a retry; Linux's xHCI order): slot without SET_ADDRESS, the 8 bytes read at address 0,
//! endpoint 0 fixed, then SET_ADDRESS. Some cheap full speed devices fail a SET_ADDRESS straight after a reset.

use crate::desc::{mps0_from_first8, Configuration, DeviceDescriptor, Endpoint};
use crate::device::Device;
use crate::host::{Data, HcSlot, Host, UsbError};
use crate::place::Place;
use crate::registry;
use crate::setup::{dt, Setup};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use ksync::Mutex;

klog::counter!(MPS0_FIXED, "usb.mps0_fixed");

/// SET_ADDRESS recovery (USB 2.0 §9.2.6.3).
pub const SET_ADDRESS_RECOVERY_NS: u64 = 2_000_000;

/// How the address is given (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Direct,
    Deferred,
}

pub fn enumerate(host: &Arc<dyn Host>, place: Place, scheme: Scheme) -> Result<Arc<Device>, UsbError> {
    let guess = place.speed.default_mps0();
    let deferred = scheme == Scheme::Deferred;
    let (slot, mut address) = host.address(&place, guess, deferred)?;
    let early = if deferred {
        match first8(host.as_ref(), slot, &place, guess).and_then(|()| host.set_address(slot)) {
            Ok(a) => {
                address = a;
                Ok(())
            }
            Err(e) => Err(e),
        }
    } else {
        Ok(())
    };
    match early.and_then(|()| setup(host.as_ref(), slot, &place, guess, !deferred)) {
        Ok((desc, config)) => {
            let dev = Arc::new(Device {
                id: registry::next_id(),
                host: host.clone(),
                slot,
                address,
                place,
                desc,
                config,
                gone: AtomicBool::new(false),
                bind_lock: Mutex::named("usb.dev_bind", ()),
                alts: Default::default(),
            });
            registry::add(dev.clone());
            let cfg = dev.config.as_ref();
            klog::event!(Info, "usb", "device.attached", device = dev.path(), id = dev.id, speed = place.speed.as_str(),
                address = address, route = klog::Hex(place.route as u64), depth = place.depth,
                vendor = klog::Hex(dev.desc.vendor as u64), product = klog::Hex(dev.desc.product as u64),
                class = klog::Hex(dev.desc.class as u64), mps0 = mps0_of(&dev.desc, place.speed, guess),
                config = cfg.map(|c| c.value).unwrap_or(0), interfaces = cfg.map(|c| c.defaults().count()).unwrap_or(0),
                tt_port = place.tt.map(|t| t.port).unwrap_or(0));
            registry::bind(&dev);
            Ok(dev)
        }
        Err(e) => {
            host.release(slot);
            Err(e)
        }
    }
}

fn mps0_of(d: &DeviceDescriptor, speed: crate::speed::Speed, guess: u16) -> u16 {
    let mut b = [0u8; 8];
    b[1] = dt::DEVICE;
    b[7] = d.max_packet0;
    mps0_from_first8(&b, speed).unwrap_or(guess)
}

/// The first 8 bytes of the device descriptor; endpoint 0's size told to the controller if they show another.
fn first8(host: &dyn Host, slot: HcSlot, place: &Place, guess: u16) -> Result<(), UsbError> {
    let mut d8 = [0u8; 8];
    let n = host.control(slot, Setup::get_descriptor(dt::DEVICE, 0, 8), Data::In(&mut d8))?;
    if n < 8 {
        return Err(UsbError::Descriptor);
    }
    if let Some(m) = mps0_from_first8(&d8, place.speed)
        && m != guess
    {
        host.set_mps0(slot, m)?;
        MPS0_FIXED.inc();
        klog::event!(Debug, "usb", "mps0.fixed", hub_port = place.path(), from = guess, to = m);
    }
    Ok(())
}

/// After SET_ADDRESS: the device descriptor (its first 8 bytes before, unless already read at address 0), the
/// first configuration read and set.
fn setup(host: &dyn Host, slot: HcSlot, place: &Place, guess: u16, need_first8: bool) -> Result<(DeviceDescriptor, Option<Configuration>), UsbError> {
    host.sleep_ns(SET_ADDRESS_RECOVERY_NS);
    if need_first8 {
        first8(host, slot, place, guess)?;
    }
    let mut d = [0u8; 18];
    let n = host.control(slot, Setup::get_descriptor(dt::DEVICE, 0, 18), Data::In(&mut d))?;
    let desc = DeviceDescriptor::parse(&d[..n]).map_err(|_| UsbError::Descriptor)?;
    if desc.num_configurations == 0 {
        return Ok((desc, None));
    }
    let config = read_config(host, slot)?;
    let eps: Vec<Endpoint> = config.defaults().flat_map(|f| f.endpoints.iter().copied()).collect();
    host.configure(slot, &eps)?;
    host.control(slot, Setup::set_configuration(config.value), Data::None)?;
    Ok((desc, Some(config)))
}

/// The first configuration: 9 bytes for wTotalLength, then all of it (at most 64 KiB - 1 by its size field).
fn read_config(host: &dyn Host, slot: HcSlot) -> Result<Configuration, UsbError> {
    let mut h = [0u8; 9];
    let n = host.control(slot, Setup::get_descriptor(dt::CONFIGURATION, 0, 9), Data::In(&mut h))?;
    if n < 9 || h[1] != dt::CONFIGURATION {
        return Err(UsbError::Descriptor);
    }
    let total = u16::from_le_bytes([h[2], h[3]]).max(9);
    let mut all = vec![0u8; total as usize];
    let n = host.control(slot, Setup::get_descriptor(dt::CONFIGURATION, 0, total), Data::In(&mut all))?;
    Configuration::parse(&all[..n]).map_err(|_| UsbError::Descriptor)
}
