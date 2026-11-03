//! RB-14 CR-3 (INT-W24): why the last start happened, from the AMD FCH, read only. A start right after a sync
//! flood must not spend a new generation's one try (`bitsboot_pure::gens::choose_after_flood`). The register is
//! the kernel's `fch_wdt::regs` PM `S5_RESET_STATUS` (ACPI MMIO 0xFED80000 + PM 0x300 + 0xC0); bitsboot never
//! writes it (the kernel's watchdog reads and logs it again).

use alloc::format;
use alloc::string::String;
use core::arch::asm;
use uefi::boot::{self, MemoryType};
use uefi::mem::memory_map::MemoryMap;

/// The FCH SMBus function 00:14.0 (1022:790B, rev >= 0x51: the EFCH with the MMIO window; fch_wdt::system).
const SMBUS_CF8: u32 = 0x8000_0000 | 0x14 << 11;
const FCH_ID: u32 = 0x790B_1022;
const FCH_MIN_REV: u32 = 0x51;
const S5_RESET_STATUS: u64 = 0xFED8_0000 + 0x300 + 0xC0;
/// Bits 27 (`sync_flood_error`) and 31 (`sync_flood_software`), as `fch_wdt::regs::S5_SYNC_FLOOD`.
pub const SYNC_FLOOD: u32 = 1 << 27 | 1 << 31;

fn pci_read32(reg: u32) -> u32 {
    let v: u32;
    // SAFETY: configuration mechanism #1 (0xCF8 address, 0xCFC data) exists on every x86 PC chipset (QEMU q35
    // too); boot services run one CPU without interrupt handlers that use it, and a read changes no device state.
    unsafe {
        asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") SMBUS_CF8 | reg, options(nomem, nostack));
        asm!("in eax, dx", in("dx") 0xCFCu16, out("eax") v, options(nomem, nostack));
    }
    v
}

/// `Ok(value)` of `S5_RESET_STATUS`, or `Err(why it was not read)`: no EFCH at 00:14.0 (QEMU, Intel), or the
/// register outside every descriptor of the UEFI memory map (then it may not be mapped, so it is not touched).
pub fn s5_reset_status() -> Result<u32, String> {
    let id = pci_read32(0);
    let rev = pci_read32(8) & 0xff;
    if id != FCH_ID || rev < FCH_MIN_REV {
        return Err(format!("00:14.0 is {:04x}:{:04x} rev {rev:#x}, not an FCH 1022:790b rev >= 0x51", id & 0xffff, id >> 16));
    }
    let map = boot::memory_map(MemoryType::LOADER_DATA).map_err(|e| format!("no memory map: {e:?}"))?;
    let page = 4096u64;
    if !map.entries().any(|d| d.phys_start <= S5_RESET_STATUS && S5_RESET_STATUS + 4 <= d.phys_start.saturating_add(d.page_count.saturating_mul(page))) {
        return Err(format!("{S5_RESET_STATUS:#x} is in no UEFI memory map descriptor"));
    }
    let mut v = 0u32;
    for i in 0..4u64 {
        // SAFETY: before ExitBootServices UEFI identity-maps all memory its map describes, and the 4 bytes lie in
        // one descriptor (checked above); byte reads, as the kernel's PM window reads them, have no side effect on
        // this sticky status register (only a write of 1 clears a bit).
        let b = unsafe { core::ptr::read_volatile((S5_RESET_STATUS + i) as *const u8) };
        v |= (b as u32) << (8 * i);
    }
    Ok(v)
}
