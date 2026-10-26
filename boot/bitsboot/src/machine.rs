//! Which machine bitsboot runs on (BT-5): the CPU from CPUID and the DMI names from the firmware's SMBIOS table,
//! read before ExitBootServices while firmware memory is identity-mapped. The profile choice itself is pure
//! (`bitsboot_pure::profile`); this only gathers the facts and logs them.

use crate::{cpu, handover, mem};
use alloc::vec::Vec;
use bitsboot_pure::dmi::{self, Ident};
use bitsboot_pure::profile::Cpu;
use log::{info, warn};
use uefi::boot::{self, MemoryType};
use uefi::mem::memory_map::MemoryMap;
use uefi::table::cfg::ConfigTableEntry;

pub fn cpu() -> Cpu {
    let (_, b, c, d) = cpu::cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[..4].copy_from_slice(&b.to_le_bytes());
    vendor[4..8].copy_from_slice(&d.to_le_bytes());
    vendor[8..].copy_from_slice(&c.to_le_bytes());
    Cpu { vendor, hypervisor: cpu::hypervisor() }
}

/// Logs the CPU line the Gaming-PC check reads: vendor, family/model, hybrid (CPUID.7.0:EDX[15]).
pub fn log_cpu(c: &Cpu) {
    let (max, eax1) = (cpu::cpuid(0).0, cpu::cpuid(1).0);
    let (base_f, base_m) = ((eax1 >> 8) & 0xF, (eax1 >> 4) & 0xF);
    let family = if base_f == 0xF { base_f + ((eax1 >> 20) & 0xFF) } else { base_f };
    let model = if base_f == 0x6 || base_f == 0xF { base_m | ((eax1 >> 12) & 0xF0) } else { base_m };
    let hybrid = max >= 7 && cpu::cpuid(7).3 & (1 << 15) != 0;
    info!(
        "cpu: {} family {family:#x} model {model:#x} hybrid={} hypervisor={}",
        c.vendor_text(),
        if hybrid { "yes" } else { "no" },
        if c.hypervisor { "yes" } else { "no" }
    );
}

/// The bytes of firmware memory at `phys`, at most `want` of them and never past the end of the UEFI memory
/// map descriptor that holds `phys` (a lying SMBIOS length must not make bitsboot read unmapped memory).
fn firmware_bytes(phys: u64, want: u64) -> Option<Vec<u8>> {
    let map = boot::memory_map(MemoryType::LOADER_DATA).ok()?;
    let d = map.entries().find(|d| d.phys_start <= phys && phys < d.phys_start.saturating_add(d.page_count.saturating_mul(mem::PAGE)))?;
    let end = d.phys_start + d.page_count * mem::PAGE;
    let len = want.min(end - phys) as usize;
    let mut v = alloc::vec![0u8; len];
    // SAFETY: before ExitBootServices UEFI identity-maps all memory its map describes, and [phys, phys+len)
    // lies inside one descriptor of that map; the firmware does not free its configuration tables. The read is
    // a plain byte copy into our own buffer (no reference into firmware memory is kept).
    unsafe { core::ptr::copy_nonoverlapping(phys as *const u8, v.as_mut_ptr(), len) };
    Some(v)
}

/// The DMI names, or None when the firmware has no SMBIOS table or it cannot be read (logged).
pub fn dmi() -> Option<Ident> {
    let Some(at) = handover::config_table(ConfigTableEntry::SMBIOS3_GUID).or_else(|| handover::config_table(ConfigTableEntry::SMBIOS_GUID))
    else {
        info!("dmi: the firmware has no SMBIOS table");
        return None;
    };
    let Some(e) = firmware_bytes(at, dmi::ENTRY_MAX as u64).as_deref().and_then(dmi::entry) else {
        warn!("bitsboot dmi.bad_entry at={at:#x}: no valid SMBIOS entry point there");
        return None;
    };
    let Some(table) = firmware_bytes(e.table, e.len.min(dmi::TABLE_MAX)) else {
        warn!("bitsboot dmi.unmapped table={:#x} len={}: the SMBIOS table is not in the memory map", e.table, e.len);
        return None;
    };
    if (table.len() as u64) < e.len {
        warn!("bitsboot dmi.cut table={:#x} len={} readable={}: reading only the part the memory map holds (at most 1 MiB)", e.table, e.len, table.len());
    }
    let id = dmi::ident(&table, e.count);
    if id.is_empty() {
        warn!(
            "bitsboot dmi.unusable table={:#x} len={} count={:?}: no system or board names in the SMBIOS table",
            e.table,
            e.len,
            e.count
        );
    }
    info!("dmi: SMBIOS {}.{} {}", e.major, e.minor, id.text());
    Some(id)
}
