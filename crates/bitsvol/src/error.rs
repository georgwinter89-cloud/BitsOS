//! Errors of the pool. `NoSpace` reaches a stacked device's user as `DevError::NoSpace` (ENOSPC through
//! crates/block), so ext4 on a full pool sees ENOSPC, not an I/O error.

use devmodel::DevError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolError {
    /// The device below failed.
    Dev(DevError),
    /// No free chunk even after releasing freed chunks and dropping every automatic snapshot (ENOSPC).
    NoSpace,
    NoMemory,
    /// On-disk metadata failed a check (checksum, bounds, reference counts); the text says which.
    Corrupt(&'static str),
    NotFound,
    /// Write to a snapshot (EROFS).
    ReadOnly,
    /// The volume is open as a device and cannot be deleted (EBUSY).
    Busy,
    InvalidArgument,
}

impl VolError {
    pub fn errno(self) -> i32 {
        match self {
            VolError::NoSpace => 28,
            VolError::NoMemory => 12,
            VolError::NotFound => 2,
            VolError::ReadOnly => 30,
            VolError::Busy => 16,
            VolError::InvalidArgument => 22,
            VolError::Dev(DevError::Refused) => 30,
            VolError::Dev(DevError::NoMemory) => 12,
            VolError::Dev(DevError::InvalidArgument) => 22,
            VolError::Dev(_) | VolError::Corrupt(_) => 5,
        }
    }

    /// The error a stacked `BlockDevice` returns.
    pub fn to_dev(self) -> DevError {
        match self {
            VolError::Dev(e) => e,
            VolError::NoMemory => DevError::NoMemory,
            VolError::ReadOnly | VolError::Busy => DevError::Refused,
            VolError::InvalidArgument | VolError::NotFound => DevError::InvalidArgument,
            VolError::NoSpace => DevError::NoSpace,
            VolError::Corrupt(_) => DevError::Io,
        }
    }
}

impl From<DevError> for VolError {
    fn from(e: DevError) -> Self {
        VolError::Dev(e)
    }
}

pub type Result<T> = core::result::Result<T, VolError>;
