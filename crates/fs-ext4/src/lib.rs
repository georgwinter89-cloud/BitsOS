//! ext4 for the data volume (owner: FS; FS-8a: read and write; FS-8b: the jbd2 journal).
//!
//! Our own implementation in safe Rust (the library spike and the decision are in plan/notes/fs-FS-8a.md):
//! every metadata change goes through one transaction (`tx`), so FS-8b can put jbd2 under it; file data
//! goes through the VFS page cache, ext4 owns only the block mapping and the metadata.
//!
//! - `sb`, `gd`, `inode`, `crc`, `le`: on-disk structures and checksums (metadata_csum crc32c, gdt_csum crc16).
//! - `balloc`: bitmaps and allocators; `extent`: extent trees; `blockmap`: ext2/3 block maps (read, migrated to extents before a change);
//!   `dir`, `vol_dir`: directories; `inline`, `xattr`: inline data and xattrs (read).
//! - `state`, `tx`: the volume state behind its lock and transactions.
//! - `jbd2`, `journal`, `recovery`: the journal's format, the commit path (ordered mode) and replay at mount.
//! - `vol`, `vol_data`, `vol_ns`: one mounted volume, file content, names.
//! - `file`, `glue`, `mount`: the VFS side (page-cache backend, `vfs::FsOps`, mounting a disk or an image).
//! - `dev`: what the volume reads and writes (memory, a block disk, a loop image file).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod balloc;
/// Linux errno values (uapi's list plus the two ext4 names for corruption: EFSBADCRC and EFSCORRUPTED).
pub mod errno {
    //! uapi's errno values; EBADMSG (`EFSBADCRC`, a metadata checksum is wrong) and EUCLEAN (`EFSCORRUPTED`,
    //! metadata is impossible) are what ext4 returns for a damaged volume.
    pub use uapi::errno::*;
}
pub mod blockmap;
pub mod crc;
pub mod data;
pub mod dev;
pub mod dir;
pub mod extent;
pub mod file;
pub mod glue;
pub mod gd;
pub mod imgblk;
pub mod inline;
pub mod inode;
pub mod jbd2;
pub mod journal;
mod ktests;
mod ktests_data;
mod ktests_journal;
pub mod le;
pub mod mkfs;
pub mod mount;
pub mod recovery;
pub mod sb;
pub mod state;
pub mod tx;
pub mod vol;
pub mod vol_data;
pub mod vol_dir;
pub mod vol_ns;
pub mod xattr;

pub use dev::{Dev, MemDev};
pub use vol::{Options, Volume};
pub use vol_ns::Kind;
#[cfg(test)]
mod tests;
