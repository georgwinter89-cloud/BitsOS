//! xHCI host controller. Owner: US. US-1: bring-up of every xHCI controller (the Mini-PC has four), the command
//! ring with its abort path, the event ring (multi-segment through the ERST), an event thread per controller,
//! health watch and `Quiesce`. US-2: root ports, device slots and contexts, control transfers, the minimal
//! interrupt pipe hubs need, and a hotplug worker per controller; the controller is a `usb_core::Host`, so
//! enumeration, hubs and class-driver binding run in `usb-core`. US-3: bulk and interrupt transfers for class
//! drivers with deep queues per endpoint (`queue`, `bulk`), transfer buffers (`buf`), cancel/timeout by stop and
//! skip, endpoint reset to toggle 0, and interrupt IN pipes with several transfers queued (`pipe`). SN-8a: isochronous transfers (`isoch`: one TD per
//! packet, per-packet results, missed service, halt repair) and alternate settings (`alt`).
//!
//! Layout: `regs` (register map, capability parsing), `trb`, `ring` (producer ring with Link TRBs, event ring),
//! `mem` (DMA access through MM-4's typed accessors), `ctx` (slot/endpoint contexts), `quirks` (by PCI ID), `hc`
//! (the controller logic, generic over `Env` so host tests run it against a fake controller model; `command`,
//! `events`, `slot`, `xfer` are further `impl` blocks), `port` (root ports), `hotplug` (the worker's rounds),
//! `usbhost` (`usb_core::Host`), `driver` (PCI probe, kernel `Env`, threads, poller), `ktests`.
//! No `unsafe` here: registers through `hal::mmio::IoMem`, ring and context memory through `mm::dma::DmaBuffer`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod alt;
pub mod buf;
mod bulk;
mod pipe;
mod driver;
mod command;
pub mod ctx;
mod events;
pub mod hc;
pub mod hotplug;
mod ktests;
mod ktests_usb;
mod ktests_xfer;
mod ktests_isoch;
pub mod isoch;
mod isoch_repair;
pub mod port;
pub mod slot;
mod usbhost;
mod xfer;
pub mod queue;
pub mod quirks;
pub mod mem;
pub mod regs;
pub mod ring;
pub mod trb;

#[cfg(test)]
mod fake;
#[cfg(test)]
mod fake_bulk;
#[cfg(test)]
mod fake_dev;
#[cfg(test)]
mod fake_usb;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_usb;
#[cfg(test)]
mod tests_xfer;
#[cfg(test)]
mod tests_fixes;
#[cfg(test)]
mod tests_ev;
#[cfg(test)]
mod tests_isoch;
#[cfg(test)]
mod fake_iso;

pub use driver::{controllers, is_xhci, KernelEnv, Xhci, CLASS_XHCI};
pub use hc::{CmdCompletion, CmdError, Controller, Env, State};
pub use bulk::dci_of;
pub use pipe::PIPE_ERROR_LIMIT;
