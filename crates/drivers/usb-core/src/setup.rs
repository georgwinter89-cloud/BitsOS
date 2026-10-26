//! Setup packets (USB 2.0 §9.3) and the standard and hub class requests the core sends (§9.4, §11.24; USB 3.2
//! §10.16 for SuperSpeed hubs).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

/// Descriptor types.
pub mod dt {
    pub const DEVICE: u8 = 1;
    pub const CONFIGURATION: u8 = 2;
    pub const STRING: u8 = 3;
    pub const INTERFACE: u8 = 4;
    pub const ENDPOINT: u8 = 5;
    pub const HID: u8 = 0x21;
    pub const HUB: u8 = 0x29;
    pub const SS_HUB: u8 = 0x2A;
    pub const SS_ENDPOINT_COMPANION: u8 = 0x30;
}

/// Hub and port feature selectors (USB 2.0 Table 11-17, USB 3.2 Table 10-9).
pub mod feature {
    pub const C_HUB_LOCAL_POWER: u16 = 0;
    pub const C_HUB_OVER_CURRENT: u16 = 1;
    pub const PORT_ENABLE: u16 = 1;
    pub const PORT_RESET: u16 = 4;
    pub const PORT_LINK_STATE: u16 = 5;
    pub const PORT_POWER: u16 = 8;
    pub const C_PORT_CONNECTION: u16 = 16;
    pub const C_PORT_ENABLE: u16 = 17;
    pub const C_PORT_SUSPEND: u16 = 18;
    pub const C_PORT_OVER_CURRENT: u16 = 19;
    pub const C_PORT_RESET: u16 = 20;
    pub const C_PORT_LINK_STATE: u16 = 25;
    pub const C_PORT_CONFIG_ERROR: u16 = 26;
    pub const BH_PORT_RESET: u16 = 28;
    pub const C_BH_PORT_RESET: u16 = 29;
    pub const ENDPOINT_HALT: u16 = 0;
}

/// SuperSpeed link state SS.Disabled (USB 3.2 Table 10-13), used to switch a SuperSpeed hub port off.
pub const LINK_SS_DISABLED: u16 = 4;

impl Setup {
    /// The eight bytes as the controller wants them in a Setup Stage TRB (little endian).
    pub fn to_u64(&self) -> u64 {
        self.request_type as u64
            | (self.request as u64) << 8
            | (self.value as u64) << 16
            | (self.index as u64) << 32
            | (self.length as u64) << 48
    }
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }

    pub const fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Setup {
        Setup { request_type, request, value, index, length }
    }
    pub fn get_descriptor(ty: u8, index: u8, length: u16) -> Setup {
        Setup::new(0x80, 6, (ty as u16) << 8 | index as u16, 0, length)
    }
    pub fn set_configuration(value: u8) -> Setup {
        Setup::new(0x00, 9, value as u16, 0, 0)
    }
    pub fn set_interface(interface: u8, alt: u8) -> Setup {
        Setup::new(0x01, 11, alt as u16, interface as u16, 0)
    }
    pub fn clear_halt(endpoint: u8) -> Setup {
        Setup::new(0x02, 1, feature::ENDPOINT_HALT, endpoint as u16, 0)
    }
    pub fn hub_descriptor(superspeed: bool, length: u16) -> Setup {
        let ty = if superspeed { dt::SS_HUB } else { dt::HUB };
        Setup::new(0xA0, 6, (ty as u16) << 8, 0, length)
    }
    pub fn hub_status() -> Setup {
        Setup::new(0xA0, 0, 0, 0, 4)
    }
    pub fn port_status(port: u8) -> Setup {
        Setup::new(0xA3, 0, 0, port as u16, 4)
    }
    /// SET_FEATURE of a port; `selector` goes into wIndex's high byte (link state, test mode).
    pub fn set_port_feature(port: u8, feature: u16, selector: u8) -> Setup {
        Setup::new(0x23, 3, feature, port as u16 | (selector as u16) << 8, 0)
    }
    pub fn clear_port_feature(port: u8, feature: u16) -> Setup {
        Setup::new(0x23, 1, feature, port as u16, 0)
    }
    pub fn clear_hub_feature(feature: u16) -> Setup {
        Setup::new(0x20, 1, feature, 0, 0)
    }
    /// SET_HUB_DEPTH (USB 3.2 §10.16.2.9): 0 for a hub on a root port.
    pub fn set_hub_depth(depth: u8) -> Setup {
        Setup::new(0x20, 12, depth as u16, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_as_on_the_wire() {
        assert_eq!(Setup::get_descriptor(dt::DEVICE, 0, 8).to_u64(), 0x0008_0000_0100_0680);
        assert_eq!(Setup::get_descriptor(dt::CONFIGURATION, 0, 9).to_u64(), 0x0009_0000_0200_0680);
        assert_eq!(Setup::port_status(3).to_u64(), 0x0004_0003_0000_00A3);
        assert_eq!(Setup::set_port_feature(2, feature::PORT_RESET, 0).to_u64(), 0x0000_0002_0004_0323);
        assert_eq!(Setup::set_port_feature(2, feature::PORT_LINK_STATE, 4).index, 0x0402);
        assert_eq!(Setup::hub_descriptor(false, 9).to_u64(), 0x0009_0000_2900_06A0);
        assert_eq!(Setup::hub_descriptor(true, 12).value, 0x2A00);
        assert_eq!(Setup::set_hub_depth(1).to_u64(), 0x0000_0000_0001_0C20);
        assert!(Setup::hub_status().is_in() && !Setup::set_configuration(1).is_in());
    }
}
