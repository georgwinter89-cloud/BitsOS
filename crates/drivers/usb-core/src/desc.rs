//! Descriptor parsing (USB 2.0 §9.6, §11.23.2.1; USB 3.2 §9.6.7, §10.15.2.1). Everything comes from a device, so
//! every length is checked and a broken descriptor ends the walk instead of a panic.

use crate::setup::dt;
use crate::speed::Speed;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescError {
    Short,
    WrongType,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceDescriptor {
    pub usb: u16,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub device: u16,
    pub num_configurations: u8,
}

impl DeviceDescriptor {
    pub fn parse(b: &[u8]) -> Result<Self, DescError> {
        if b.len() < 18 || (b[0] as usize) < 18 {
            return Err(DescError::Short);
        }
        if b[1] != dt::DEVICE {
            return Err(DescError::WrongType);
        }
        Ok(DeviceDescriptor {
            usb: le16(b, 2),
            class: b[4],
            subclass: b[5],
            protocol: b[6],
            max_packet0: b[7],
            vendor: le16(b, 8),
            product: le16(b, 10),
            device: le16(b, 12),
            num_configurations: b[17],
        })
    }
}

/// Endpoint 0's packet size from the first eight bytes of the device descriptor, if it is one the speed allows
/// (USB 2.0 §5.5.3, USB 3.2 §9.6.1). `None` keeps the speed's guess (Linux `hub_port_init` does the same).
pub fn mps0_from_first8(b: &[u8], speed: Speed) -> Option<u16> {
    if b.len() < 8 || b[1] != dt::DEVICE {
        return None;
    }
    let v = b[7];
    match speed {
        // SuperSpeed gives an exponent; 9 (512) is the only valid one.
        Speed::Super | Speed::SuperPlus => (v == 9).then_some(512),
        Speed::High => (v == 64).then_some(64),
        Speed::Low => (v == 8).then_some(8),
        Speed::Full => matches!(v, 8 | 16 | 32 | 64).then_some(v as u16),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpKind {
    Control,
    Isoch,
    Bulk,
    Interrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SsCompanion {
    pub max_burst: u8,
    pub attributes: u8,
    pub bytes_per_interval: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    /// wMaxPacketSize as sent (bits 12:11 are the high-bandwidth multiplier of high speed periodic endpoints).
    pub max_packet_raw: u16,
    pub interval: u8,
    pub companion: Option<SsCompanion>,
}

impl Endpoint {
    pub fn number(&self) -> u8 {
        self.address & 0x0F
    }
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
    pub fn kind(&self) -> EpKind {
        match self.attributes & 3 {
            0 => EpKind::Control,
            1 => EpKind::Isoch,
            2 => EpKind::Bulk,
            _ => EpKind::Interrupt,
        }
    }
    pub fn max_packet(&self) -> u16 {
        self.max_packet_raw & 0x7FF
    }
    /// Additional transactions per microframe of a high speed periodic endpoint (0..2).
    pub fn hs_mult(&self) -> u8 {
        ((self.max_packet_raw >> 11) & 3) as u8
    }
    /// The xHCI Device Context Index: 2 * number + direction (control endpoints use the IN index).
    pub fn dci(&self) -> u8 {
        2 * self.number() + (self.is_in() || self.kind() == EpKind::Control) as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub number: u8,
    pub alt: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<Endpoint>,
    /// Class-specific descriptors between this interface and its endpoints (e.g. HID), raw.
    pub extra: Vec<u8>,
    /// Class-specific descriptors after an endpoint, raw, by endpoint address (USB audio's CS_ENDPOINT with the
    /// sampling-frequency control and lock delay, UAC1 4.6.1.2).
    pub ep_extra: Vec<(u8, Vec<u8>)>,
}

impl Interface {
    /// The class-specific descriptors that follow endpoint `address` (empty if none).
    pub fn endpoint_extra(&self, address: u8) -> &[u8] {
        self.ep_extra.iter().find(|x| x.0 == address).map_or(&[], |x| &x.1)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub value: u8,
    pub attributes: u8,
    pub max_power: u8,
    pub total_length: u16,
    pub interfaces: Vec<Interface>,
    /// A descriptor ran past the end or had a length below 2; the walk stopped there.
    pub truncated: bool,
}

impl Configuration {
    pub fn parse(b: &[u8]) -> Result<Self, DescError> {
        if b.len() < 9 || (b[0] as usize) < 9 {
            return Err(DescError::Short);
        }
        if b[1] != dt::CONFIGURATION {
            return Err(DescError::WrongType);
        }
        let mut c = Configuration {
            value: b[5],
            attributes: b[7],
            max_power: b[8],
            total_length: le16(b, 2),
            interfaces: Vec::new(),
            truncated: false,
        };
        let end = b.len().min(c.total_length as usize);
        let mut i = b[0] as usize;
        while i < end {
            let len = b[i] as usize;
            if len < 2 || i + len > end {
                c.truncated = true;
                break;
            }
            let d = &b[i..i + len];
            match d[1] {
                dt::INTERFACE if len >= 9 => c.interfaces.push(Interface {
                    number: d[2],
                    alt: d[3],
                    class: d[5],
                    subclass: d[6],
                    protocol: d[7],
                    endpoints: Vec::new(),
                    extra: Vec::new(),
                    ep_extra: Vec::new(),
                }),
                dt::ENDPOINT if len >= 7 => {
                    if let Some(f) = c.interfaces.last_mut() {
                        f.endpoints.push(Endpoint {
                            address: d[2],
                            attributes: d[3],
                            max_packet_raw: le16(d, 4),
                            interval: d[6],
                            companion: None,
                        });
                    }
                }
                dt::SS_ENDPOINT_COMPANION if len >= 6 => {
                    if let Some(e) = c.interfaces.last_mut().and_then(|f| f.endpoints.last_mut()) {
                        e.companion = Some(SsCompanion { max_burst: d[2], attributes: d[3], bytes_per_interval: le16(d, 4) });
                    }
                }
                _ => {
                    if let Some(f) = c.interfaces.last_mut() {
                        match f.endpoints.last().map(|e| e.address) {
                            None => f.extra.extend_from_slice(d),
                            Some(a) => match f.ep_extra.iter_mut().find(|x| x.0 == a) {
                                Some(x) => x.1.extend_from_slice(d),
                                None => f.ep_extra.push((a, d.to_vec())),
                            },
                        }
                    }
                }
            }
            i += len;
        }
        Ok(c)
    }

    /// The interfaces in their default alternate setting (0), the ones a configuration starts with.
    pub fn defaults(&self) -> impl Iterator<Item = &Interface> {
        self.interfaces.iter().filter(|f| f.alt == 0)
    }
    pub fn interface(&self, number: u8, alt: u8) -> Option<&Interface> {
        self.interfaces.iter().find(|f| f.number == number && f.alt == alt)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubDescriptor {
    pub ports: u8,
    pub characteristics: u16,
    /// bPwrOn2PwrGood in ms (the descriptor gives units of 2 ms).
    pub power_on_ms: u32,
    pub superspeed: bool,
}

impl HubDescriptor {
    pub fn parse(b: &[u8], superspeed: bool) -> Result<Self, DescError> {
        if b.len() < 7 || (b[0] as usize) < 7 {
            return Err(DescError::Short);
        }
        if b[1] != if superspeed { dt::SS_HUB } else { dt::HUB } {
            return Err(DescError::WrongType);
        }
        Ok(HubDescriptor { ports: b[2], characteristics: le16(b, 3), power_on_ms: b[5] as u32 * 2, superspeed })
    }
    /// TT think time of a high speed hub (wHubCharacteristics bits 6:5): 8, 16, 24 or 32 FS bit times as 0..3.
    pub fn tt_think_time(&self) -> u8 {
        ((self.characteristics >> 5) & 3) as u8
    }
    /// Bytes of a status change report: one bit for the hub and one per port.
    pub fn change_report_len(&self) -> usize {
        (self.ports as usize + 1).div_ceil(8)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn audio_endpoint_extras_are_kept() {
        // UAC1 streaming alternate setting: AS_GENERAL + FORMAT_TYPE before the endpoint, then the 9-byte
        // isochronous endpoint (JBL Quantum 800 game stream) and its CS_ENDPOINT (sampling frequency control).
        let mut b = vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50];
        b.extend_from_slice(&[9, 4, 1, 1, 1, 1, 2, 0, 0]);
        b.extend_from_slice(&[7, 0x24, 1, 1, 1, 1, 0]);
        b.extend_from_slice(&[11, 0x24, 2, 1, 2, 2, 16, 1, 0x80, 0xBB, 0]);
        b.extend_from_slice(&[9, 5, 0x02, 0x0D, 0x20, 0x01, 1, 0, 0]);
        b.extend_from_slice(&[7, 0x25, 1, 0x01, 0, 0, 0]);
        let n = b.len() as u16;
        b[2..4].copy_from_slice(&n.to_le_bytes());
        let c = Configuration::parse(&b).unwrap();
        let f = &c.interfaces[0];
        assert_eq!((f.extra.len(), f.endpoints[0].max_packet(), f.endpoints[0].interval), (18, 288, 1));
        assert_eq!(f.endpoint_extra(0x02), [7, 0x25, 1, 0x01, 0, 0, 0]);
        assert!(f.endpoint_extra(0x81).is_empty());
    }

    use super::*;
    use alloc::vec;

    /// Shaped like QEMU's usb-kbd: full speed, endpoint 0 packet size 8, 0627:0001.
    pub(crate) const KBD_DEVICE: [u8; 18] = [18, 1, 0x00, 0x02, 0, 0, 0, 8, 0x27, 0x06, 0x01, 0x00, 0, 0, 1, 4, 5, 1];

    #[test]
    fn device_descriptor_and_mps0() {
        let d = DeviceDescriptor::parse(&KBD_DEVICE).unwrap();
        assert_eq!((d.vendor, d.product, d.max_packet0, d.num_configurations), (0x0627, 0x0001, 8, 1));
        assert_eq!(mps0_from_first8(&KBD_DEVICE[..8], Speed::Full), Some(8));
        assert_eq!(mps0_from_first8(&KBD_DEVICE[..7], Speed::Full), None, "short");
        let mut b = KBD_DEVICE;
        b[7] = 9;
        assert_eq!(mps0_from_first8(&b, Speed::Super), Some(512));
        assert_eq!(mps0_from_first8(&b, Speed::Full), None, "9 is no full speed size");
        b[7] = 32;
        assert_eq!(mps0_from_first8(&b, Speed::High), None, "high speed must be 64");
        assert_eq!(DeviceDescriptor::parse(&KBD_DEVICE[..17]), Err(DescError::Short));
        b[1] = 2;
        assert_eq!(DeviceDescriptor::parse(&b), Err(DescError::WrongType));
    }

    #[test]
    fn configuration_walk() {
        // config, iface 0 (HID kbd) + HID desc + EP 0x81 int, iface 1 alt 0 (bulk 0x82 SS + companion), iface 1 alt 1.
        let b = vec![
            9, 2, 65, 0, 2, 1, 0, 0xA0, 50, //
            9, 4, 0, 0, 1, 3, 1, 1, 0, //
            9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, //
            7, 5, 0x81, 3, 8, 0, 10, //
            9, 4, 1, 0, 1, 8, 6, 0x50, 0, //
            7, 5, 0x82, 2, 0x00, 0x04, 0, //
            6, 0x30, 15, 0, 0, 0, //
            9, 4, 1, 1, 0, 8, 6, 0x62, 0, //
        ];
        assert_eq!(b.len(), 65);
        let c = Configuration::parse(&b).unwrap();
        assert!(!c.truncated);
        assert_eq!((c.value, c.interfaces.len()), (1, 3));
        let k = &c.interfaces[0];
        assert_eq!((k.class, k.subclass, k.protocol, k.extra.len()), (3, 1, 1, 9));
        assert_eq!(k.endpoints[0].kind(), EpKind::Interrupt);
        assert_eq!((k.endpoints[0].dci(), k.endpoints[0].interval), (3, 10));
        let s = &c.interfaces[1].endpoints[0];
        assert_eq!((s.kind(), s.is_in(), s.max_packet(), s.dci()), (EpKind::Bulk, true, 1024, 5));
        assert_eq!(s.companion.unwrap().max_burst, 15);
        assert_eq!(c.defaults().count(), 2);
        assert_eq!(c.interface(1, 1).unwrap().protocol, 0x62);
    }

    #[test]
    fn hostile_configurations_end_the_walk() {
        // wTotalLength larger than what came: parse what is whole.
        let b = [9, 2, 200, 0, 1, 1, 0, 0x80, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 7, 5, 0x81];
        let c = Configuration::parse(&b).unwrap();
        assert!(c.truncated);
        assert_eq!(c.interfaces[0].endpoints.len(), 0);
        // A zero-length descriptor would loop forever: it ends the walk.
        let b = [9, 2, 20, 0, 1, 1, 0, 0x80, 50, 0, 4, 0, 0, 1, 3, 1, 1, 0, 0, 0];
        assert!(Configuration::parse(&b).unwrap().truncated);
        // An endpoint before any interface is ignored, not a panic.
        let b = [9, 2, 16, 0, 1, 1, 0, 0x80, 50, 7, 5, 0x81, 3, 8, 0, 10];
        assert!(Configuration::parse(&b).unwrap().interfaces.is_empty());
        assert_eq!(Configuration::parse(&[9, 2, 9]), Err(DescError::Short));
        // Every prefix and every single-byte corruption parses or fails, never panics.
        let good = [9u8, 2, 34, 0, 1, 1, 0, 0xA0, 50, 9, 4, 0, 0, 1, 3, 1, 1, 0, 9, 0x21, 0x11, 1, 0, 1, 0x22, 63, 0, 7, 5, 0x81, 3, 8, 0, 10];
        for n in 0..good.len() {
            let _ = Configuration::parse(&good[..n]);
        }
        for i in 0..good.len() {
            for v in [0u8, 1, 2, 7, 9, 0xFF] {
                let mut b = good;
                b[i] = v;
                let _ = Configuration::parse(&b);
            }
        }
    }

    #[test]
    fn hub_descriptors() {
        // QEMU usb-hub: 8 ports, characteristics 0x0A, power on 1 (2 ms).
        let h = HubDescriptor::parse(&[10, 0x29, 8, 0x0A, 0, 1, 0, 0, 0xFF, 0xFF], false).unwrap();
        assert_eq!((h.ports, h.power_on_ms, h.change_report_len()), (8, 2, 2));
        let hs = HubDescriptor::parse(&[9, 0x29, 4, 0x40, 0, 50, 100, 0, 0xFF], false).unwrap();
        assert_eq!((hs.tt_think_time(), hs.power_on_ms, hs.change_report_len()), (2, 100, 1));
        let ss = HubDescriptor::parse(&[12, 0x2A, 4, 0, 0, 50, 0, 0, 0, 0, 0, 0], true).unwrap();
        assert!(ss.superspeed);
        assert_eq!(HubDescriptor::parse(&[12, 0x29, 4, 0, 0, 50, 0], true), Err(DescError::WrongType));
        assert_eq!(HubDescriptor::parse(&[6, 0x29, 4, 0, 0, 50], false), Err(DescError::Short));
    }
}
