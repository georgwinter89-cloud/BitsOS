//! Decoding of capability parameters and the pin default configuration (HDA spec 7.3.4,
//! Linux `hda_verbs.h`).

/// Widget type, bits 23:20 of the audio widget capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetType {
    Output,
    Input,
    Mixer,
    Selector,
    Pin,
    Power,
    VolumeKnob,
    Beep,
    Vendor,
    Reserved(u8),
}

/// Audio widget capabilities (parameter 0x09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WidgetCaps(pub u32);

impl WidgetCaps {
    pub fn kind(self) -> WidgetType {
        match (self.0 >> 20) & 0xF {
            0 => WidgetType::Output,
            1 => WidgetType::Input,
            2 => WidgetType::Mixer,
            3 => WidgetType::Selector,
            4 => WidgetType::Pin,
            5 => WidgetType::Power,
            6 => WidgetType::VolumeKnob,
            7 => WidgetType::Beep,
            0xF => WidgetType::Vendor,
            n => WidgetType::Reserved(n as u8),
        }
    }
    fn bit(self, n: u32) -> bool {
        self.0 & (1 << n) != 0
    }
    pub fn stereo(self) -> bool {
        self.bit(0)
    }
    pub fn in_amp(self) -> bool {
        self.bit(1)
    }
    pub fn out_amp(self) -> bool {
        self.bit(2)
    }
    /// The widget has its own amp parameters (else the function group's apply).
    pub fn amp_override(self) -> bool {
        self.bit(3)
    }
    /// The converter has its own PCM/stream parameters (else the function group's apply).
    pub fn format_override(self) -> bool {
        self.bit(4)
    }
    pub fn unsol(self) -> bool {
        self.bit(7)
    }
    pub fn conn_list(self) -> bool {
        self.bit(8)
    }
    pub fn digital(self) -> bool {
        self.bit(9)
    }
    pub fn power_control(self) -> bool {
        self.bit(10)
    }
    /// Channel count (bits 15:13 extend the stereo bit).
    pub fn channels(self) -> u32 {
        (((self.0 >> 13) & 7) << 1 | (self.0 & 1)) + 1
    }
}

/// Pin capabilities (parameter 0x0C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinCaps(pub u32);

impl PinCaps {
    fn bit(self, n: u32) -> bool {
        self.0 & (1 << n) != 0
    }
    /// Presence detect needs an "execute" (verb 709) before reading the sense (F09).
    pub fn trigger_required(self) -> bool {
        self.bit(1)
    }
    pub fn presence_detect(self) -> bool {
        self.bit(2)
    }
    pub fn hp_drive(self) -> bool {
        self.bit(3)
    }
    pub fn output(self) -> bool {
        self.bit(4)
    }
    pub fn input(self) -> bool {
        self.bit(5)
    }
    pub fn hdmi(self) -> bool {
        self.bit(7)
    }
    pub fn eapd(self) -> bool {
        self.bit(16)
    }
    pub fn dp(self) -> bool {
        self.bit(24)
    }
    pub fn hbr(self) -> bool {
        self.bit(27)
    }
    pub fn digital(self) -> bool {
        self.hdmi() || self.dp()
    }
}

/// Amplifier capabilities (parameters 0x0D and 0x12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AmpCaps(pub u32);

impl AmpCaps {
    /// The step that is 0 dB.
    pub fn offset(self) -> u8 {
        (self.0 & 0x7F) as u8
    }
    /// The highest step.
    pub fn steps(self) -> u8 {
        ((self.0 >> 8) & 0x7F) as u8
    }
    /// Step size in 0.25 dB units, minus one.
    pub fn step_size(self) -> u8 {
        ((self.0 >> 16) & 0x7F) as u8
    }
    pub fn mute(self) -> bool {
        self.0 & (1 << 31) != 0
    }
    /// The gain for 0 dB, never above the top step (V1 `amp_gain`).
    pub fn zero_db_gain(self) -> u8 {
        self.offset().min(self.steps())
    }
}

/// Supported PCM sizes and rates (parameter 0x0A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PcmCaps(pub u32);

impl PcmCaps {
    pub fn rate_44k1(self) -> bool {
        self.0 & (1 << 5) != 0
    }
    pub fn rate_48k(self) -> bool {
        self.0 & (1 << 6) != 0
    }
    pub fn bits_16(self) -> bool {
        self.0 & (1 << 17) != 0
    }
    /// 48 kHz with 16 bits, the format V1 plays (`SND_FORMAT` 0x0011).
    pub fn takes_48k_16(self) -> bool {
        self.rate_48k() && self.bits_16()
    }
}

/// Port connectivity, bits 31:30 of the default configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectivity {
    Jack,
    None,
    Fixed,
    Both,
}

/// Default device, bits 23:20 of the default configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    LineOut,
    Speaker,
    Headphone,
    Cd,
    SpdifOut,
    DigitalOtherOut,
    ModemLine,
    ModemHandset,
    LineIn,
    Aux,
    Mic,
    Telephony,
    SpdifIn,
    DigitalOtherIn,
    Reserved,
    Other,
}

/// A pin's default configuration (verb F1C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PinConfig(pub u32);

impl PinConfig {
    pub fn connectivity(self) -> Connectivity {
        match self.0 >> 30 {
            0 => Connectivity::Jack,
            1 => Connectivity::None,
            2 => Connectivity::Fixed,
            _ => Connectivity::Both,
        }
    }
    /// Location, bits 29:24 (0x18 = internal HDMI).
    pub fn location(self) -> u8 {
        ((self.0 >> 24) & 0x3F) as u8
    }
    pub fn device(self) -> Device {
        const ALL: [Device; 16] = [
            Device::LineOut,
            Device::Speaker,
            Device::Headphone,
            Device::Cd,
            Device::SpdifOut,
            Device::DigitalOtherOut,
            Device::ModemLine,
            Device::ModemHandset,
            Device::LineIn,
            Device::Aux,
            Device::Mic,
            Device::Telephony,
            Device::SpdifIn,
            Device::DigitalOtherIn,
            Device::Reserved,
            Device::Other,
        ];
        ALL[((self.0 >> 20) & 0xF) as usize]
    }
    pub fn conn_type(self) -> u8 {
        ((self.0 >> 16) & 0xF) as u8
    }
    pub fn color(self) -> u8 {
        ((self.0 >> 12) & 0xF) as u8
    }
    pub fn misc(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
    }
    /// Misc bit 0: the jack has no presence detect, whatever the pin caps say.
    pub fn no_presence_detect(self) -> bool {
        self.misc() & 1 != 0
    }
    pub fn association(self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }
    pub fn sequence(self) -> u8 {
        (self.0 & 0xF) as u8
    }
}
