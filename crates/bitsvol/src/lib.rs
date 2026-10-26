//! Copy-on-write volumes and snapshots (B1). Owner: VS.
//!
//! A thin pool on any `BlockDevice` (V2-PLAN §1a B1, V2-LANES VS-1): volumes and snapshots share chunks
//! through copy-on-write mapping trees with reference counts; two alternating superblocks make every commit
//! atomic. The core (`Pool`) needs only `alloc` and builds on the host for tests and the Linux rescue tool
//! (VS-4); `device` stacks a volume as a `BlockDevice` in the kernel.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

/// An Info event, or Debug when the pool is quiet (`Config::quiet`: test pools opened by the thousand).
macro_rules! note {
    ($quiet:expr, $name:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if $quiet {
            klog::event!(Debug, "bitsvol", $name $(, $k = $v)*)
        } else {
            klog::event!(Info, "bitsvol", $name $(, $k = $v)*)
        }
    };
}

mod checks;
mod crc;
pub mod device;
pub mod error;
mod ktests;
pub mod layout;
pub mod memdev;
pub mod model;
pub mod pool;
mod refs;
mod scan;
mod tree;
pub mod volume;

#[cfg(test)]
mod tests;

pub use device::{BEFORE_ROLLBACK, DeferFlush, PoolHandle, VolumeDevice, Window, set_clock};
pub use error::VolError;
pub use pool::{Config, Pool, Stats};
pub use volume::VolumeInfo;
