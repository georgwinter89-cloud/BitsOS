//! USB Audio Class 1 (owner SN, item SN-8b; V2-LANES SN-8, spec usb.md 7; Linux `sound/usb/`): headsets and
//! dongles like Georg's JBL Quantum 800 (plan/hw/jbl-quantum-800-lsusb.txt). `uac` parses the class
//! descriptors, `plan` picks the 48 kHz PCM settings, `req` makes the class requests (sampling frequency,
//! feature unit mute/volume), `stream` moves frames over isochronous pipes (SN-8a), `dev` binds AudioControl
//! interfaces and gives the sound side (`snd`'s USB back end) its outputs and inputs. UAC2 is a later item.
//! CONFIG `usb_audio=off` (or US's `usb_iso_test=on`) leaves audio devices unbound.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod dev;
mod ktests;
pub mod plan;
pub mod req;
pub mod stream;
pub mod uac;

#[cfg(test)]
mod jbl;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_stream;

use usb_core::{UsbDriver, UsbMatch};

pub use dev::{CaptureHandle, OutputInfo, generation, hw_mute, inputs, on_change, open_capture, open_playback, outputs, set_hw_mute};
pub use ktests::settle as ktest_settle;
pub use stream::{Capture, Playback};

/// AudioControl interfaces (class 1, subclass 1); the driver claims the streaming interfaces they list.
static MATCHES: [UsbMatch; 1] = [UsbMatch::Interface { class: 1, subclass: Some(1), protocol: Some(0) }];

pub static DRIVER: UsbDriver = UsbDriver { name: "usb-audio", matches: &MATCHES, probe: dev::probe, disconnect: dev::disconnect };

usb_core::usb_driver!("usb-audio", DRIVER);
