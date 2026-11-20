//! Who plays on a playback interface: the streams open on it now (a stream the mixer retires stays open until it has
//! played out) and the tone check's claim. The claim is taken only while nothing is open there; while it is held,
//! every other open of that interface is `Busy`, so two streams never share one isochronous endpoint. Opens refused
//! meanwhile are remembered: the mixer passed over that output, and `dev` counts outputs as changed when the claim
//! ends, so it picks again.

use alloc::vec::Vec;
use usb_core::UsbError;

#[derive(Debug, Default)]
pub struct Plays {
    open: Vec<u8>,
    tone: Option<u8>,
    refused: bool,
}

impl Plays {
    pub const fn new() -> Plays {
        Plays { open: Vec::new(), tone: None, refused: false }
    }

    /// A stream opens on `iface`; `by_tone` is the claim holder's own stream.
    pub fn open(&mut self, iface: u8, by_tone: bool) -> Result<(), UsbError> {
        if !by_tone && self.tone == Some(iface) {
            self.refused = true;
            return Err(UsbError::Busy);
        }
        self.open.push(iface);
        Ok(())
    }

    pub fn close(&mut self, iface: u8) {
        if let Some(i) = self.open.iter().position(|&x| x == iface) {
            self.open.swap_remove(i);
        }
    }

    pub fn claim_tone(&mut self, iface: u8) -> Result<(), UsbError> {
        if self.tone.is_some() || self.open.contains(&iface) {
            return Err(UsbError::Busy);
        }
        self.tone = Some(iface);
        self.refused = false;
        Ok(())
    }

    /// Ends the claim; true when an open was refused while it was held.
    pub fn release_tone(&mut self) -> bool {
        self.tone = None;
        core::mem::take(&mut self.refused)
    }
}
