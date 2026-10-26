//! How many bytes the BootInfo block needs. Computed from what was actually loaded (no fixed share per
//! module or mode), with room for the log lines still to come and for memory map entries the firmware adds
//! before ExitBootServices (the map is written last, when no allocation is possible any more).

use bootinfo::{FRAMEBUFFER_SIZE, HEADER_SIZE, MEMDESC_SIZE, MODE_SIZE, STICK_FIXED_SIZE, Writer};

/// Log text written after the size is computed (a few lines; generous).
pub const LOG_MARGIN: usize = 32 * 1024;
/// Memory map entries the firmware may add between the count and ExitBootServices (allocations of bitsboot
/// itself split a few descriptors; firmware events at most a handful). Linux's stub reserves 32 (`EFI_MMAP_NR_SLACK_SLOTS`).
pub const MAP_SPARE: usize = 64;

pub struct Inputs<'a> {
    pub config: usize,
    pub log_so_far: usize,
    /// Name length of every module.
    pub module_names: &'a [usize],
    pub stick_path: usize,
    pub modes: usize,
    /// EDID bytes (0 = no record).
    pub edid: usize,
    pub vendor: usize,
    pub map_entries: usize,
}

fn rec(payload: usize) -> usize {
    (8 + payload + 7) & !7
}

/// Fixed-size records: HANDOVER 40, GENERATION 16, KERNEL_IMAGE 32, RSDP 16, SMBIOS 16, TSC 24, RELOAD 8,
/// CRASH_PAGE 16, TRAMPOLINE 16, RELOAD_STUB 16, PAGE_TABLES 40, BOOT_ID 16, FRAMEBUFFER.
fn fixed() -> usize {
    [40, 16, 32, 16, 16, 24, 8, 16, 16, 16, 40, 16, FRAMEBUFFER_SIZE].iter().map(|&p| rec(p)).sum()
}

pub fn capacity(i: &Inputs) -> usize {
    let modules: usize = i.module_names.iter().map(|&n| rec(28 + n)).sum();
    let total = HEADER_SIZE
        + fixed()
        + rec(12 + i.vendor)
        + rec(STICK_FIXED_SIZE + i.stick_path)
        + rec(8 + i.modes * MODE_SIZE)
        + rec(i.edid)
        + modules
        + rec(i.config)
        + rec(i.log_so_far + LOG_MARGIN)
        + Writer::memory_map_record_size(i.map_entries + MAP_SPARE)
        // Room for a record a later loader adds without touching this formula.
        + 4096;
    total.next_multiple_of(4096)
}

/// Whether the final memory map with `entries` descriptors fits into what is left.
pub fn map_fits(free: usize, entries: usize) -> bool {
    Writer::memory_map_record_size(entries) <= free
}

const _: () = assert!(MEMDESC_SIZE == 40);

#[cfg(test)]
mod tests {
    use super::*;
    use bootinfo::*;
    use std::vec;
    use std::vec::Vec;

    /// Writes every record bitsboot writes, at the sizes given, into a buffer of `capacity` bytes.
    fn fill(i: &Inputs, log_len: usize, map_entries: usize) -> Result<usize, WriteError> {
        let cap = capacity(i);
        let mut buf = vec![0u8; cap];
        let mut w = Writer::new(&mut buf)?;
        w.handover(Handover::default())?;
        w.generation(Generation { number: 1, reason: 0, skipped: 0, flags: 0 })?;
        w.kernel_image(KernelImage { phys_base: 0, phys_size: 0, virt_base: 0, entry: 0 })?;
        let names: Vec<Vec<u8>> = i.module_names.iter().map(|&n| vec![b'm'; n]).collect();
        for n in &names {
            w.module(&Module { kind: module_kind::FIRMWARE, phys: 0, size: 0, name: n })?;
        }
        w.config(&vec![b'#'; i.config])?;
        let path = vec![b'p'; i.stick_path];
        w.stick(&Stick {
            flags: 0, pci_seg: 0, pci_bus: 0, pci_dev: 0, pci_fn: 0, usb_vid: 0, usb_pid: 0,
            stick_id: [0; 16], block0: [0; 96], device_path: &path,
        })?;
        let fb = Framebuffer { phys: 1, virt: 1, size: 1, width: 1, height: 1, stride: 1, format: PixelFormat::Bgrx, masks: [0; 4], mode_index: 0 };
        w.framebuffer(&fb)?;
        let m = Mode { index: 0, width: 1, height: 1, format: PixelFormat::Bgrx, stride: 1 };
        w.mode_list(0, core::iter::repeat_n(m, i.modes))?;
        if i.edid > 0 {
            w.edid(&vec![0u8; i.edid])?;
        }
        w.rsdp(Rsdp { phys: 0, revision: 2 })?;
        w.smbios(Smbios { phys: 0, major: 3 })?;
        w.firmware(0, 0, &vec![b'v'; i.vendor])?;
        w.boot_id([0; 16])?;
        w.tsc(Tsc::default())?;
        w.reload(Reload::default())?;
        w.crash_page(PhysRange { phys: 0, size: 0 })?;
        w.trampoline(Trampoline { page_phys: 0, low_pml4_phys: 0 })?;
        w.reload_stub(PhysRange { phys: 0, size: 0 })?;
        w.page_tables(PageTables { pml4_phys: 0, direct_map_base: 0, direct_map_size: 0, kernel_virt_base: 0, vmap_base: 0 })?;
        w.loader_log(&vec![b'l'; log_len])?;
        let d = MemDesc { ty: 7, phys: 0, virt: 0, pages: 1, attr: 0 };
        w.memory_map(core::iter::repeat_n(d, map_entries))?;
        Ok(w.len())
    }

    #[test]
    fn everything_fits_with_the_margins_used_up() {
        for (config, mods, path, modes, map, edid) in
            [(0, 0, 0, 0, 0, 0), (415, 3, 40, 30, 117, 256), (16 << 20, 40, 300, 400, 900, crate::edid::MAX)]
        {
            let names: Vec<usize> = (0..mods).map(|k| 1 + k * 7).collect();
            let i = Inputs { config, log_so_far: 5000, module_names: &names, stick_path: path, modes, edid, vendor: 30, map_entries: map };
            assert!(fill(&i, 5000 + LOG_MARGIN, map + MAP_SPARE).is_ok(), "config {config}");
            assert_eq!(capacity(&i) % 4096, 0);
        }
    }

    #[test]
    fn map_check() {
        assert!(map_fits(Writer::memory_map_record_size(100), 100));
        assert!(!map_fits(Writer::memory_map_record_size(100), 101));
    }
}
