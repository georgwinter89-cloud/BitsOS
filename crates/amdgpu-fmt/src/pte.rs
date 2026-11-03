//! GFX 11 page-table entry formats (GC 10.x/11.x layout; GFX 12 moves bits and needs its own module).
//!
//! Bit names follow Linux's macros (linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_vm.h:57-112), not the
//! letters of the format comment at gmc_v11_0.c:446-477 (which calls bit 51 "T", 55 "L", 56 "F" and leaves 54
//! reserved in the PTE; Linux sets bit 54 as PDE_PTE on directory levels and the PDE comment calls it "P").
//!
//! PTE (and PDE used as PTE): address 47:12, MTYPE 50:48, fragment 11:7, flags in 6:0 and the high bits.
//! PDE (pointer to the next table): address 47:6, C (= SNOOPED) bit 2, SYSTEM bit 1, VALID bit 0.

use crate::store::TableAddr;

pub const VALID: u64 = 1 << 0;
pub const SYSTEM: u64 = 1 << 1;
pub const SNOOPED: u64 = 1 << 2;
pub const TMZ: u64 = 1 << 3;
pub const EXECUTABLE: u64 = 1 << 4;
pub const READABLE: u64 = 1 << 5;
pub const WRITEABLE: u64 = 1 << 6;
pub const FRAG_SHIFT: u32 = 7;
pub const FRAG_MASK: u64 = 0x1f << FRAG_SHIFT;
pub const MTYPE_SHIFT: u32 = 48;
pub const MTYPE_MASK: u64 = 7 << MTYPE_SHIFT;
pub const PRT: u64 = 1 << 51;
/// Software bits 53:52 (gmc_v11_0.c:453); the hardware ignores them.
pub const SW_MASK: u64 = 3 << 52;
pub const PDE_PTE: u64 = 1 << 54;
pub const LOG: u64 = 1 << 55;
pub const TF: u64 = 1 << 56;
pub const NOALLOC: u64 = 1 << 58;
pub const BFS_SHIFT: u32 = 59;
pub const BFS_MASK: u64 = 0x1f << BFS_SHIFT;

/// Page address field of a PTE (amdgpu_gmc.c:164 masks the same bits).
pub const PTE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Table address field of a PDE (gmc_v11_0.c:500 refuses anything outside it).
pub const PDE_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_FFC0;
/// Highest fragment the 5-bit field holds (Linux caps at 31 from Vega10 on, amdgpu_vm_pt.c:888).
pub const MAX_FRAG: u32 = 31;

/// An unmapped PTE: Linux writes EXECUTABLE alone (the GMC9 fault-priority workaround, amdgpu_vm_pt.c:476
/// and :827), so a fault on it is reported with PTE priority.
pub const INVALID_PTE: u64 = EXECUTABLE;
/// An unmapped directory entry: PDE_PTE without VALID (amdgpu_vm_pt.c:470-473), a leaf that faults.
pub const INVALID_PDE: u64 = PDE_PTE;

/// Bits a valid page entry may carry besides address and flags we encode.
const PTE_KNOWN: u64 = VALID
    | SYSTEM
    | SNOOPED
    | TMZ
    | EXECUTABLE
    | READABLE
    | WRITEABLE
    | FRAG_MASK
    | MTYPE_MASK
    | SW_MASK
    | NOALLOC
    | PTE_ADDR_MASK;

/// GPU memory type in bits 50:48 (include/soc21_enum.h MTYPE_NC/WC/CC/UC = 0..3; gmc_v11_0.c:479).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mtype {
    /// Non-coherent, the default for every mapping (gmc_v11_0.c:482).
    #[default]
    Nc = 0,
    Wc = 1,
    Cc = 2,
    /// Uncached: the GART (gmc_v11_0.c:753) and coherent/uncached BOs (gmc_v11_0.c:544).
    Uc = 3,
}

impl Mtype {
    pub fn from_bits(v: u64) -> Option<Mtype> {
        match v {
            0 => Some(Mtype::Nc),
            1 => Some(Mtype::Wc),
            2 => Some(Mtype::Cc),
            3 => Some(Mtype::Uc),
            _ => None,
        }
    }
}

/// Access and cache attributes of a mapping (what Linux takes from the VA ioctl flags).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MapFlags {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    pub mtype: Mtype,
    /// Trusted memory zone (encrypted BO).
    pub tmz: bool,
    /// MALL no-allocate hint.
    pub noalloc: bool,
}

/// AMDGPU_VM_PAGE_* and AMDGPU_VM_MTYPE_* of the VA ioctl (include/uapi/drm/amdgpu_drm.h:536-558).
pub mod uapi {
    pub const PAGE_READABLE: u32 = 1 << 1;
    pub const PAGE_WRITEABLE: u32 = 1 << 2;
    pub const PAGE_EXECUTABLE: u32 = 1 << 3;
    pub const PAGE_PRT: u32 = 1 << 4;
    pub const MTYPE_MASK: u32 = 0xf << 5;
    pub const MTYPE_NC: u32 = 1 << 5;
    pub const MTYPE_WC: u32 = 2 << 5;
    pub const MTYPE_CC: u32 = 3 << 5;
    pub const MTYPE_UC: u32 = 4 << 5;
    pub const PAGE_NOALLOC: u32 = 1 << 9;
}

impl MapFlags {
    /// Read and write, NC: the common data mapping.
    pub const RW: MapFlags =
        MapFlags { read: true, write: true, exec: false, mtype: Mtype::Nc, tmz: false, noalloc: false };

    /// The ioctl flags as GFX 11 sees them (amdgpu_gem.c:642 with gmc_v11_0.c:479: DEFAULT, RW and unknown
    /// memory types become NC). Returns the flags and whether PRT was asked for; other bits are the
    /// ioctl's business (DR validates them).
    pub fn from_uapi(flags: u32) -> (MapFlags, bool) {
        let mtype = match flags & uapi::MTYPE_MASK {
            uapi::MTYPE_WC => Mtype::Wc,
            uapi::MTYPE_CC => Mtype::Cc,
            uapi::MTYPE_UC => Mtype::Uc,
            _ => Mtype::Nc,
        };
        let f = MapFlags {
            read: flags & uapi::PAGE_READABLE != 0,
            write: flags & uapi::PAGE_WRITEABLE != 0,
            exec: flags & uapi::PAGE_EXECUTABLE != 0,
            mtype,
            tmz: false,
            noalloc: flags & uapi::PAGE_NOALLOC != 0,
        };
        (f, flags & uapi::PAGE_PRT != 0)
    }

    /// The entry bits of these flags (no VALID, no address).
    pub fn bits(&self) -> u64 {
        let mut b = (self.mtype as u64) << MTYPE_SHIFT;
        if self.read {
            b |= READABLE;
        }
        if self.write {
            b |= WRITEABLE;
        }
        if self.exec {
            b |= EXECUTABLE;
        }
        if self.tmz {
            b |= TMZ;
        }
        if self.noalloc {
            b |= NOALLOC;
        }
        b
    }

    fn from_entry(e: u64) -> Option<MapFlags> {
        Some(MapFlags {
            read: e & READABLE != 0,
            write: e & WRITEABLE != 0,
            exec: e & EXECUTABLE != 0,
            mtype: Mtype::from_bits((e & MTYPE_MASK) >> MTYPE_SHIFT)?,
            tmz: e & TMZ != 0,
            noalloc: e & NOALLOC != 0,
        })
    }
}

/// Where a page lives, which decides the SYSTEM and SNOOPED bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The carve-out, reached through the local memory controller; the address is host-physical
    /// (FB_OFFSET + offset), see [`crate::addr::Carveout`].
    Local,
    /// System memory through the data fabric; `snooped` for CPU-cached pages (amdgpu_ttm.c:1084).
    System { snooped: bool },
}

impl Target {
    fn bits(self) -> u64 {
        match self {
            Target::Local => 0,
            Target::System { snooped: false } => SYSTEM,
            Target::System { snooped: true } => SYSTEM | SNOOPED,
        }
    }
}

/// Why an entry cannot be encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Address not aligned to what the entry needs (4 KiB page, leaf span, 64-byte table).
    Misaligned(u64),
    /// Address has bits above 47 (gmc_v11_0.c:500 is a BUG_ON in Linux; here an error).
    TooWide(u64),
    /// Fragment above [`MAX_FRAG`].
    Frag(u32),
}

/// A page entry (PTB level, or a directory level when `pde_as_pte`): `phys` must be aligned to `span`
/// bytes (4 KiB for a PTE, 2 MiB/1 GiB for huge leaves).
pub fn leaf(phys: u64, span: u64, target: Target, flags: MapFlags, frag: u32, pde_as_pte: bool) -> Result<u64, EncodeError> {
    if phys >> 48 != 0 {
        return Err(EncodeError::TooWide(phys));
    }
    if span < crate::GPU_PAGE || !span.is_power_of_two() || phys & (span - 1) != 0 {
        return Err(EncodeError::Misaligned(phys));
    }
    if frag > MAX_FRAG {
        return Err(EncodeError::Frag(frag));
    }
    let mut e = phys | VALID | target.bits() | flags.bits() | (u64::from(frag) << FRAG_SHIFT);
    if pde_as_pte {
        e |= PDE_PTE;
    }
    Ok(e)
}

/// A partially-resident-texture entry: never valid, PRT|SNOOPED|LOG|SYSTEM plus the mapping's flags, address
/// 0 (gmc_v11_0.c:535-541); a GPU access reads zero and writes are dropped instead of faulting.
pub fn prt(flags: MapFlags, frag: u32, pde_as_pte: bool) -> Result<u64, EncodeError> {
    if frag > MAX_FRAG {
        return Err(EncodeError::Frag(frag));
    }
    let mut e = PRT | SNOOPED | LOG | SYSTEM | flags.bits() | (u64::from(frag) << FRAG_SHIFT);
    if pde_as_pte {
        e |= PDE_PTE;
    }
    Ok(e)
}

/// A directory entry pointing at the next table (amdgpu_gmc.c:103 get_pde_for_bo: VALID, SYSTEM and C for a
/// table in cached system memory). The address is host-physical: for a table in the carve-out that is
/// FB_OFFSET + offset, which Linux computes from the MC address in gmc_v11_0.c:499.
pub fn pde(table: TableAddr) -> Result<u64, EncodeError> {
    if table.phys >> 48 != 0 {
        return Err(EncodeError::TooWide(table.phys));
    }
    if table.phys & !PDE_ADDR_MASK != 0 {
        return Err(EncodeError::Misaligned(table.phys));
    }
    Ok(table.phys | VALID | table.target.bits())
}

/// Value for a page-table base register (CONTEXTn_PAGE_TABLE_BASE): the root PDE (amdgpu_gmc.c:127 pd_addr).
pub fn pd_addr(root: TableAddr) -> Result<u64, EncodeError> {
    pde(root)
}

/// One decoded entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// No translation here (VALID clear, PRT clear).
    Invalid,
    /// PRT range: valid clear, PRT set.
    Prt { frag: u32 },
    /// A page or a huge leaf (PDE_PTE on a directory level).
    Page { phys: u64, target: Target, flags: MapFlags, frag: u32, pde_as_pte: bool },
    /// A pointer to the next table.
    Table(TableAddr),
}

/// Why an entry does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Bits set that GFX 11 leaves reserved or that this crate never writes (BFS, TF, reserved PDE bits,
    /// MTYPE 4-7, LOG/PRT on a valid page).
    Reserved(u64),
    /// SNOOPED without SYSTEM on a page: not something GR writes.
    SnoopLocal(u64),
}

/// Decode an entry read from a directory level (`leaf_allowed`: PDE_PTE makes it a leaf) or from a page
/// table (`is_ptb`: it is always a PTE there).
pub fn decode(e: u64, is_ptb: bool) -> Result<Entry, DecodeError> {
    let leafish = is_ptb || e & PDE_PTE != 0;
    if e & VALID == 0 {
        if e & PRT != 0 {
            // Linux writes PRT only with SNOOPED|LOG|SYSTEM (gmc_v11_0.c:535-541) and, on a directory level,
            // as PDE-as-PTE; a directory entry without PDE_PTE is a pointer to the walker, never a PRT range.
            if !leafish || e & (SNOOPED | LOG | SYSTEM) != SNOOPED | LOG | SYSTEM {
                return Err(DecodeError::Reserved(e));
            }
            return Ok(Entry::Prt { frag: ((e & FRAG_MASK) >> FRAG_SHIFT) as u32 });
        }
        return Ok(Entry::Invalid);
    }
    if !leafish {
        // Pointer: only address, C, SYSTEM, VALID.
        if e & !(PDE_ADDR_MASK | SNOOPED | SYSTEM | VALID) != 0 {
            return Err(DecodeError::Reserved(e));
        }
        let target = target_of(e).ok_or(DecodeError::SnoopLocal(e))?;
        return Ok(Entry::Table(TableAddr { phys: e & PDE_ADDR_MASK, target }));
    }
    let known = if is_ptb { PTE_KNOWN } else { PTE_KNOWN | PDE_PTE };
    if e & !known != 0 {
        return Err(DecodeError::Reserved(e));
    }
    let flags = MapFlags::from_entry(e).ok_or(DecodeError::Reserved(e))?;
    let target = target_of(e).ok_or(DecodeError::SnoopLocal(e))?;
    Ok(Entry::Page {
        phys: e & PTE_ADDR_MASK,
        target,
        flags,
        frag: ((e & FRAG_MASK) >> FRAG_SHIFT) as u32,
        pde_as_pte: e & PDE_PTE != 0,
    })
}

fn target_of(e: u64) -> Option<Target> {
    match (e & SYSTEM != 0, e & SNOOPED != 0) {
        (false, false) => Some(Target::Local),
        (false, true) => None,
        (true, s) => Some(Target::System { snooped: s }),
    }
}
