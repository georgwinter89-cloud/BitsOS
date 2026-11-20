//! The disk the panic path writes its record to with its own polled I/O (CRASHDISK). kexec cannot depend on the
//! NVMe driver (nvme -> fs-ext4 -> kexec), so the driver implements this and hands it in (`set_provider`).

use alloc::string::String;
use alloc::sync::Arc;

/// One write of the record: `blocks` blocks at `lba` from the buffer at byte `off`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub lba: u64,
    pub off: usize,
    pub blocks: u32,
}

/// Why a panic write failed: the step and, for a command, its status (SCT << 8 | SC; 0 for a time-out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fail {
    pub stage: &'static str,
    pub status: u32,
}

/// Panic context for `ready`, `put`, `get` and `write`: no lock is waited for, nothing allocates, nothing sleeps,
/// every wait is bounded by `bound_ns` on `clock` (raw TSC: the clock's sequence lock may be held for good).
pub trait Device: Send + Sync {
    fn name(&self) -> &str;
    /// Bytes of the write buffer (one slot).
    fn capacity(&self) -> usize;
    fn block_size(&self) -> usize;
    /// The one range (first LBA, blocks) the device ever writes: the EFI System partition `nvme_boot=` names.
    fn allowed(&self) -> (u64, u64);
    /// Copies into / out of the write buffer; false outside it.
    fn put(&self, off: usize, data: &[u8]) -> bool;
    fn get(&self, off: usize, out: &mut [u8]) -> bool;
    /// Err: why a write now is not allowed (the boot partition's window closed or changed, the controller gone).
    fn ready(&self) -> Result<(), &'static str>;
    /// Writes every run from the buffer, Force Unit Access, then a flush; Ok = durable. Refuses all before the
    /// first command when one run leaves `allowed` or the buffer.
    fn write(&self, runs: &[Run], clock: &dyn Fn() -> u64, bound_ns: u64) -> Result<(), Fail>;
    /// Normal context (boot): reads blocks through the driver's own path, for the extent check.
    fn read(&self, lba: u64, out: &mut [u8]) -> Result<(), String>;
}

/// The driver's answer to "a panic writer for the whole disk `disk` with a buffer of `bytes`".
pub type Provider = fn(disk: &str, bytes: usize) -> Result<Arc<dyn Device>, String>;
