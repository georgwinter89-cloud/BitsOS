//! FAT32 on the boot stick (owner: FS; FS-7).
//!
//! Our own implementation in safe Rust (decision and reasons in `plan/notes/fs-FS-7.md`): the write order is
//! the point of this crate, and it lives in a few hundred lines we control instead of being retrofitted into a
//! library that writes when it likes.
//!
//! - `bpb`: boot sector and FSInfo (FAT32 recognised by structure).
//! - `fat`: the allocation table (cache, chains, FAT 1 then FAT 2).
//! - `name`, `dirent`, `listing`: long names (UTF-8 <-> UTF-16), 8.3 aliases, entries, reading a directory as
//!   Windows does.
//! - `vol`, `vol_dir`, `vol_file`, `vol_ops`: one volume and its crash-safe changes.
//! - `protect`: the boot stick's rules (R8b), enforced inside every change.
//! - `file`, `glue`: the VFS side (page-cache backend, `vfs::FsOps`, mount).
//! - `stick`: finding the boot stick among the block disks, its identity, the block-layer write gate, `/boot`.
//! - `sysapi`: the narrow API for system generations (RB-6, install).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bpb;
pub mod dev;
pub mod dirent;
pub mod fat;
pub mod file;
pub mod gate;
pub mod glue;
pub mod lift;
mod ktests;
mod prio_ktests;
mod cold_ktests;
pub mod listing;
pub mod name;
pub mod protect;
pub mod stick;
pub mod sysapi;
pub mod vol;
mod vol_dir;
pub mod vol_file;
mod vol_ops;

pub use dev::{Dev, MemDev};
pub use vol::{Child, Clock, Options, Volume};

#[cfg(test)]
mod tests;
