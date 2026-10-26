//! USB core (owner: US): what host controller drivers and class drivers share. Descriptors and requests
//! (`desc`, `setup`), speeds and places (`speed`, `place`: route string, TT), the host controller interface
//! (`host`), enumeration (`enumerate`), the port logic shared by root ports and hub ports (`ports`: debounce,
//! reset, two tries, detach), the hub class driver (`hub`), and the device registry with class-driver matching
//! (`registry`, `usb_driver!`), and the transfer API for class drivers (`transfer`: buffers, queued bulk and
//! interrupt transfers with completion and timeout, interrupt IN pipes; `iso`: isochronous pipes). No controller specifics here: xHCI lives in `crates/drivers/xhci`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod alt;
pub mod desc;
pub mod device;
pub mod enumerate;
pub mod host;
pub mod hub;
pub mod iso;
pub mod place;
pub mod ports;
pub mod registry;
pub mod setup;
pub mod speed;
pub mod transfer;
#[cfg(test)]
mod tests_timed;

#[doc(hidden)]
pub use kreg as __kreg;

pub use desc::{Configuration, DeviceDescriptor, Endpoint, EpKind, HubDescriptor, Interface, SsCompanion};
pub use device::Device;
pub use iso::{IsoDone, IsoOpts, IsoPacket, IsoPipe, IsoRequest, IsoResult, IsoUrb};
pub use host::{Data, HcSlot, Host, HubParams, PipeEvent, Sink, TimedSink, UsbError};
pub use place::{DeviceId, Place, Tt};
pub use ports::{PortSet, PortStatus, Ports};
pub use registry::{UsbDriver, UsbMatch};
pub use setup::Setup;
pub use speed::Speed;
pub use transfer::{Buffer, OnDone, PipeHandle, PipeOpts, Request, Transfer, Urb, XferMem, XferResult};
