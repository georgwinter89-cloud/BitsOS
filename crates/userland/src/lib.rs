//! The kernel side of UL's user programs (item UL-2).
//!
//! - [`supervisor`]: starts `/bin/init` as the first user process after the kernel is ready (CONFIG `init=`,
//!   `init=off` disables it), watches it and restarts it with a back-off when it dies; gives up after repeated
//!   quick deaths (the kernel, MCP and the console stay up either way). The kernel calls [`start_init`].
//! - [`dhcpview`]: `/proc/net/dhcp` is NW's since NW-5 (`net::dhcpview`, registered there); re-exported here for
//!   the ktests that read it.
//! - `ktests`: `ul.init.*`, `ul.abi.wave_a/b/c` (the abi-* transcripts against CachyOS's) and `ul.stress.procs`.
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub use net::dhcpview;
pub mod supervisor;

/// The transcript comparison shared with the host test in `user/abi-tests` (same file, so both judge alike).
#[path = "../../../user/abi-tests/src/compare.rs"]
pub mod compare;

#[cfg(target_os = "none")]
mod ktests;

#[cfg(target_os = "none")]
pub use supervisor::{init_process, start_init};
