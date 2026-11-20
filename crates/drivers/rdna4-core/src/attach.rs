//! The first thing done to a card, `plan/spec/gpu-rdna4.md` piece 1: find its discovery blob. Before there is a table, Linux reads
//! two fixed dwords (A:`amdgpu_discovery.c`:295-321) and, only when the default place holds no table, the three
//! dwords of the PSP's mailbox (A:`amdgpu_discovery.c`:338-349). `Regs` has no write, so nothing built on it can
//! change a card.

use crate::layout;

/// A:`amdgpu_discovery.c`:142-143: fixed before there is a table; `regmap`'s tests show the table leads to the same
/// dwords.
pub const C2PMSG_33_DWORD: u32 = 0x16061;
pub const MEMSIZE_DWORD: u32 = 0xDE3;
/// Set in C2PMSG_33 when the card's firmware has finished starting. Usually 60 to 100 ms after power on, up to 2 s.
pub const IFWI_READY: u32 = 1 << 31;
pub const IFWI_WAIT_MS: u32 = 2000;

/// Everything this step needs from the hardware.
pub trait Regs {
    fn read_dword(&mut self, dword: u32) -> u32;
    fn sleep_ms(&mut self, ms: u32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
    /// Linux goes on regardless after its 2 s. Here it stops: this card was started by its own firmware long ago, so a
    /// card still not ready is not one to touch further.
    NotReady,
    Memsize(layout::MemsizeError),
    /// The blob would not lie inside BAR0.
    OutsideBar0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Located {
    pub memsize_mib: u32,
    pub vram_bytes: u64,
    /// Physical address to read `layout::DISCOVERY_READ` bytes at.
    pub discovery: u64,
}

pub fn locate_discovery<R: Regs + ?Sized>(
    regs: &mut R,
    bar0_base: u64,
    bar0_bytes: u64,
) -> Result<Located, AttachError> {
    let mut ready = false;
    for _ in 0..IFWI_WAIT_MS {
        if regs.read_dword(C2PMSG_33_DWORD) & IFWI_READY != 0 {
            ready = true;
            break;
        }
        regs.sleep_ms(1);
    }
    if !ready {
        return Err(AttachError::NotReady);
    }
    let memsize_mib = regs.read_dword(MEMSIZE_DWORD);
    let vram_bytes = layout::vram_bytes(memsize_mib).map_err(AttachError::Memsize)?;
    let discovery = layout::discovery_phys(bar0_base, bar0_bytes, memsize_mib).ok_or(AttachError::OutsideBar0)?;
    Ok(Located { memsize_mib, vram_bytes, discovery })
}

/// A:`amdgpu_discovery.c`:147-149 (`mmDRIVER_SCRATCH_0..2`, "consistent across all the SOCs"): where the card's
/// firmware says the table lies when it is not at the default place. Offset low, offset high, size in bytes.
pub const DRIVER_SCRATCH_0_DWORD: u32 = 0x94;
pub const DRIVER_SCRATCH_1_DWORD: u32 = 0x95;
pub const DRIVER_SCRATCH_2_DWORD: u32 = 0x96;

/// Which of Linux's two places held the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `memsize - 64 KiB` (A:`amdgpu_discovery.c`:322-336).
    Default,
    /// What DRIVER_SCRATCH_0..2 name (A:`amdgpu_discovery.c`:338-349).
    Mailbox,
}

/// The place the mailbox names: byte offset in VRAM (and so in BAR0), the size the card states, and how much of it
/// is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mailbox {
    pub offset: u64,
    pub size: u32,
    pub read: usize,
}

/// Linux takes whatever the three dwords say. Here a place is read only if the whole of it lies in the card's own
/// window, which is what makes the read safe on a card nobody has probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxRefusal {
    /// The size dword reads all ones: the card stopped answering between two reads.
    Gone,
    /// Too small for a table's header.
    Size { size: u32 },
    /// Linux's own reader needs both dword-aligned (A:`amdgpu_device.c` `amdgpu_device_mm_access`: BUG_ON).
    Unaligned { offset: u64, size: u32 },
    /// The place does not lie inside BAR0.
    OutsideBar0 { offset: u64, size: u32 },
}

/// Linux's second place for the table, asked only after the default place showed no signature. `Ok(None)`: the size
/// dword is 0, the card names no other place (Linux then goes on with the default and fails its check).
pub fn mailbox_discovery<R: Regs + ?Sized>(regs: &mut R, bar0_bytes: u64) -> Result<Option<Mailbox>, MailboxRefusal> {
    let size = regs.read_dword(DRIVER_SCRATCH_2_DWORD);
    if size == 0 {
        return Ok(None);
    }
    if size == u32::MAX {
        return Err(MailboxRefusal::Gone);
    }
    let lo = regs.read_dword(DRIVER_SCRATCH_0_DWORD);
    let hi = regs.read_dword(DRIVER_SCRATCH_1_DWORD);
    let offset = (hi as u64) << 32 | lo as u64;
    if (size as usize) < layout::HEADER_BYTES {
        return Err(MailboxRefusal::Size { size });
    }
    if offset % 4 != 0 || size % 4 != 0 {
        return Err(MailboxRefusal::Unaligned { offset, size });
    }
    if offset.checked_add(size as u64).is_none_or(|end| end > bar0_bytes) {
        return Err(MailboxRefusal::OutsideBar0 { offset, size });
    }
    Ok(Some(Mailbox { offset, size, read: (size as usize).min(layout::MAILBOX_READ_MAX) }))
}
