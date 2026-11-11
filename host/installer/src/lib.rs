//! The BitsOS installer: chooses a USB stick (only removable USB disks without the running system), asks the
//! questions, fetches the release of its own version and checks it against the manifest hash it was built with,
//! fetches Steam and Brave from their makers, and writes an MBR stick with one FAT32 partition labelled BITSOS
//! (or adds the release to a BitsOS stick as a new generation); `qemu` does the same into an image file and starts
//! it in a QEMU window. `pack` is what `cargo xtask release-assets` uses
//! to make the assets, so both sides read and write the same format.
// Unsafe code is the Win32 calls in devices::windows and the one ioctl in devices::linux that drops the cache.
#![deny(unsafe_code)]

pub mod archive;
pub mod blockio;
pub mod cli;
pub mod config;
pub mod confirm;
pub mod devices;
pub mod dots;
pub mod fat;
pub mod fetch;
pub mod flow;
pub mod gens;
pub mod json;
pub mod manifest;
pub mod mbr;
pub mod qemu;
pub mod release;
pub mod secrets;
pub mod sha;
pub mod sha512;
pub mod stick;
pub mod tar;
pub mod ui;
pub mod vendor;

#[cfg(test)]
mod flow_tests;
#[cfg(test)]
mod tests;
