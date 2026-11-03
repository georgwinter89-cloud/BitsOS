//! The PCM format and output kinds the sound path shares (hda, snd, usb-audio; SN). There is no device trait: snd
//! drives hda through `hda::controllers()` and usb-audio through its own API (C1: the unused `AudioDevice` trait
//! was removed).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u8,
    pub bits: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    Jack,
    Hdmi,
}
