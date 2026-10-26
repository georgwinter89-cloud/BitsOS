//! The parts of bitsboot that do not touch the firmware: checks of files and values read from the stick, the
//! choices made from them, and the BootInfo size. They build for the host too, so `cargo test -p bitsboot`
//! tests them without OVMF (the UEFI side lives in the `bitsboot` binary).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod config;
pub mod cpu;
pub mod dmi;
pub mod edid;
pub mod elf;
pub mod gens;
pub mod pixel;
pub mod profile;
pub mod readall;
pub mod records;
pub mod screen;
pub mod sizing;
pub mod volume;
