//! The host-testable core of the RTL8852BE (rtw89) driver.
//!
//! Every access to the chip goes through [`Bus`]: the kernel driver
//! (`crates/drivers/rtw89`, WL-4) implements it on MMIO, config space and DMA;
//! on the host, [`tracebus::TraceBus`] sends each access as one of V1's 16-byte
//! op records to the JS fake chip, so a port can be checked op by op against
//! `bringup-golden.txt` without QEMU. The driver code keeps Linux rtw89's order
//! of every register access; each function names its Linux original.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(any(test, feature = "std"))]
extern crate std;

pub mod bringup;
pub mod btc;
pub mod bus;
pub mod chan;
pub mod deadread;
pub mod drv;
pub mod error;
pub mod events;
pub mod fw;
pub mod macinit;
pub mod pci;
pub mod pcitx;
pub mod phy;
pub mod rfk1;
pub mod rfk2;
pub mod sta;
#[path = "gen/mod.rs"]
pub mod tables;
pub mod trace;
pub mod txrx;

#[cfg(any(test, feature = "std"))]
pub mod membus;
#[cfg(feature = "std")]
pub mod tracebus;

pub use bus::{Bus, BusError, BusResult, MemWidth, Width};
pub use error::Rtw89Error;

#[cfg(test)]
mod tests;
#[cfg(all(test, feature = "std"))]
mod tests_tracebus;
