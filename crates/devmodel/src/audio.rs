//! `C: AudioDevice` (back end hda, SN). Streams of PCM frames with a hardware position for A/V sync.

use crate::DevError;

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

pub trait AudioDevice: Send + Sync {
    fn name(&self) -> &str;
    fn outputs(&self) -> alloc::vec::Vec<Output>;
    fn formats(&self) -> alloc::vec::Vec<PcmFormat>;
    /// Starts a playback stream into a ring of `ring_frames`.
    fn open_stream(&self, out: Output, fmt: PcmFormat, ring_frames: u32) -> Result<u32, DevError>;
    /// Writes frames at the stream's write position; returns frames accepted.
    fn write(&self, stream: u32, data: &[u8]) -> Result<usize, DevError>;
    /// Frames the hardware has played (for delay / A/V sync).
    fn position(&self, stream: u32) -> u64;
    fn close_stream(&self, stream: u32);
    /// Jack plugged? (unsolicited events later)
    fn jack_present(&self, out: Output) -> Option<bool>;
}
