//! What a block request answers when it fails, and the errno a syscall makes of it.

use devmodel::DevError;

/// Why a write, a write with FUA or a flush before it, or a discard was refused (R8a). Every refusal is
/// `EROFS` to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The device itself is read-only (write-protect switch, MODE SENSE WP, a read-only RAM disk).
    DeviceReadOnly,
    /// Its partition table holds a Windows partition (Microsoft basic data/reserved, recovery, LDM, Storage
    /// Spaces; MBR NTFS/exFAT, hidden NTFS, recovery, dynamic) or the disk is a Windows file system as a whole.
    WindowsPartition,
    /// Its partition table holds an EFI System Partition (GPT type GUID or MBR type EFh).
    EfiPartition,
    /// The first sectors could not be read or a GPT was announced and none could be read: refused until
    /// the table is known (the safe side).
    UnknownTable,
    /// The boot stick (or a USB disk that might be it while the stick's identity is unknown): read-only
    /// unless CONFIG `stick_write=on` (FS-7 brings writes below `\BITSOS2\`).
    BootStick,
    /// Another USB disk: read-only unless CONFIG `usb_msd_write=on` (tests only, R8b).
    UsbWritesOff,
    /// An NVMe namespace without CONFIG `nvme_write=on` (NV's gate checks too).
    NvmeWritesOff,
    /// A device of a kind the block layer has no write rule for.
    UnknownKind,
    /// A plugged-in gate (`WriteGate`) said no; its name.
    Gate(&'static str),
}

impl Refusal {
    pub fn as_str(&self) -> &'static str {
        match self {
            Refusal::DeviceReadOnly => "device read-only",
            Refusal::WindowsPartition => "Windows partition",
            Refusal::EfiPartition => "EFI system partition",
            Refusal::UnknownTable => "partition table unknown",
            Refusal::BootStick => "boot stick (stick_write off)",
            Refusal::UsbWritesOff => "USB disk (usb_msd_write off)",
            Refusal::NvmeWritesOff => "NVMe (nvme_write off)",
            Refusal::UnknownKind => "no write rule for this device kind",
            Refusal::Gate(n) => n,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// The device reported an error (medium error, transport failure after its retries).
    Io,
    /// A write or discard past the end of the device.
    NoSpace,
    /// A read past the end of the device (Linux: "attempt to access beyond end of device", EIO).
    BeyondEnd,
    /// R8a: the write-refusal hook said no.
    ReadOnly(Refusal),
    /// Length not a multiple of the block size, an empty request.
    Invalid,
    NoMemory,
    /// The device is gone (unplugged, no medium) or the disk was unregistered.
    Gone,
    /// The request did not complete within its deadline (B4); it is dropped from the queue or, if the device
    /// already has it, its answer is dropped.
    Timeout,
    /// The device cannot do this (discard on a device without it).
    Unsupported,
}

impl BlockError {
    /// The positive errno (syscalls return it negated).
    pub fn errno(self) -> i64 {
        use uapi::errno::*;
        match self {
            BlockError::Io | BlockError::BeyondEnd | BlockError::Gone | BlockError::Timeout => EIO,
            BlockError::NoSpace => ENOSPC,
            BlockError::ReadOnly(_) => EROFS,
            BlockError::Invalid => EINVAL,
            BlockError::NoMemory => ENOMEM,
            BlockError::Unsupported => EOPNOTSUPP,
        }
    }

    pub fn from_dev(e: DevError) -> BlockError {
        match e {
            DevError::Gone => BlockError::Gone,
            DevError::Timeout | DevError::Io => BlockError::Io,
            DevError::Refused => BlockError::ReadOnly(Refusal::DeviceReadOnly),
            DevError::NoMemory => BlockError::NoMemory,
            DevError::Unsupported => BlockError::Unsupported,
            DevError::InvalidArgument => BlockError::Invalid,
            DevError::NoSpace => BlockError::NoSpace,
        }
    }

    /// The `DevError` a stacked device answers with (a `Disk` is a `BlockDevice` itself).
    pub fn to_dev(self) -> DevError {
        match self {
            BlockError::Io | BlockError::BeyondEnd => DevError::Io,
            BlockError::Timeout => DevError::Timeout,
            BlockError::NoSpace => DevError::NoSpace,
            BlockError::Invalid => DevError::InvalidArgument,
            BlockError::ReadOnly(_) => DevError::Refused,
            BlockError::NoMemory => DevError::NoMemory,
            BlockError::Gone => DevError::Gone,
            BlockError::Unsupported => DevError::Unsupported,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BlockError::Io => "I/O error",
            BlockError::NoSpace => "beyond the end (write)",
            BlockError::BeyondEnd => "beyond the end (read)",
            BlockError::ReadOnly(r) => r.as_str(),
            BlockError::Invalid => "invalid request",
            BlockError::NoMemory => "no memory",
            BlockError::Gone => "device gone",
            BlockError::Timeout => "timeout",
            BlockError::Unsupported => "unsupported",
        }
    }
}
