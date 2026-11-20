//! Owner: wave GP-CPU (plan/spec/gaming-pc.md §2, DMA gate). The Intel VT-d table (DMAR) and what an IOMMU's status
//! registers say at handover, read-only. BitsOS has no IOMMU driver: if the firmware left DMA translation or a
//! protected memory region switched on, a device's DMA is blocked by the hardware, and the kernel has to know that
//! before the first driver turns bus mastering on. Pure: `acpi` reads the bytes and the registers and asks here.
//!
//! Table layout: linux-7.3.1/include/acpi/actbl1.h:802-879; walk and checks: drivers/iommu/intel/dmar.c:583-620,
//! :881-917; registers and bits: drivers/iommu/intel/iommu.h:68-84, :151-152, :180-181, :266-289.

use alloc::vec::Vec;

/// Header (36) + host address width (1) + flags (1) + reserved (10) (actbl1.h:802-807).
pub const HEADER_LEN: usize = 48;
/// DMAR flags (linux-7.3.1/include/linux/dmar.h:24-26).
pub const FLAG_INTR_REMAP: u8 = 0x1;
pub const FLAG_X2APIC_OPT_OUT: u8 = 0x2;
pub const FLAG_PLATFORM_OPT_IN: u8 = 0x4;
/// Remapping structure types (actbl1.h:824-833).
pub const TYPE_DRHD: u16 = 0;
pub const TYPE_RMRR: u16 = 1;
pub const TYPE_ATSR: u16 = 2;
pub const TYPE_RHSA: u16 = 3;
pub const TYPE_ANDD: u16 = 4;
pub const TYPE_SATC: u16 = 5;
pub const TYPE_SIDP: u16 = 6;
/// DRHD: header (4), flags, size, segment, register base (actbl1.h:869-875), device scopes follow.
pub const DRHD_LEN: usize = 16;
/// DRHD flag: this unit covers every device of the segment no other unit names (actbl1.h:879).
pub const DRHD_INCLUDE_ALL: u8 = 1;
/// More units than any PC has; a table that names more is hostile or broken and the walk stops there.
pub const MAX_UNITS: usize = 32;

/// Register offsets (iommu.h:68-72, :80).
pub const REG_VER: u64 = 0x00;
pub const REG_CAP: u64 = 0x08;
pub const REG_ECAP: u64 = 0x10;
pub const REG_GSTS: u64 = 0x1C;
pub const REG_PMEN: u64 = 0x64;
/// Global status bits (iommu.h:281-289).
pub const GSTS_TES: u32 = 1 << 31;
pub const GSTS_RTPS: u32 = 1 << 30;
pub const GSTS_QIES: u32 = 1 << 26;
pub const GSTS_IRES: u32 = 1 << 25;
pub const GSTS_CFIS: u32 = 1 << 23;
/// Protected memory enable: the status bit (iommu.h:266-267).
pub const PMEN_EPM: u32 = 1 << 31;
pub const PMEN_PRS: u32 = 1 << 0;
/// Capability bits that say the protected low/high memory regions exist (iommu.h:180-181).
pub const CAP_PLMR: u64 = 1 << 5;
pub const CAP_PHMR: u64 = 1 << 6;

/// One DMA remapping hardware unit the table names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drhd {
    pub flags: u8,
    /// Register set size: 2^(size + 12) bytes.
    pub size: u8,
    pub segment: u16,
    pub address: u64,
}

impl Drhd {
    pub fn include_all(&self) -> bool {
        self.flags & DRHD_INCLUDE_ALL != 0
    }
    /// Bytes of the register set, at least one page, at most 1 MiB whatever the table says.
    pub fn reg_bytes(&self) -> u64 {
        1u64 << (12 + (self.size & 0xF).min(8))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dmar {
    /// Host address width in bits (the table stores it minus one; Linux prints "Host address width 42").
    pub host_address_width: u32,
    pub flags: u8,
    pub units: Vec<Drhd>,
    /// Reserved memory regions (RMRR): memory a device must keep reaching by DMA. Counted, not read.
    pub rmrr: u32,
    pub atsr: u32,
    pub rhsa: u32,
    pub andd: u32,
    pub satc: u32,
    pub sidp: u32,
    /// Structures of a type this version does not know (Linux goes on for forward compatibility, dmar.c:603-606).
    pub unknown: u32,
}

impl Dmar {
    pub fn platform_opt_in(&self) -> bool {
        self.flags & FLAG_PLATFORM_OPT_IN != 0
    }
    pub fn intr_remap(&self) -> bool {
        self.flags & FLAG_INTR_REMAP != 0
    }
}

/// Something wrong with the table, with the byte offset it was seen at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem {
    pub what: &'static str,
    pub at: u64,
}

/// What a walk of the table gave: the table as far as it could be trusted, and what was wrong.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub dmar: Option<Dmar>,
    pub problems: Vec<Problem>,
}

fn u16_at(t: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(t.get(o..o.checked_add(2)?)?.try_into().ok()?))
}
fn u64_at(t: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(t.get(o..o.checked_add(8)?)?.try_into().ok()?))
}

/// Walks the table `t` (the whole table, header included; the caller checked length field and checksum). Never
/// panics and never reads past `t`; units found before a broken structure are kept, as their registers can
/// still be asked.
pub fn parse(t: &[u8]) -> Parsed {
    let mut out = Parsed::default();
    if t.len() < HEADER_LEN {
        out.problems.push(Problem { what: "too_short", at: t.len() as u64 });
        return out;
    }
    let mut d = Dmar { host_address_width: t[36] as u32 + 1, flags: t[37], ..Dmar::default() };
    let mut o = HEADER_LEN;
    while o < t.len() {
        let (Some(ty), Some(len)) = (u16_at(t, o), u16_at(t, o + 2)) else {
            out.problems.push(Problem { what: "structure_cut", at: o as u64 });
            break;
        };
        let len = len as usize;
        if len == 0 {
            // dmar.c:591-594: "Invalid 0-length structure", the walk ends.
            out.problems.push(Problem { what: "zero_length", at: o as u64 });
            break;
        }
        if len < 4 || o + len > t.len() {
            // dmar.c:595-598: "Record passes table end".
            out.problems.push(Problem { what: "structure_length", at: o as u64 });
            break;
        }
        match ty {
            TYPE_DRHD => match drhd(&t[o..o + len]) {
                Some(u) if u.address == 0 => out.problems.push(Problem { what: "drhd_no_address", at: o as u64 }),
                Some(_) if d.units.len() >= MAX_UNITS => {
                    out.problems.push(Problem { what: "too_many_units", at: o as u64 });
                    break;
                }
                Some(u) => d.units.push(u),
                None => out.problems.push(Problem { what: "drhd_short", at: o as u64 }),
            },
            TYPE_RMRR => d.rmrr += 1,
            TYPE_ATSR => d.atsr += 1,
            TYPE_RHSA => d.rhsa += 1,
            TYPE_ANDD => d.andd += 1,
            TYPE_SATC => d.satc += 1,
            TYPE_SIDP => d.sidp += 1,
            _ => d.unknown = d.unknown.saturating_add(1),
        }
        o += len;
    }
    out.dmar = Some(d);
    out
}

fn drhd(s: &[u8]) -> Option<Drhd> {
    if s.len() < DRHD_LEN {
        return None;
    }
    Some(Drhd { flags: s[4], size: s[5], segment: u16_at(s, 6)?, address: u64_at(s, 8)? })
}

/// The registers of one unit as read (`pmen` only where the capability says the regions exist, as Linux reads
/// it, iommu.c:940-941).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnitRegs {
    pub ver: u32,
    pub cap: u64,
    pub ecap: u64,
    pub gsts: u32,
    pub pmen: Option<u32>,
}

/// Whether the protected-memory register is to be read on a unit with capability `cap`.
pub fn has_protected_regions(cap: u64) -> bool {
    cap & (CAP_PLMR | CAP_PHMR) != 0
}

/// Linux `init_translation_status` (iommu.c:229-235).
pub fn translation_on(gsts: u32) -> bool {
    gsts & GSTS_TES != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitState {
    /// The register page could not be read (the register guard refused it, or the mapping failed).
    Unreadable,
    /// Capability and extended capability both read all ones: nothing answers there (dmar.c:908-911).
    Absent,
    Present {
        version: (u8, u8),
        translation: bool,
        root_table_set: bool,
        queued_invalidation: bool,
        interrupt_remapping: bool,
        compat_interrupts: bool,
        protected_memory: bool,
    },
}

pub fn unit_state(regs: Option<UnitRegs>) -> UnitState {
    let Some(r) = regs else { return UnitState::Unreadable };
    if r.cap == u64::MAX && r.ecap == u64::MAX {
        return UnitState::Absent;
    }
    UnitState::Present {
        version: (((r.ver & 0xF0) >> 4) as u8, (r.ver & 0x0F) as u8),
        translation: translation_on(r.gsts),
        root_table_set: r.gsts & GSTS_RTPS != 0,
        queued_invalidation: r.gsts & GSTS_QIES != 0,
        interrupt_remapping: r.gsts & GSTS_IRES != 0,
        compat_interrupts: r.gsts & GSTS_CFIS != 0,
        protected_memory: r.pmen.is_some_and(|p| p & PMEN_PRS != 0),
    }
}

/// Whether drivers may let their devices do DMA.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaVerdict {
    /// No DMAR table: nothing here could be translating (the Mini-PC, every AMD machine, plain QEMU).
    NoTable,
    /// Every unit answered and none translates or protects memory.
    Free,
    /// The table is there but a unit's state is not known. DMA stays allowed: the hardware decides, and a boot
    /// that stops every driver over a register it could not read proves less than one that tries.
    Unknown,
    /// A unit translates: with no tables from BitsOS every DMA access faults in the IOMMU.
    TranslationOn,
    /// A protected memory region is on: DMA into it is blocked.
    ProtectedMemoryOn,
}

impl DmaVerdict {
    /// DMA drivers must not start.
    pub fn blocks_dma(self) -> bool {
        matches!(self, DmaVerdict::TranslationOn | DmaVerdict::ProtectedMemoryOn)
    }
    pub fn name(self) -> &'static str {
        match self {
            DmaVerdict::NoTable => "no_table",
            DmaVerdict::Free => "free",
            DmaVerdict::Unknown => "unknown",
            DmaVerdict::TranslationOn => "translation_on",
            DmaVerdict::ProtectedMemoryOn => "protected_memory_on",
        }
    }
}

/// The verdict over the units of a table (`None`: no table). Translation on any unit wins over everything, then
/// a protected region, then an unknown unit. A table that names no unit is unknown, not free.
pub fn verdict(units: Option<&[UnitState]>) -> DmaVerdict {
    let Some(units) = units else { return DmaVerdict::NoTable };
    let on = |f: fn(&UnitState) -> bool| units.iter().any(f);
    if on(|u| matches!(u, UnitState::Present { translation: true, .. })) {
        DmaVerdict::TranslationOn
    } else if on(|u| matches!(u, UnitState::Present { protected_memory: true, .. })) {
        DmaVerdict::ProtectedMemoryOn
    } else if units.is_empty() || on(|u| !matches!(u, UnitState::Present { .. })) {
        DmaVerdict::Unknown
    } else {
        DmaVerdict::Free
    }
}

/// Interrupt remapping left on without the compatibility format: interrupts BitsOS programs the plain way do not
/// arrive (VT-d spec 5.1.4). Not a DMA matter, but the report says it.
pub fn blocks_plain_interrupts(u: &UnitState) -> bool {
    matches!(u, UnitState::Present { interrupt_remapping: true, compat_interrupts: false, .. })
}

/// Builds DMAR bytes for tests of the wiring crates (valid checksum).
pub struct DmarBuilder {
    body: Vec<u8>,
}

impl DmarBuilder {
    pub fn new(host_address_width: u8, flags: u8) -> DmarBuilder {
        let mut body = alloc::vec![host_address_width - 1, flags];
        body.extend_from_slice(&[0; 10]);
        DmarBuilder { body }
    }
    /// A hardware unit with `scope` bytes of device scope after it.
    pub fn drhd(mut self, flags: u8, size: u8, segment: u16, address: u64, scope: &[u8]) -> DmarBuilder {
        self.body.extend_from_slice(&TYPE_DRHD.to_le_bytes());
        self.body.extend_from_slice(&((DRHD_LEN + scope.len()) as u16).to_le_bytes());
        self.body.extend_from_slice(&[flags, size]);
        self.body.extend_from_slice(&segment.to_le_bytes());
        self.body.extend_from_slice(&address.to_le_bytes());
        self.body.extend_from_slice(scope);
        self
    }
    /// Any other structure: `payload` after the 4-byte header.
    pub fn other(mut self, ty: u16, payload: &[u8]) -> DmarBuilder {
        self.body.extend_from_slice(&ty.to_le_bytes());
        self.body.extend_from_slice(&((4 + payload.len()) as u16).to_le_bytes());
        self.body.extend_from_slice(payload);
        self
    }
    /// Raw bytes (malformed structures).
    pub fn raw(mut self, bytes: &[u8]) -> DmarBuilder {
        self.body.extend_from_slice(bytes);
        self
    }
    pub fn build(self) -> Vec<u8> {
        let mut t = Vec::with_capacity(36 + self.body.len());
        t.extend_from_slice(b"DMAR");
        t.extend_from_slice(&((36 + self.body.len()) as u32).to_le_bytes());
        t.extend_from_slice(&[1, 0]);
        t.extend_from_slice(b"ALASKA");
        t.extend_from_slice(b"A M I \0\0");
        t.extend_from_slice(&0x0107_2009u32.to_le_bytes());
        t.extend_from_slice(b"AMI ");
        t.extend_from_slice(&0x0100_0013u32.to_le_bytes());
        t.extend_from_slice(&self.body);
        let sum = t.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        t[9] = 0u8.wrapping_sub(sum);
        t
    }
}

/// The Gaming-PC's DMAR shape as Linux's journal shows it (11.11.2026): 152 bytes, host address width 42, the
/// platform opt-in flag, a unit at 0xfc800000 (flags 0, the graphics device) and one at 0xfc810000 (include-all,
/// with the I/O APIC and the HPET in its scope), one SATC, no RMRR, no ATSR. The device-scope bytes, how the 104
/// bytes split over the three structures and the interrupt-remap flag are not readable without root: they are
/// made up so that the total and everything Linux printed fit.
pub fn gaming_pc() -> Vec<u8> {
    // One endpoint scope (type 1, length 8, bus 0, path 02.0); an I/O APIC (type 3, id 2) and an HPET (type 4).
    let gfx = [1, 8, 0, 0, 0, 0, 2, 0];
    let all = [3, 8, 0, 0, 2, 0x80, 0x1E, 7, 4, 8, 0, 0, 0, 0x80, 0x1E, 6];
    DmarBuilder::new(42, FLAG_INTR_REMAP | FLAG_PLATFORM_OPT_IN)
        .drhd(0, 0, 0, 0xFC80_0000, &gfx)
        .drhd(DRHD_INCLUDE_ALL, 0, 0, 0xFC81_0000, &all)
        .other(TYPE_SATC, &[1, 0, 0, 0, /* five scopes */ 1, 8, 0, 0, 0, 0, 2, 0, 1, 8, 0, 0, 0, 0, 6, 0, 1, 8, 0, 0, 0, 0, 7, 0, 1, 8, 0, 0, 0, 0, 7, 1, 1, 8, 0, 0, 0, 0, 1, 0])
        .build()
}

#[cfg(test)]
mod tests;
