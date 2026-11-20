//! USB Audio Class 1 (owner SN, item SN-8b; V2-LANES SN-8, spec usb.md 7; Linux `sound/usb/`): headsets and
//! dongles like Georg's JBL Quantum 800 (plan/hw/jbl-quantum-800-lsusb.txt). `uac` parses the class
//! descriptors, `plan` picks the 48 kHz PCM settings, `req` makes the class requests (sampling frequency,
//! feature unit mute/volume), `stream` moves frames over isochronous pipes (SN-8a), `dev` binds AudioControl
//! interfaces and gives the sound side (`snd`'s USB back end) its outputs and inputs. UAC2 (GP-SOUND): `uac2dev`
//! binds an AudioControl interface with protocol 0x20 on top of the pure crate `uac2` (the Gaming-PC's board jacks);
//! `check` is the tone check of the hardware day (`snd ton`). US's test key `usb_iso_test=on` leaves audio devices
//! unbound, UAC1 and UAC2 alike.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod check;
pub mod dev;
mod ktests;
pub mod plan;
pub mod plays;
pub mod req;
pub mod stream;
pub mod uac;
pub mod uac2dev;
pub mod words;

#[cfg(test)]
mod jbl;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_stream;
#[cfg(test)]
mod tests_uac2;
#[cfg(test)]
mod tests_check;

use usb_core::{UsbDriver, UsbMatch};

pub use dev::{CaptureHandle, OutputInfo, generation, hw_mute, inputs, on_change, open_capture, open_playback, outputs, set_hw_mute};
pub use ktests::settle as ktest_settle;
pub use stream::{Capture, Playback};

/// AudioControl interfaces (class 1, subclass 1); the driver claims the streaming interfaces they list. Protocol 0
/// is UAC1 (`dev::probe`, the JBL); protocol 0x20 is UAC2 (`uac2dev::probe`, the Gaming-PC's board jacks). One driver
/// for both, so the USB lines of the report page name the board device a sound device like the headset.
static MATCHES: [UsbMatch; 2] = [
    UsbMatch::Interface { class: 1, subclass: Some(1), protocol: Some(0) },
    UsbMatch::Interface { class: 1, subclass: Some(1), protocol: Some(uac2::desc::PROTOCOL_UAC2) },
];

fn probe(dev: &alloc::sync::Arc<usb_core::Device>, f: &usb_core::Interface) -> Result<(), alloc::string::String> {
    if f.protocol == uac2::desc::PROTOCOL_UAC2 { uac2dev::probe(dev, f) } else { dev::probe(dev, f) }
}

fn disconnect(dev: &alloc::sync::Arc<usb_core::Device>, iface: u8) {
    // `uac2dev::disconnect` forgets its jacks (none for a UAC1 function) and then unbinds as `dev::disconnect` does.
    uac2dev::disconnect(dev, iface)
}

pub static DRIVER: UsbDriver = UsbDriver { name: "usb-audio", matches: &MATCHES, probe, disconnect };

usb_core::usb_driver!("usb-audio", DRIVER);
