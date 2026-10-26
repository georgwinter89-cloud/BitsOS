//! Writing BootInfo into a caller-provided buffer (no allocation: bitsboot writes into `BITS_KEEP` pages, and
//! after ExitBootServices it can no longer allocate). Records are appended in order; the memory map is usually
//! the last one, written after ExitBootServices into space reserved before.

use crate::records::*;
use crate::{HEADER_SIZE, MAGIC, VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The buffer is too small for the next record.
    Full { needed: usize, free: usize },
}

pub struct Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
    count: u32,
}

impl<'a> Writer<'a> {
    /// Starts a new BootInfo in `buf` (zeroes the header).
    pub fn new(buf: &'a mut [u8]) -> Result<Self, WriteError> {
        if buf.len() < HEADER_SIZE {
            return Err(WriteError::Full { needed: HEADER_SIZE, free: buf.len() });
        }
        buf[..HEADER_SIZE].fill(0);
        let mut w = Writer { buf, len: HEADER_SIZE, count: 0 };
        w.sync_header();
        Ok(w)
    }

    /// Continues an existing BootInfo (Reload appends or replaces records in the kept block).
    pub fn resume(buf: &'a mut [u8]) -> Option<Self> {
        let total = crate::BootInfo::peek_total_size(buf)?;
        let count = crate::rd_u32(buf, 24);
        (total <= buf.len()).then_some(Writer { buf, len: total, count })
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn free(&self) -> usize {
        self.buf.len() - self.len
    }

    fn sync_header(&mut self) {
        let cap = self.buf.len() as u32;
        let b = &mut self.buf[..HEADER_SIZE];
        b[0..8].copy_from_slice(&MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        b[16..20].copy_from_slice(&(self.len as u32).to_le_bytes());
        b[20..24].copy_from_slice(&cap.to_le_bytes());
        b[24..28].copy_from_slice(&self.count.to_le_bytes());
        b[28..32].copy_from_slice(&0u32.to_le_bytes());
    }

    /// Appends a record whose payload is produced by `fill` into a slice of exactly `size` bytes.
    pub fn record_with(&mut self, tag: u32, size: usize, fill: impl FnOnce(&mut [u8])) -> Result<(), WriteError> {
        let padded = (8 + size + 7) & !7;
        if padded > self.free() {
            return Err(WriteError::Full { needed: padded, free: self.free() });
        }
        let start = self.len;
        self.buf[start..start + 4].copy_from_slice(&tag.to_le_bytes());
        self.buf[start + 4..start + 8].copy_from_slice(&(size as u32).to_le_bytes());
        let payload = &mut self.buf[start + 8..start + 8 + size];
        payload.fill(0);
        fill(payload);
        self.buf[start + 8 + size..start + padded].fill(0);
        self.len += padded;
        self.count += 1;
        self.sync_header();
        Ok(())
    }

    pub fn record(&mut self, tag: u32, payload: &[u8]) -> Result<(), WriteError> {
        self.record_with(tag, payload.len(), |p| p.copy_from_slice(payload))
    }

    /// Bytes a memory map record with `count` descriptors needs (reserve this before ExitBootServices).
    pub fn memory_map_record_size(count: usize) -> usize {
        (8 + 8 + count * MEMDESC_SIZE + 7) & !7
    }

    /// Writes the memory map, sorted by physical address.
    pub fn memory_map(&mut self, descs: impl Iterator<Item = MemDesc> + Clone) -> Result<(), WriteError> {
        let count = descs.clone().count();
        self.record_with(tag::MEMORY_MAP, 8 + count * MEMDESC_SIZE, |p| {
            p[0..4].copy_from_slice(&(count as u32).to_le_bytes());
            p[4..8].copy_from_slice(&(MEMDESC_SIZE as u32).to_le_bytes());
            for (i, d) in descs.enumerate() {
                d.write(&mut p[8 + i * MEMDESC_SIZE..8 + (i + 1) * MEMDESC_SIZE]);
            }
            // Insertion sort in place by phys: the firmware's map is nearly sorted and has ~100 entries.
            let n = count;
            for i in 1..n {
                let mut j = i;
                while j > 0 {
                    let a = MemDesc::read(&p[8 + (j - 1) * MEMDESC_SIZE..]);
                    let b = MemDesc::read(&p[8 + j * MEMDESC_SIZE..]);
                    if a.phys <= b.phys {
                        break;
                    }
                    b.write(&mut p[8 + (j - 1) * MEMDESC_SIZE..8 + j * MEMDESC_SIZE]);
                    a.write(&mut p[8 + j * MEMDESC_SIZE..8 + (j + 1) * MEMDESC_SIZE]);
                    j -= 1;
                }
            }
        })
    }

    pub fn rsdp(&mut self, r: Rsdp) -> Result<(), WriteError> {
        self.record_with(tag::RSDP, 16, |p| {
            p[0..8].copy_from_slice(&r.phys.to_le_bytes());
            p[8..12].copy_from_slice(&r.revision.to_le_bytes());
        })
    }

    pub fn smbios(&mut self, s: Smbios) -> Result<(), WriteError> {
        self.record_with(tag::SMBIOS, 16, |p| {
            p[0..8].copy_from_slice(&s.phys.to_le_bytes());
            p[8..12].copy_from_slice(&s.major.to_le_bytes());
        })
    }

    pub fn framebuffer(&mut self, fb: &Framebuffer) -> Result<(), WriteError> {
        self.record(tag::FRAMEBUFFER, &fb.encode())
    }

    pub fn mode_list(&mut self, current: u32, modes: impl Iterator<Item = Mode> + Clone) -> Result<(), WriteError> {
        let count = modes.clone().count();
        self.record_with(tag::MODE_LIST, 8 + count * MODE_SIZE, |p| {
            p[0..4].copy_from_slice(&(count as u32).to_le_bytes());
            p[4..8].copy_from_slice(&current.to_le_bytes());
            for (i, m) in modes.enumerate() {
                let b = &mut p[8 + i * MODE_SIZE..8 + (i + 1) * MODE_SIZE];
                b[0..4].copy_from_slice(&m.index.to_le_bytes());
                b[4..8].copy_from_slice(&m.width.to_le_bytes());
                b[8..12].copy_from_slice(&m.height.to_le_bytes());
                b[12..16].copy_from_slice(&(m.format as u32).to_le_bytes());
                b[16..20].copy_from_slice(&m.stride.to_le_bytes());
            }
        })
    }

    pub fn config(&mut self, text: &[u8]) -> Result<(), WriteError> {
        self.record(tag::CONFIG, text)
    }

    /// The monitor's EDID bytes (DP-3).
    pub fn edid(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.record(tag::EDID, bytes)
    }

    pub fn loader_log(&mut self, text: &[u8]) -> Result<(), WriteError> {
        self.record(tag::LOADER_LOG, text)
    }

    pub fn stick(&mut self, s: &Stick<'_>) -> Result<(), WriteError> {
        let path = s.device_path;
        self.record_with(tag::STICK, STICK_FIXED_SIZE + path.len(), |p| {
            p[0..4].copy_from_slice(&s.flags.to_le_bytes());
            p[4..6].copy_from_slice(&s.pci_seg.to_le_bytes());
            p[6] = s.pci_bus;
            p[7] = s.pci_dev;
            p[8] = s.pci_fn;
            p[12..14].copy_from_slice(&s.usb_vid.to_le_bytes());
            p[14..16].copy_from_slice(&s.usb_pid.to_le_bytes());
            p[16..32].copy_from_slice(&s.stick_id);
            p[32..128].copy_from_slice(&s.block0);
            p[128..132].copy_from_slice(&(path.len() as u32).to_le_bytes());
            p[STICK_FIXED_SIZE..].copy_from_slice(path);
        })
    }

    pub fn tsc(&mut self, t: Tsc) -> Result<(), WriteError> {
        self.record_with(tag::TSC, 24, |p| {
            p[0..8].copy_from_slice(&t.hint_hz.to_le_bytes());
            p[8..16].copy_from_slice(&t.calibrated_hz.to_le_bytes());
            p[16..20].copy_from_slice(&t.source.to_le_bytes());
            p[20..24].copy_from_slice(&t.calibrated_at_reload.to_le_bytes());
        })
    }

    pub fn reload(&mut self, r: Reload) -> Result<(), WriteError> {
        self.record_with(tag::RELOAD, 8, |p| p[0..4].copy_from_slice(&r.counter.to_le_bytes()))
    }

    pub fn module(&mut self, m: &Module<'_>) -> Result<(), WriteError> {
        self.record_with(tag::MODULE, 28 + m.name.len(), |p| {
            p[0..4].copy_from_slice(&m.kind.to_le_bytes());
            p[8..16].copy_from_slice(&m.phys.to_le_bytes());
            p[16..24].copy_from_slice(&m.size.to_le_bytes());
            p[24..28].copy_from_slice(&(m.name.len() as u32).to_le_bytes());
            p[28..].copy_from_slice(m.name);
        })
    }

    fn phys_range(&mut self, t: u32, r: PhysRange) -> Result<(), WriteError> {
        self.record_with(t, 16, |p| {
            p[0..8].copy_from_slice(&r.phys.to_le_bytes());
            p[8..16].copy_from_slice(&r.size.to_le_bytes());
        })
    }
    pub fn crash_page(&mut self, r: PhysRange) -> Result<(), WriteError> {
        self.phys_range(tag::CRASH_PAGE, r)
    }
    pub fn reload_stub(&mut self, r: PhysRange) -> Result<(), WriteError> {
        self.phys_range(tag::RELOAD_STUB, r)
    }

    pub fn trampoline(&mut self, t: Trampoline) -> Result<(), WriteError> {
        self.record_with(tag::TRAMPOLINE, 16, |p| {
            p[0..8].copy_from_slice(&t.page_phys.to_le_bytes());
            p[8..16].copy_from_slice(&t.low_pml4_phys.to_le_bytes());
        })
    }

    pub fn kernel_image(&mut self, k: KernelImage) -> Result<(), WriteError> {
        self.record_with(tag::KERNEL_IMAGE, 32, |p| {
            p[0..8].copy_from_slice(&k.phys_base.to_le_bytes());
            p[8..16].copy_from_slice(&k.phys_size.to_le_bytes());
            p[16..24].copy_from_slice(&k.virt_base.to_le_bytes());
            p[24..32].copy_from_slice(&k.entry.to_le_bytes());
        })
    }

    pub fn generation(&mut self, g: Generation) -> Result<(), WriteError> {
        self.record_with(tag::GENERATION, 16, |p| {
            p[0..4].copy_from_slice(&g.number.to_le_bytes());
            p[4..8].copy_from_slice(&g.reason.to_le_bytes());
            p[8..12].copy_from_slice(&g.skipped.to_le_bytes());
            p[12..16].copy_from_slice(&g.flags.to_le_bytes());
        })
    }

    pub fn page_tables(&mut self, t: PageTables) -> Result<(), WriteError> {
        self.record_with(tag::PAGE_TABLES, 40, |p| {
            p[0..8].copy_from_slice(&t.pml4_phys.to_le_bytes());
            p[8..16].copy_from_slice(&t.direct_map_base.to_le_bytes());
            p[16..24].copy_from_slice(&t.direct_map_size.to_le_bytes());
            p[24..32].copy_from_slice(&t.kernel_virt_base.to_le_bytes());
            p[32..40].copy_from_slice(&t.vmap_base.to_le_bytes());
        })
    }

    pub fn boot_id(&mut self, id: [u8; 16]) -> Result<(), WriteError> {
        self.record(tag::BOOT_ID, &id)
    }

    pub fn firmware(&mut self, revision: u32, uefi_revision: u32, vendor: &[u8]) -> Result<(), WriteError> {
        self.record_with(tag::FIRMWARE, 12 + vendor.len(), |p| {
            p[0..4].copy_from_slice(&revision.to_le_bytes());
            p[4..8].copy_from_slice(&uefi_revision.to_le_bytes());
            p[8..12].copy_from_slice(&(vendor.len() as u32).to_le_bytes());
            p[12..].copy_from_slice(vendor);
        })
    }

    pub fn handover(&mut self, h: Handover) -> Result<(), WriteError> {
        self.record_with(tag::HANDOVER, 40, |p| {
            p[0..8].copy_from_slice(&h.cr0.to_le_bytes());
            p[8..16].copy_from_slice(&h.cr3.to_le_bytes());
            p[16..24].copy_from_slice(&h.cr4.to_le_bytes());
            p[24..32].copy_from_slice(&h.efer.to_le_bytes());
            p[32..40].copy_from_slice(&h.flags.to_le_bytes());
        })
    }
}
