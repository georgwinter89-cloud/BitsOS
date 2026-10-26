//! The hardware abstraction (V2-PLAN §3): driver traits every back end implements, so a new card is a new crate
//! behind the same trait (the Gaming-PC later). Owner of the traits: desk (changes by change request); each
//! back end belongs to its lane. Rules (CONTRACTS.md): every driver implements `Quiesce` from its first item;
//! long waits sleep (R5); state changes are `klog::event!`s (B6); counts of things are dynamic (R1).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod audio;
pub mod block;
pub mod display;
pub mod net;
pub mod watchdog;

use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

/// Stops a device's DMA and interrupts and clears the bus master it set, before Reload, reboot or a panic
/// (RB-5a/5b). Must work with interrupts off and must not sleep.
pub trait Quiesce: Send + Sync {
    fn name(&self) -> &str;
    fn quiesce(&self);
}

static QUIESCE: SpinLock<Vec<Arc<dyn Quiesce>>> = SpinLock::named("devmodel.quiesce", Vec::new());

/// Every driver registers itself here when it starts a device.
pub fn register_quiesce(d: Arc<dyn Quiesce>) {
    QUIESCE.lock().push(d);
}

/// Quiesces every registered device, newest first. Returns the names in the order handled.
pub fn quiesce_all() -> Vec<alloc::string::String> {
    let list: Vec<Arc<dyn Quiesce>> = QUIESCE.lock().iter().rev().cloned().collect();
    list.iter()
        .map(|d| {
            d.quiesce();
            alloc::string::String::from(d.name())
        })
        .collect()
}

/// Quiesces only the registered devices with this exact name (tests, a single driver's restart).
pub fn quiesce_matching(name: &str) -> usize {
    let list: Vec<Arc<dyn Quiesce>> = QUIESCE.lock().iter().filter(|d| d.name() == name).cloned().collect();
    list.iter().for_each(|d| d.quiesce());
    list.len()
}

/// Error type shared by driver traits where nothing more specific fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevError {
    /// Device gone (unplugged, reads all ones).
    Gone,
    Timeout,
    Io,
    /// Refused by a safety rule (R8: write gate, opt-in key).
    Refused,
    NoMemory,
    Unsupported,
    InvalidArgument,
    /// No space left (a full thin pool, or a write beyond the end of a disk): ENOSPC.
    NoSpace,
}
