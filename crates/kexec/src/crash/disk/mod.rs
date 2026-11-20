//! CRASHDISK (owner RB): a disk copy of the crash record, so it survives a cold reset. On 13.11.2026 the
//! Mini-PC (generation 150) panicked during a Steam download; its FADT reset (I/O 0xB2 <- 0xBE) is a cold
//! reset, the crash page in RAM was gone and only a photo of the screen was left.
//!
//! Linux 7.3.1's model is pstore (fs/pstore/): ramoops keeps records in RAM over a warm reset (our crash page),
//! pstore/blk writes them to a block device region with the driver's own `panic_write` (zone.c
//! `psz_zone_write` picks it `is_on_panic()`), efi-pstore puts them in UEFI variables. Here: the region is a
//! file on the boot ESP, `\BITSOS2\CRASH\SLOTS.BIN` (`format`: 4 slots of 256 KiB, a ring), made and checked at
//! boot (`boot`), and written at panic by the NVMe driver's polled writer (`device`; crates/drivers/nvme
//! panic.rs). UEFI variables are not used: V2 does not keep the firmware's runtime services after the boot
//! (bitsboot exits boot services and the kernel never calls back into the firmware), and a variable store
//! holds a few KiB where a report with every CPU and the log is tens of KiB.
//!
//! Safety (Georg's rules for the NVMe): the panic writes only when /boot on the ESP `nvme_boot=` names is
//! writable at this boot (the controller's boot window is open, and still the same one at the panic), only the
//! file's own blocks (checked at boot: inside the ESP, the data area, no overlap, the file's chain; and read
//! back through both paths), and every block again right before the write (`extents::holds`, the device's
//! window). Otherwise the record stays in RAM only and `crash_info` says why.
//!
//! - `format`, `extents`: pure, host-tested; `device`: the driver's side;
//! - `sink`: the panic path's writer (no lock, no heap, every wait bounded);
//! - `boot`: the thread that waits for /boot, reads the slots, prepares and arms;
//! - `show`: the records found (crash_info, the journal event, `/proc/bitsos/crash`, the desktop's line).

pub mod device;
pub mod extents;
pub mod format;
mod boot;
pub mod show;
pub(crate) mod sink;
mod ktests;
#[cfg(test)]
mod tests;

pub use device::{Device, Fail, Provider, Run};

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};
use extents::Extents;
use ksync::{Once, OnceRef, SpinLock};

static PROVIDER: OnceRef<Provider> = OnceRef::new();

/// The NVMe driver hands in its panic writers (the first call wins).
pub fn set_provider(p: &'static Provider) -> bool {
    PROVIDER.set(p)
}

/// What the panic path writes with, set once at boot.
pub(crate) struct Armed {
    pub dev: Arc<dyn Device>,
    pub ext: Extents,
    pub slot: usize,
    pub seq: u64,
    pub tag: u64,
    /// Wall clock (unix s) and uptime (ns) at arming: the panic's wall time without the clock's lock.
    pub wall_s: i64,
    pub up_ns: u64,
}

static ARMED: Once<Armed> = Once::new();

pub(crate) fn armed() -> Option<&'static Armed> {
    ARMED.get()
}

/// Where the disk copy stands (normal context: crash_info, the ktests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    /// RAM only, and why.
    Off(String),
    Armed { disk: String, slot: usize, seq: u64, runs: usize, first_lba: u64 },
    /// It should have worked: the step that failed.
    Failed(String),
}

static STATE: SpinLock<State> = SpinLock::named("crash.disk.state", State::Waiting);

pub fn state() -> State {
    STATE.lock().clone()
}

/// Waits (sleeping, at most `secs`) until the boot thread decided; its state then (ktests of other crates).
pub fn ktests_wait_decided(secs: u64) -> State {
    ktests::wait_decided(secs)
}

fn set_state(s: State) {
    *STATE.lock() = s;
}

/// Memory as last sampled (normal context, every few seconds): the panic path cannot take the zone locks.
pub(crate) static MEM_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static MEM_FREE: AtomicU64 = AtomicU64::new(0);
pub(crate) static MEM_AT_NS: AtomicU64 = AtomicU64::new(0);

pub(crate) fn sample_memory() {
    if let Some(f) = mm_core::frames() {
        let s = f.stats();
        MEM_TOTAL.store(s.total_bytes, Ordering::Relaxed);
        MEM_FREE.store(s.free_bytes, Ordering::Relaxed);
        MEM_AT_NS.store(time::now_ns(), Ordering::Relaxed);
    }
}

fn init() -> Result<(), &'static str> {
    boot::start();
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "crash.disk", init);
