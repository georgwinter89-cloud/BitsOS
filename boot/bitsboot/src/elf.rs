//! Placing the checked kernel ELF (`bitsboot_pure::elf::parse`) in physical memory.

use crate::R;
use crate::mem::{self, PAGE};
use bitsboot_pure::elf::Elf;
use bootinfo::memtype;
use uefi::boot::AllocateType;

/// The kernel image as placed in physical memory.
pub struct Loaded {
    pub phys: u64,
    pub size: u64,
    pub virt: u64,
    pub entry: u64,
}

impl Loaded {
    pub fn phys_of(&self, vaddr: u64) -> u64 {
        self.phys + (vaddr - self.virt)
    }
}

/// Copies the segments into one contiguous `BITS_KERNEL` block covering their whole span (bss zeroed).
/// `parse` guaranteed: every segment lies inside the file and inside `[lo, hi)`, and `hi - lo` is bounded.
pub fn load(file: &[u8], elf: &Elf, what: &str) -> R<Loaded> {
    let size = elf.hi - elf.lo;
    let phys = mem::alloc(memtype::BITS_KERNEL, AllocateType::AnyPages, size / PAGE, what)?;
    mem::zero(phys, size);
    let img = Loaded { phys, size, virt: elf.lo, entry: elf.entry };
    for s in &elf.segs {
        let src = &file[s.offset as usize..(s.offset + s.filesz) as usize];
        mem::bytes(img.phys_of(s.vaddr), s.filesz).copy_from_slice(src);
    }
    Ok(img)
}
