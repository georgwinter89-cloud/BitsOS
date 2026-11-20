//! What a UAC2 function holds. It differs from UAC1 (`uac.rs` in `usb-audio`): clocks are units of their own, a
//! terminal names its clock, and the format moves from the endpoint's rate table to `bmFormats` plus a RANGE request.
//! `parse` builds this from the bytes.

use alloc::vec::Vec;

/// bInterfaceProtocol of UAC2, 0x20; UAC1 is 0, which is `usb-audio`'s.
pub const PROTOCOL_UAC2: u8 = 0x20;

pub const ADC_2_00: u16 = 0x0200;
pub const FORMAT_TYPE_I: u8 = 1;
/// bmFormats bit 0.
pub const FORMAT_PCM: u32 = 1;

/// wTerminalType (USB Audio Terminal Types 2.0). What Linux's lsusb names on this device.
pub mod terminal {
    pub const USB_STREAMING: u16 = 0x0101;
    pub const MICROPHONE: u16 = 0x0201;
    pub const DESKTOP_MICROPHONE: u16 = 0x0202;
    pub const SPEAKER: u16 = 0x0301;
    pub const HEADPHONES: u16 = 0x0302;
    pub const SPDIF: u16 = 0x0605;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescError {
    /// bLength below 2, or a descriptor running past the end of the bytes.
    Truncated {
        at: usize,
    },
    /// Long enough to walk over, too short for what it says it is.
    TooShort {
        at: usize,
        subtype: u8,
        len: u8,
    },
    NotAConfiguration,
    /// wTotalLength says more than we were given.
    ConfigLength {
        total: u16,
        have: usize,
    },
    /// The header's wTotalLength says the unit tree is longer than what is there.
    ControlLength {
        total: u16,
        walked: usize,
    },
    /// An audio control interface that is not 2.00.
    AdcVersion(u16),
    NoControlInterface,
    /// A control interface whose unit tree has no header: nothing says how long it should be, or which version it is.
    NoControlHeader,
    /// One function is all this crate understands; a second one might be the jacks we would miss.
    SecondControlInterface(u8),
    /// A streaming alternate setting with an endpoint but no AS_GENERAL or FORMAT_TYPE, or without an endpoint.
    IncompleteAlt {
        iface: u8,
        alt: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockSource {
    pub id: u8,
    pub attributes: u8,
    pub controls: u8,
    pub assoc_terminal: u8,
}

impl ClockSource {
    /// `uac_v2v3_control_is_writeable(controls, CLOCK_FREQ)`, V:39: two bits per control, not one as in UAC1.
    pub const fn frequency_writable(&self) -> bool {
        self.controls >> ((crate::clock::CONTROL_SAM_FREQ - 1) * 2) & 2 != 0
    }

    pub const fn validity_readable(&self) -> bool {
        self.controls >> ((crate::clock::CONTROL_CLOCK_VALID - 1) * 2) & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputTerminal {
    pub id: u8,
    pub ty: u16,
    pub assoc: u8,
    pub clock: u8,
    pub channels: u8,
    pub channel_config: u32,
    pub controls: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputTerminal {
    pub id: u8,
    pub ty: u16,
    pub assoc: u8,
    pub source: u8,
    pub clock: u8,
    pub controls: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeatureUnit {
    pub id: u8,
    pub source: u8,
    /// bmaControls: [0] the master channel, then the logical channels. Two bits per control: bit 0 readable, bit 1
    /// writable; mute is bits 1:0, volume bits 3:2.
    pub controls: Vec<u32>,
}

impl FeatureUnit {
    pub fn mute_writable(&self) -> bool {
        self.controls.first().is_some_and(|c| c & 2 != 0)
    }

    /// Volume can be set on every logical channel. The headphones' unit here has mute only.
    pub fn volume_writable(&self) -> bool {
        self.controls.len() > 1 && self.controls[1..].iter().all(|c| c & 8 != 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    None,
    Async,
    Adaptive,
    Sync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub address: u8,
    pub attributes: u8,
    pub max_packet: u16,
    pub interval: u8,
}

impl Endpoint {
    pub const fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    pub const fn is_isochronous(&self) -> bool {
        self.attributes & 3 == 1
    }

    /// bmAttributes bits 5:4 = 1: it carries the sink's rate back, not samples.
    pub const fn is_feedback(&self) -> bool {
        (self.attributes >> 4) & 3 == 1
    }

    pub const fn sync(&self) -> Sync {
        match (self.attributes >> 2) & 3 {
            0 => Sync::None,
            1 => Sync::Async,
            2 => Sync::Adaptive,
            _ => Sync::Sync,
        }
    }

    /// A high-speed wMaxPacketSize: bits 10:0 are the size, bits 12:11 the extra transactions in each interval. 0x0BE0
    /// is 992 bytes twice, not 3040.
    pub const fn bytes_per_interval(&self) -> u32 {
        let size = (self.max_packet & 0x7FF) as u32;
        let extra = ((self.max_packet >> 11) & 3) as u32;
        size * (extra + 1)
    }

    /// High speed: 125 us times 2^(bInterval-1). `None` outside the 1..=16 the USB spec allows.
    pub const fn interval_us(&self) -> Option<u32> {
        match self.interval {
            1..=16 => Some(125 << (self.interval - 1)),
            _ => None,
        }
    }

    /// As `interval_us`, or for a full-speed device 1 ms times 2^(bInterval-1) (USB 2.0 9.6.6, isochronous).
    pub const fn interval_us_at(&self, full_speed: bool) -> Option<u32> {
        match (full_speed, self.interval) {
            (true, 1..=16) => Some(1000 << (self.interval - 1)),
            (true, _) => None,
            (false, _) => self.interval_us(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltSetting {
    pub iface: u8,
    pub alt: u8,
    /// The terminal this stream connects to: an input terminal for playback, an output terminal for capture.
    pub link: u8,
    pub format_type: u8,
    pub formats: u32,
    pub channels: u8,
    pub channel_config: u32,
    pub subslot: u8,
    pub bits: u8,
    pub endpoint: Endpoint,
    /// The explicit feedback endpoint of an asynchronous OUT stream, if the setting has one.
    pub feedback: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Function {
    pub control_iface: u8,
    pub clocks: Vec<ClockSource>,
    pub inputs: Vec<InputTerminal>,
    pub outputs: Vec<OutputTerminal>,
    pub features: Vec<FeatureUnit>,
    /// Ids of units this crate does not look inside (selector, mixer, effect, extension and so on). A path through one
    /// of them is refused by `plan`.
    pub other_units: Vec<u8>,
    pub alts: Vec<AltSetting>,
}

impl Function {
    pub fn clock(&self, id: u8) -> Option<&ClockSource> {
        self.clocks.iter().find(|c| c.id == id)
    }

    pub fn input(&self, id: u8) -> Option<&InputTerminal> {
        self.inputs.iter().find(|t| t.id == id)
    }

    pub fn output(&self, id: u8) -> Option<&OutputTerminal> {
        self.outputs.iter().find(|t| t.id == id)
    }

    pub fn feature(&self, id: u8) -> Option<&FeatureUnit> {
        self.features.iter().find(|f| f.id == id)
    }
}
