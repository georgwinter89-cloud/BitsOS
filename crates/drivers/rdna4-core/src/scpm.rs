//! Whether the secure processor keeps the power controller's table to itself (SCPM), read from the PSP's runtime
//! database in VRAM as `psp_sw_init` reads it (`A:` amdgpu/amdgpu_psp.c:389-470, :489-499; the layout
//! amdgpu_psp.h:290-354). With SCPM Linux sends the PMFW neither its allowed-feature masks nor a table
//! (pm/swsmu/amdgpu_smu.c:735-752, :1776-1786, :1806-1812); the register trace of Linux's start on this card holds
//! neither message, so SCPM is on there. A card that reads otherwise is one whose start Linux would make with
//! messages no trace of this card shows: the stage that starts the power controller stops for it.

/// `PSP_RUNTIME_DB_OFFSET`: the database's header lies this far below the end of VRAM (`mc_vram_size`, A:412).
pub const FROM_END: u64 = 0x10_0000;
/// `struct psp_runtime_data_header`: cookie, version.
pub const HEADER_BYTES: usize = 4;
/// `struct psp_runtime_data_directory` right behind it: the count, two bytes of padding, 0x40 entries of 8 bytes.
pub const DIRECTORY_BYTES: usize = 4 + 8 * ENTRY_MAX as usize;
pub const COOKIE: u16 = 0x0ED5;
/// `PSP_RUNTIME_DB_DIAG_ENTRY_MAX_COUNT`.
pub const ENTRY_MAX: u16 = 0x40;
/// `PSP_RUNTIME_ENTRY_TYPE_PPTABLE_ERR_STATUS`.
pub const ENTRY_SCPM: u32 = 6;
/// `sizeof(struct psp_runtime_scpm_entry)`.
pub const ENTRY_BYTES: usize = 4;

/// What the database says, in the order Linux's reader gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scpm {
    /// The cookie is not there (A:418-422): Linux takes SCPM as off.
    NoDatabase { cookie: u16 },
    /// An entry count Linux refuses (A:428-432).
    BadCount { count: u16 },
    /// No entry of the SCPM type.
    NoEntry,
    /// The entry is smaller than its struct (A:449-453).
    BadSize { size: u16 },
    /// `SCPM_DISABLE`.
    Disabled,
    /// `SCPM_ENABLE` (1) or `SCPM_ENABLE_WITH_SCPM_ERR` (2): `adev->scpm_enabled`.
    Enabled { status: u32 },
    /// The read of VRAM failed.
    Unread,
}

impl Scpm {
    pub const fn enabled(self) -> bool {
        matches!(self, Scpm::Enabled { .. })
    }
}

/// The header's verdict: `Ok` to read the directory.
pub fn header(h: &[u8; HEADER_BYTES]) -> Result<(), Scpm> {
    let cookie = u16::from_le_bytes([h[0], h[1]]);
    if cookie == COOKIE { Ok(()) } else { Err(Scpm::NoDatabase { cookie }) }
}

/// The SCPM entry's offset from the header (A:435-460): the first entry of its type decides.
pub fn entry(dir: &[u8]) -> Result<u16, Scpm> {
    if dir.len() < DIRECTORY_BYTES {
        return Err(Scpm::Unread);
    }
    let count = u16::from_le_bytes([dir[0], dir[1]]);
    if count >= ENTRY_MAX {
        return Err(Scpm::BadCount { count });
    }
    for e in dir[4..].chunks_exact(8).take(usize::from(count)) {
        if u32::from_le_bytes([e[0], e[1], e[2], e[3]]) == ENTRY_SCPM {
            let (offset, size) = (u16::from_le_bytes([e[4], e[5]]), u16::from_le_bytes([e[6], e[7]]));
            return if usize::from(size) < ENTRY_BYTES { Err(Scpm::BadSize { size }) } else { Ok(offset) };
        }
    }
    Err(Scpm::NoEntry)
}

pub fn status(e: &[u8; ENTRY_BYTES]) -> Scpm {
    match u32::from_le_bytes(*e) {
        0 => Scpm::Disabled,
        status => Scpm::Enabled { status },
    }
}

/// A database with one SCPM entry of this status, as bytes from the header on (a model's card and tests).
pub fn synthetic(status: u32) -> [u8; HEADER_BYTES + 12 + ENTRY_BYTES] {
    let mut b = [0u8; HEADER_BYTES + 12 + ENTRY_BYTES];
    b[0..2].copy_from_slice(&COOKIE.to_le_bytes());
    b[2..4].copy_from_slice(&0x0100u16.to_le_bytes());
    b[4..6].copy_from_slice(&1u16.to_le_bytes());
    b[8..12].copy_from_slice(&ENTRY_SCPM.to_le_bytes());
    b[12..14].copy_from_slice(&16u16.to_le_bytes());
    b[14..16].copy_from_slice(&(ENTRY_BYTES as u16).to_le_bytes());
    b[16..20].copy_from_slice(&status.to_le_bytes());
    b
}
