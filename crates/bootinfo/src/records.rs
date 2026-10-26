//! Record tags and typed views. Every view checks the payload length; fields added later are read only when
//! the payload is long enough (`size` grows, old readers ignore the tail).

use crate::{rd_u16, rd_u32, rd_u64};

pub mod tag {
    pub const MEMORY_MAP: u32 = 1;
    pub const RSDP: u32 = 2;
    pub const SMBIOS: u32 = 3;
    pub const FRAMEBUFFER: u32 = 4;
    pub const MODE_LIST: u32 = 5;
    pub const CONFIG: u32 = 6;
    pub const STICK: u32 = 7;
    pub const LOADER_LOG: u32 = 8;
    pub const TSC: u32 = 9;
    pub const RELOAD: u32 = 10;
    pub const MODULE: u32 = 11;
    pub const CRASH_PAGE: u32 = 12;
    pub const TRAMPOLINE: u32 = 13;
    pub const RELOAD_STUB: u32 = 14;
    pub const KERNEL_IMAGE: u32 = 15;
    pub const GENERATION: u32 = 16;
    pub const PAGE_TABLES: u32 = 17;
    pub const BOOT_ID: u32 = 18;
    pub const FIRMWARE: u32 = 19;
    pub const HANDOVER: u32 = 20;
    /// DP-3 CR1 (INT-A2a): the monitor's EDID as the firmware gave it (EFI_EDID_ACTIVE_PROTOCOL, else
    /// EFI_EDID_DISCOVERED_PROTOCOL; the raw bytes, any length). bitsboot writes it from BT's follow-up on.
    pub const EDID: u32 = 21;
}

/// One UEFI memory descriptor, normalised to 40 bytes (type, phys, virt, pages, attributes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemDesc {
    pub ty: u32,
    pub phys: u64,
    pub virt: u64,
    pub pages: u64,
    pub attr: u64,
}
pub const MEMDESC_SIZE: usize = 40;

impl MemDesc {
    pub fn end(&self) -> u64 {
        self.phys + self.pages * 4096
    }
    pub(crate) fn read(b: &[u8]) -> Self {
        MemDesc { ty: rd_u32(b, 0), phys: rd_u64(b, 8), virt: rd_u64(b, 16), pages: rd_u64(b, 24), attr: rd_u64(b, 32) }
    }
    pub(crate) fn write(&self, b: &mut [u8]) {
        b[0..4].copy_from_slice(&self.ty.to_le_bytes());
        b[4..8].copy_from_slice(&0u32.to_le_bytes());
        b[8..16].copy_from_slice(&self.phys.to_le_bytes());
        b[16..24].copy_from_slice(&self.virt.to_le_bytes());
        b[24..32].copy_from_slice(&self.pages.to_le_bytes());
        b[32..40].copy_from_slice(&self.attr.to_le_bytes());
    }
}

/// Payload: `count u32, desc_size u32 (= 40), then count descriptors`. Sorted by `phys` (the writer sorts).
#[derive(Clone, Copy)]
pub struct MemoryMap<'a> {
    raw: &'a [u8],
    count: usize,
}

impl<'a> MemoryMap<'a> {
    pub(crate) fn parse(p: &'a [u8]) -> Option<Self> {
        if p.len() < 8 || rd_u32(p, 4) as usize != MEMDESC_SIZE {
            return None;
        }
        let count = rd_u32(p, 0) as usize;
        let need = 8 + count.checked_mul(MEMDESC_SIZE)?;
        if p.len() < need {
            return None;
        }
        Some(MemoryMap { raw: &p[8..need], count })
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn get(&self, i: usize) -> Option<MemDesc> {
        (i < self.count).then(|| MemDesc::read(&self.raw[i * MEMDESC_SIZE..]))
    }
    pub fn iter(&self) -> impl Iterator<Item = MemDesc> + 'a {
        let raw = self.raw;
        (0..self.count).map(move |i| MemDesc::read(&raw[i * MEMDESC_SIZE..]))
    }
    /// Highest physical address of any RAM descriptor (exclusive).
    pub fn ram_top(&self) -> u64 {
        self.iter().filter(|d| crate::memtype::is_ram(d.ty)).map(|d| d.end()).max().unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rsdp {
    pub phys: u64,
    pub revision: u32,
}
impl Rsdp {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 12).then(|| Rsdp { phys: rd_u64(p, 0), revision: rd_u32(p, 8) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Smbios {
    pub phys: u64,
    /// 2 or 3 (entry point kind).
    pub major: u32,
}
impl Smbios {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 12).then(|| Smbios { phys: rd_u64(p, 0), major: rd_u32(p, 8) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// Blue in the lowest byte (the Mini-PC's GOP).
    Bgrx = 0,
    Rgbx = 1,
    Bitmask = 2,
    /// No linear framebuffer: the kernel runs headless (serial + MCP).
    None = 3,
}
impl PixelFormat {
    pub fn from_u32(v: u32) -> Self {
        match v {
            0 => PixelFormat::Bgrx,
            1 => PixelFormat::Rgbx,
            2 => PixelFormat::Bitmask,
            _ => PixelFormat::None,
        }
    }
}

/// The scanout the firmware set up. `virt` is bitsboot's early mapping (write-combining once the kernel has
/// programmed the PAT; see CONTRACTS.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    pub phys: u64,
    pub virt: u64,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// Pixels per line (not bytes).
    pub stride: u32,
    pub format: PixelFormat,
    /// Only for `Bitmask`: red, green, blue, reserved.
    pub masks: [u32; 4],
    pub mode_index: u32,
}
pub const FRAMEBUFFER_SIZE: usize = 64;
impl Framebuffer {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= FRAMEBUFFER_SIZE).then(|| Framebuffer {
            phys: rd_u64(p, 0),
            virt: rd_u64(p, 8),
            size: rd_u64(p, 16),
            width: rd_u32(p, 24),
            height: rd_u32(p, 28),
            stride: rd_u32(p, 32),
            format: PixelFormat::from_u32(rd_u32(p, 36)),
            masks: [rd_u32(p, 40), rd_u32(p, 44), rd_u32(p, 48), rd_u32(p, 52)],
            mode_index: rd_u32(p, 56),
        })
    }
    pub(crate) fn encode(&self) -> [u8; FRAMEBUFFER_SIZE] {
        let mut b = [0u8; FRAMEBUFFER_SIZE];
        b[0..8].copy_from_slice(&self.phys.to_le_bytes());
        b[8..16].copy_from_slice(&self.virt.to_le_bytes());
        b[16..24].copy_from_slice(&self.size.to_le_bytes());
        b[24..28].copy_from_slice(&self.width.to_le_bytes());
        b[28..32].copy_from_slice(&self.height.to_le_bytes());
        b[32..36].copy_from_slice(&self.stride.to_le_bytes());
        b[36..40].copy_from_slice(&(self.format as u32).to_le_bytes());
        for (i, m) in self.masks.iter().enumerate() {
            b[40 + i * 4..44 + i * 4].copy_from_slice(&m.to_le_bytes());
        }
        b[56..60].copy_from_slice(&self.mode_index.to_le_bytes());
        b
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub stride: u32,
}
pub const MODE_SIZE: usize = 20;

/// Payload: `count u32, current u32, then count modes (index, width, height, format, stride; u32 each)`.
#[derive(Clone, Copy)]
pub struct ModeList<'a> {
    raw: &'a [u8],
    count: usize,
    pub current: u32,
}
impl<'a> ModeList<'a> {
    pub(crate) fn parse(p: &'a [u8]) -> Option<Self> {
        if p.len() < 8 {
            return None;
        }
        let count = rd_u32(p, 0) as usize;
        let need = 8 + count.checked_mul(MODE_SIZE)?;
        (p.len() >= need).then(|| ModeList { raw: &p[8..need], count, current: rd_u32(p, 4) })
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = Mode> + 'a {
        let raw = self.raw;
        (0..self.count).map(move |i| {
            let b = &raw[i * MODE_SIZE..];
            Mode {
                index: rd_u32(b, 0),
                width: rd_u32(b, 4),
                height: rd_u32(b, 8),
                format: PixelFormat::from_u32(rd_u32(b, 12)),
                stride: rd_u32(b, 16),
            }
        })
    }
}

/// Where the loader came from (V1 rule: write only to the stick we booted from, `spec/core.md` §2.1).
#[derive(Clone, Copy, Debug)]
pub struct Stick<'a> {
    pub flags: u32,
    pub pci_seg: u16,
    pub pci_bus: u8,
    pub pci_dev: u8,
    pub pci_fn: u8,
    pub usb_vid: u16,
    pub usb_pid: u16,
    pub stick_id: [u8; 16],
    pub block0: [u8; 96],
    /// UEFI device path of the boot volume as text.
    pub device_path: &'a [u8],
}
pub mod stick_flags {
    pub const PCI_VALID: u32 = 1;
    pub const USB_VALID: u32 = 2;
    pub const BLOCK0_VALID: u32 = 4;
    pub const STICK_ID_VALID: u32 = 8;
    /// The ID came from V1's `\BITSOS\STICK.ID` (first V2 boot before the stick tool moved it).
    pub const STICK_ID_LEGACY: u32 = 16;
}
pub const STICK_FIXED_SIZE: usize = 136;
impl<'a> Stick<'a> {
    pub(crate) fn parse(p: &'a [u8]) -> Option<Self> {
        if p.len() < STICK_FIXED_SIZE {
            return None;
        }
        let mut stick_id = [0u8; 16];
        stick_id.copy_from_slice(&p[16..32]);
        let mut block0 = [0u8; 96];
        block0.copy_from_slice(&p[32..128]);
        let path_len = rd_u32(p, 128) as usize;
        let path_end = STICK_FIXED_SIZE.checked_add(path_len)?;
        if p.len() < path_end {
            return None;
        }
        Some(Stick {
            flags: rd_u32(p, 0),
            pci_seg: rd_u16(p, 4),
            pci_bus: p[6],
            pci_dev: p[7],
            pci_fn: p[8],
            usb_vid: rd_u16(p, 12),
            usb_pid: rd_u16(p, 14),
            stick_id,
            block0,
            device_path: &p[STICK_FIXED_SIZE..path_end],
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Tsc {
    /// The loader's rough value (UEFI Stall based; on the Mini-PC 1850 ppm off): a plausibility bound only.
    pub hint_hz: u64,
    /// A calibrated rate passed on by Reload (0 = cold boot: calibrate against the HPET).
    pub calibrated_hz: u64,
    pub source: u32,
    pub calibrated_at_reload: u32,
}
pub mod tsc_source {
    pub const NONE: u32 = 0;
    pub const UEFI_STALL: u32 = 1;
    pub const HPET: u32 = 2;
    pub const PM_TIMER: u32 = 3;
    pub const PIT: u32 = 4;
    pub const CPUID: u32 = 5;
}
impl Tsc {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 24).then(|| Tsc {
            hint_hz: rd_u64(p, 0),
            calibrated_hz: rd_u64(p, 8),
            source: rd_u32(p, 16),
            calibrated_at_reload: rd_u32(p, 20),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Reload {
    /// 0 = started by the firmware; n = the n-th Reload since then.
    pub counter: u32,
}
impl Reload {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 8).then(|| Reload { counter: rd_u32(p, 0) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module<'a> {
    pub kind: u32,
    pub phys: u64,
    pub size: u64,
    pub name: &'a [u8],
}
pub mod module_kind {
    pub const INITRAMFS: u32 = 1;
    pub const FIRMWARE: u32 = 2;
    pub const OTHER: u32 = 3;
}
impl<'a> Module<'a> {
    pub(crate) fn parse(p: &'a [u8]) -> Option<Self> {
        if p.len() < 28 {
            return None;
        }
        let name_len = rd_u32(p, 24) as usize;
        let end = 28usize.checked_add(name_len)?;
        (p.len() >= end).then(|| Module { kind: rd_u32(p, 0), phys: rd_u64(p, 8), size: rd_u64(p, 16), name: &p[28..end] })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysRange {
    pub phys: u64,
    pub size: u64,
}
impl PhysRange {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 16).then(|| PhysRange { phys: rd_u64(p, 0), size: rd_u64(p, 8) })
    }
}

/// AP start-up: a page below 1 MB and a PML4 copy below 4 GB, both `KEEP` (kept for Reload).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trampoline {
    pub page_phys: u64,
    pub low_pml4_phys: u64,
}
impl Trampoline {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 16).then(|| Trampoline { page_phys: rd_u64(p, 0), low_pml4_phys: rd_u64(p, 8) })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelImage {
    pub phys_base: u64,
    pub phys_size: u64,
    pub virt_base: u64,
    pub entry: u64,
}
impl KernelImage {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 32).then(|| KernelImage {
            phys_base: rd_u64(p, 0),
            phys_size: rd_u64(p, 8),
            virt_base: rd_u64(p, 16),
            entry: rd_u64(p, 24),
        })
    }
}

/// Which system generation `\BITSOS2\SYS\<n>\` was started, and why (V2-PLAN §7, B1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Generation {
    pub number: u32,
    pub reason: u32,
    /// For fallbacks: the generation that was skipped.
    pub skipped: u32,
    pub flags: u32,
}
pub mod gen_reason {
    /// G0 loader: the highest-numbered generation, no marker rule yet.
    pub const NEWEST_NO_RULE: u32 = 0;
    pub const NEWEST_OK: u32 = 1;
    pub const NEWEST_TRY: u32 = 2;
    pub const FALLBACK_AFTER_TRY: u32 = 3;
    pub const FALLBACK_BAD_FILES: u32 = 4;
    pub const PINNED: u32 = 5;
}
/// Bits of [`Generation::flags`] (bitsboot sets them; moved here from bitsboot's `gens::flag`).
pub mod gen_flag {
    /// `\BITSOS2\SYS\PIN` exists but could not be honoured (bad number, no such generation, not `OK`, bad files).
    pub const PIN_IGNORED: u32 = 1 << 0;
    /// An unconfirmed generation was started because no `OK` generation could be.
    pub const NO_CONFIRMED_FALLBACK: u32 = 1 << 1;
    /// Writing `TRY` failed (read-only stick or file system error).
    pub const MARKER_WRITE_FAILED: u32 = 1 << 2;
    /// The started generation carries no marker at all.
    pub const UNMARKED: u32 = 1 << 3;
    /// Besides `skipped`, further generations were passed over for bad files.
    pub const OTHERS_SKIPPED: u32 = 1 << 4;
    /// Safe mode (BT-5, the Gaming-PC): bitsboot wrote no marker and chose read-only.
    pub const SAFE_MODE: u32 = 1 << 5;
}
impl Generation {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 16).then(|| Generation {
            number: rd_u32(p, 0),
            reason: rd_u32(p, 4),
            skipped: rd_u32(p, 8),
            flags: rd_u32(p, 12),
        })
    }
}

/// The page tables live at hand-over (built by bitsboot, `BITS_PAGETABLE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageTables {
    pub pml4_phys: u64,
    pub direct_map_base: u64,
    /// Physical bytes covered by the direct map, from 0.
    pub direct_map_size: u64,
    pub kernel_virt_base: u64,
    pub vmap_base: u64,
}
impl PageTables {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 40).then(|| PageTables {
            pml4_phys: rd_u64(p, 0),
            direct_map_base: rd_u64(p, 8),
            direct_map_size: rd_u64(p, 16),
            kernel_virt_base: rd_u64(p, 24),
            vmap_base: rd_u64(p, 32),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Firmware<'a> {
    pub revision: u32,
    pub uefi_revision: u32,
    pub vendor: &'a [u8],
}
impl<'a> Firmware<'a> {
    pub(crate) fn parse(p: &'a [u8]) -> Option<Self> {
        if p.len() < 12 {
            return None;
        }
        let len = rd_u32(p, 8) as usize;
        let end = 12usize.checked_add(len)?;
        (p.len() >= end).then(|| Firmware { revision: rd_u32(p, 0), uefi_revision: rd_u32(p, 4), vendor: &p[12..end] })
    }
}

/// CPU state the firmware left, recorded before bitsboot changed anything (diagnostics, `hw_info`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Handover {
    pub cr0: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub efer: u64,
    pub flags: u64,
}
pub mod handover_flags {
    /// CET was on and bitsboot turned it off.
    pub const CET_TURNED_OFF: u64 = 1;
    /// bitsboot set EFER.NXE.
    pub const NXE_SET: u64 = 2;
    /// The direct map uses 1 GB pages.
    pub const GB_PAGES: u64 = 4;
}
impl Handover {
    pub(crate) fn parse(p: &[u8]) -> Option<Self> {
        (p.len() >= 40).then(|| Handover {
            cr0: rd_u64(p, 0),
            cr3: rd_u64(p, 8),
            cr4: rd_u64(p, 16),
            efer: rd_u64(p, 24),
            flags: rd_u64(p, 32),
        })
    }
}
