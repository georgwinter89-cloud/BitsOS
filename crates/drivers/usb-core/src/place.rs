//! Where a device sits: root port, route string, depth, its parent hub and the transaction translator it uses
//! (xHCI 1.2 §4.5.2, §6.2.2; USB 3.2 §8.9; V1 `usbenum.asm:4-96`).

use crate::host::HcSlot;
use crate::speed::Speed;
use alloc::format;
use alloc::string::String;

/// The route string has five nibbles, one per hub tier below the root port (USB 3.2 §8.9), so a device sits at
/// most five hubs deep. A bound set by the standard (hostile input), not a limit on a count of things (R1).
pub const MAX_DEPTH: u8 = 5;

pub type DeviceId = u64;

/// The high speed hub whose TT a low or full speed device uses (xHCI slot context TT Hub Slot ID / TT Port Number).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tt {
    pub hub: HcSlot,
    pub port: u8,
    /// The hub runs one TT per port (MTT in the slot context).
    pub multi: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// Root hub port number (1-based) the device or its topmost hub hangs on.
    pub root_port: u8,
    /// One nibble per hub tier; a port above 15 is written as 15.
    pub route: u32,
    /// Hubs between the root port and the device (0 = on a root port).
    pub depth: u8,
    pub speed: Speed,
    /// The hub (device id) and its port, if the device is not on a root port.
    pub parent: Option<(DeviceId, u8)>,
    pub tt: Option<Tt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceError {
    /// Below a hub that is already five tiers deep.
    TooDeep,
    BadPort,
}

/// What `Place::child` needs to know about the hub.
#[derive(Clone, Copy, Debug)]
pub struct HubAt {
    pub id: DeviceId,
    pub place: Place,
    pub slot: HcSlot,
    pub multi_tt: bool,
}

impl Place {
    pub fn root(port: u8, speed: Speed) -> Place {
        Place { root_port: port, route: 0, depth: 0, speed, parent: None, tt: None }
    }

    /// The place of a device on `port` of `hub`. Low/full speed devices below a high speed hub use that hub's TT
    /// at this port; below a full speed hub they keep the TT the hub itself uses (V1 `usb_tt`).
    pub fn child(hub: &HubAt, port: u8, speed: Speed) -> Result<Place, PlaceError> {
        if port == 0 {
            return Err(PlaceError::BadPort);
        }
        let d = hub.place.depth;
        if d >= MAX_DEPTH {
            return Err(PlaceError::TooDeep);
        }
        let route = hub.place.route | (port.min(15) as u32) << (4 * d as u32);
        let tt = if !speed.needs_tt() {
            None
        } else if hub.place.speed == Speed::High {
            Some(Tt { hub: hub.slot, port, multi: hub.multi_tt })
        } else {
            hub.place.tt
        };
        Ok(Place { root_port: hub.place.root_port, route, depth: d + 1, speed, parent: Some((hub.id, port)), tt })
    }

    /// "3" for root port 3, "3.1.4" for port 4 of a hub on port 1 of a hub on root port 3 (Linux-like path).
    pub fn path(&self) -> String {
        let mut s = format!("{}", self.root_port);
        for i in 0..self.depth.min(MAX_DEPTH) {
            s.push_str(&format!(".{}", (self.route >> (4 * i as u32)) & 0xF));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub(id: DeviceId, place: Place, slot: u64, multi_tt: bool) -> HubAt {
        HubAt { id, place, slot: HcSlot(slot), multi_tt }
    }

    #[test]
    fn route_strings_grow_one_nibble_per_tier() {
        let h1 = hub(1, Place::root(3, Speed::High), 1, false);
        let p1 = Place::child(&h1, 1, Speed::High).unwrap();
        assert_eq!((p1.route, p1.depth, p1.root_port, p1.parent), (0x1, 1, 3, Some((1, 1))));
        let h2 = hub(2, p1, 2, false);
        let p2 = Place::child(&h2, 4, Speed::High).unwrap();
        assert_eq!((p2.route, p2.depth), (0x41, 2));
        assert_eq!(p2.path(), "3.1.4");
        let big = Place::child(&h2, 20, Speed::High).unwrap();
        assert_eq!(big.route, 0xF1, "ports above 15 are written as 15");
        assert_eq!(Place::child(&h2, 0, Speed::High), Err(PlaceError::BadPort));
    }

    #[test]
    fn five_tiers_and_no_more() {
        let mut p = Place::root(1, Speed::High);
        for i in 0..5u64 {
            p = Place::child(&hub(i + 1, p, i + 1, false), 2, Speed::High).unwrap();
        }
        assert_eq!((p.depth, p.route), (5, 0x22222));
        assert_eq!(Place::child(&hub(9, p, 9, false), 1, Speed::Full), Err(PlaceError::TooDeep));
    }

    #[test]
    fn transaction_translators() {
        // LS keyboard on port 2 of a HS hub (slot 5, multi-TT): that hub's TT at port 2.
        let hs = hub(5, Place::root(1, Speed::High), 5, true);
        let kbd = Place::child(&hs, 2, Speed::Low).unwrap();
        assert_eq!(kbd.tt, Some(Tt { hub: HcSlot(5), port: 2, multi: true }));
        // A HS device below it needs none.
        assert_eq!(Place::child(&hs, 3, Speed::High).unwrap().tt, None);
        // A FS hub on that port, and a FS mouse below the FS hub: the HS hub's TT is inherited.
        let fs_hub_place = Place::child(&hs, 4, Speed::Full).unwrap();
        let fs = hub(6, fs_hub_place, 6, false);
        let mouse = Place::child(&fs, 1, Speed::Full).unwrap();
        assert_eq!(mouse.tt, Some(Tt { hub: HcSlot(5), port: 4, multi: true }));
        // A FS hub right on a root port (QEMU's usb-hub): the root port translates, no TT.
        let root_fs = hub(7, Place::root(2, Speed::Full), 7, false);
        assert_eq!(Place::child(&root_fs, 1, Speed::Full).unwrap().tt, None);
    }
}
