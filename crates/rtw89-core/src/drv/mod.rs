//! The driver around the modules (WL-4): bring-up, the thread's turn, the chip's health, the log.
//! Generic over [`crate::Bus`]: the kernel crate (`crates/drivers/rtw89`) runs it on MMIO, DMA
//! below 4 GB and config space; the host tests run the same code on V1's fake chip.

pub mod device;
pub mod events;
pub mod health;
pub mod hooks;
pub mod runner;
pub mod status;

pub use device::{DevConfig, Device, Stage, UpError};
pub use events::{DevEvent, Level};
pub use runner::{Runner, Stats, TurnOut};
