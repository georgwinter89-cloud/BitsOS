//! AP real-mode trampoline (owner: CP, item CP-4; spec/core.md §6.1, V1 `cpu/trampoline.asm`). Uses the `KEEP`
//! page below 1 MiB and the low PML4 page (below 4 GiB) that bitsboot records in BootInfo's TRAMPOLINE record;
//! both stay reserved for Reload.
//!
//! The code below is copied into the page once (`install`); the SIPI vector is the page number, so an AP starts
//! in real mode at `page:0`. It is position independent: the 16-bit part takes the page base from CS, loads the
//! small GDT inside the page (real mode cannot reach anything above 1 MiB, so the GDT pointer is part of the
//! page), switches to protected mode, sets CR4.PAE, CR3 = the low PML4 (loaded as 32 bits, hence below 4 GiB),
//! EFER.LME (and NXE before the first page walk), then **CR0 whole** (PG|PE with CD/NW clear: after INIT Intel
//! leaves CD/NW as they were and QEMU sets them) and far-jumps into 64-bit code in the same page, which reads
//! the per-AP parameters (stack, argument, kernel CR3, entry) and jumps to `hal_ap_entry` in the kernel half.
//! The low PML4 maps the trampoline page 1:1 and shares the kernel half of the kernel's root.
//!
//! The page holds one AP's parameters at a time: `smp` starts APs one after the other (under its start lock) and
//! rewrites them only after the previous AP has left the page (`alive` = 2) or was parked with INIT and given
//! time for it to land.

use crate::paging::MapError;
#[cfg(target_os = "none")]
use crate::paging::{self, PageFlags};

/// Bytes `alive` takes: 1 once the 16-bit code runs (a sign of life after SIPI), 2 once the 64-bit code has
/// read its parameters and leaves the page.
pub const ALIVE_REAL_MODE: u8 = 1;
pub const ALIVE_LEFT: u8 = 2;

/// What one AP needs from the page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Params {
    /// The low PML4 (below 4 GiB).
    pub pml4: u64,
    /// OR-ed into EFER in protected mode: LME, and NXE when the BSP uses NX.
    pub efer_or: u64,
    /// CR0 written whole (PE|PG, CD/NW clear).
    pub cr0: u64,
    /// The AP's first stack (top, kernel half).
    pub stack: u64,
    /// First argument of the kernel entry (`rdi`).
    pub arg: u64,
    /// The kernel's root, loaded by `hal_ap_entry` once it runs in the kernel half.
    pub cr3: u64,
    /// `hal_ap_entry`.
    pub entry: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrampolineError {
    /// The page is not 4 KiB aligned or not below 1 MiB (the SIPI vector is the page number).
    PageNotBelow1M(u64),
    /// The low PML4 is not 4 KiB aligned or not below 4 GiB (protected mode loads CR3 as 32 bits).
    Pml4NotBelow4G(u64),
    /// The code does not fit the page (cannot happen with this code; checked anyway).
    TooBig(usize),
    Map(MapError),
    /// Host build.
    NotSupported,
}

/// Checks the two pages from BootInfo (pure).
pub fn check_pages(page: u64, low_pml4: u64) -> Result<(), TrampolineError> {
    if page & 0xFFF != 0 || page == 0 || page >= 0x10_0000 {
        return Err(TrampolineError::PageNotBelow1M(page));
    }
    if low_pml4 & 0xFFF != 0 || low_pml4 == 0 || low_pml4 >= 0x1_0000_0000 {
        return Err(TrampolineError::Pml4NotBelow4G(low_pml4));
    }
    Ok(())
}

/// The SIPI vector for `page` (its page number; `check_pages` guarantees it fits 8 bits).
pub fn sipi_vector(page: u64) -> u8 {
    (page >> 12) as u8
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .text
    .p2align 4
    .global hal_tramp_start
hal_tramp_start:
    .code16
    cli
    cld
    movw %cs, %ax
    movw %ax, %ds
    movb $1, (hal_tramp_alive - hal_tramp_start)
    movzwl %ax, %esi
    shll $4, %esi
    lgdtl (hal_tramp_gdtr - hal_tramp_start)
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl *(hal_tramp_far32 - hal_tramp_start)

    .code32
    .global hal_tramp_32
hal_tramp_32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movl $0x20, %eax
    movl %eax, %cr4
    movl (hal_tramp_pml4 - hal_tramp_start)(%esi), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    rdmsr
    orl (hal_tramp_efer - hal_tramp_start)(%esi), %eax
    wrmsr
    movl (hal_tramp_cr0 - hal_tramp_start)(%esi), %eax
    movl %eax, %cr0
    ljmpl *(hal_tramp_far64 - hal_tramp_start)(%esi)

    .code64
    .global hal_tramp_64
hal_tramp_64:
    movl %esi, %esi
    movq (hal_tramp_stack - hal_tramp_start)(%rsi), %rsp
    movq (hal_tramp_arg - hal_tramp_start)(%rsi), %rdi
    movq (hal_tramp_cr3 - hal_tramp_start)(%rsi), %rdx
    movq (hal_tramp_entry - hal_tramp_start)(%rsi), %rax
    movb $2, (hal_tramp_alive - hal_tramp_start)(%rsi)
    jmpq *%rax

    .p2align 3
    .global hal_tramp_gdt
hal_tramp_gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
    .global hal_tramp_gdtr
hal_tramp_gdtr:
    .word 31
    .long 0
    .p2align 2
    .global hal_tramp_far32
hal_tramp_far32:
    .long 0
    .word 0x08
    .p2align 2
    .global hal_tramp_far64
hal_tramp_far64:
    .long 0
    .word 0x18
    .p2align 3
    .global hal_tramp_pml4
hal_tramp_pml4:
    .quad 0
    .global hal_tramp_efer
hal_tramp_efer:
    .quad 0
    .global hal_tramp_cr0
hal_tramp_cr0:
    .quad 0
    .global hal_tramp_stack
hal_tramp_stack:
    .quad 0
    .global hal_tramp_arg
hal_tramp_arg:
    .quad 0
    .global hal_tramp_cr3
hal_tramp_cr3:
    .quad 0
    .global hal_tramp_entry
hal_tramp_entry:
    .quad 0
    .global hal_tramp_alive
hal_tramp_alive:
    .byte 0
    .global hal_tramp_end
hal_tramp_end:
    .p2align 4

    .global hal_ap_entry
hal_ap_entry:
    movq %rdx, %cr3
    xorl %ebp, %ebp
    call hal_ap_main
    ud2
"#,
    options(att_syntax)
);

#[cfg(target_os = "none")]
mod imp {
    use super::*;

    unsafe extern "C" {
        static hal_tramp_start: u8;
        static hal_tramp_end: u8;
        static hal_tramp_gdt: u8;
        static hal_tramp_gdtr: u8;
        static hal_tramp_far32: u8;
        static hal_tramp_far64: u8;
        static hal_tramp_32: u8;
        static hal_tramp_64: u8;
        static hal_tramp_pml4: u8;
        static hal_tramp_efer: u8;
        static hal_tramp_cr0: u8;
        static hal_tramp_stack: u8;
        static hal_tramp_arg: u8;
        static hal_tramp_cr3: u8;
        static hal_tramp_entry: u8;
        static hal_tramp_alive: u8;
        fn hal_ap_entry();
    }

    fn off(sym: *const u8) -> usize {
        sym as usize - (&raw const hal_tramp_start) as usize
    }

    fn page_ptr(page: u64, offset: usize) -> *mut u8 {
        (crate::phys_to_virt(page) as *mut u8).wrapping_add(offset)
    }

    pub fn code_len() -> usize {
        off(&raw const hal_tramp_end)
    }

    pub fn entry() -> u64 {
        hal_ap_entry as *const () as u64
    }

    pub fn install(page: u64) -> Result<(), TrampolineError> {
        let len = code_len();
        if len > 4096 {
            return Err(TrampolineError::TooBig(len));
        }
        let start = &raw const hal_tramp_start;
        // SAFETY: `page` is the KEEP page bitsboot reserved for the trampoline (checked by `check_pages`: below
        // 1 MiB, aligned), RAM in the direct map, owned by nobody else; `len` bytes of the code fit the page.
        // The 32-bit patches go to fixed offsets inside the copied code (the GDT pointer's base and the two far
        // pointers' offsets), all below `len`.
        unsafe {
            core::ptr::copy_nonoverlapping(start, page_ptr(page, 0), len);
            let p32 = |o: usize, v: u64| page_ptr(page, o).cast::<u32>().write_unaligned(v as u32);
            p32(off(&raw const hal_tramp_gdtr) + 2, page + off(&raw const hal_tramp_gdt) as u64);
            p32(off(&raw const hal_tramp_far32), page + off(&raw const hal_tramp_32) as u64);
            p32(off(&raw const hal_tramp_far64), page + off(&raw const hal_tramp_64) as u64);
        }
        Ok(())
    }

    pub fn set_params(page: u64, p: &Params) {
        // SAFETY: the installed trampoline page (see `install`); the parameter slots are 8-byte aligned fields
        // of the copied code, and no AP reads them now: `smp` calls this only under its start lock (one start at
        // a time), after the previous AP left the page (`alive` 2) or was sent INIT and given `INIT_DELAY_NS` for
        // it to land.
        unsafe {
            let w = |o: usize, v: u64| page_ptr(page, o).cast::<u64>().write_volatile(v);
            w(off(&raw const hal_tramp_pml4), p.pml4);
            w(off(&raw const hal_tramp_efer), p.efer_or);
            w(off(&raw const hal_tramp_cr0), p.cr0);
            w(off(&raw const hal_tramp_stack), p.stack);
            w(off(&raw const hal_tramp_arg), p.arg);
            w(off(&raw const hal_tramp_cr3), p.cr3);
            w(off(&raw const hal_tramp_entry), p.entry);
            page_ptr(page, off(&raw const hal_tramp_alive)).write_volatile(0);
        }
    }

    pub fn alive(page: u64) -> u8 {
        // SAFETY: one byte of the installed page, written by the AP's trampoline code; a volatile read.
        unsafe { page_ptr(page, off(&raw const hal_tramp_alive)).read_volatile() }
    }

    pub fn build_low_pml4(low_pml4: u64, page: u64, kernel_root: u64, alloc_table: &mut dyn FnMut() -> Option<u64>) -> Result<(), TrampolineError> {
        // SAFETY: `low_pml4` is bitsboot's KEEP page for exactly this (checked by `check_pages`), RAM in the direct
        // map; the kernel root is live. The user half is cleared, the kernel half copied (its PDPTs are shared, see
        // `paging::populate_kernel_half`, so later kernel mappings such as AP stacks are visible through it).
        unsafe {
            let dst = crate::phys_to_virt(low_pml4) as *mut u64;
            core::ptr::write_bytes(dst, 0, 256);
            core::ptr::copy_nonoverlapping((crate::phys_to_virt(kernel_root) as *const u64).add(256), dst.add(256), 256);
        }
        // Executable (no NX), writable (the 64-bit code writes `alive`): only the AP's first instructions use it.
        match paging::map_4k(low_pml4, page, page, PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE), alloc_table) {
            Ok(()) | Err(MapError::AlreadyMapped) => Ok(()),
            Err(e) => Err(TrampolineError::Map(e)),
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::*;
    pub fn code_len() -> usize {
        0
    }
    pub fn entry() -> u64 {
        0
    }
    pub fn install(_page: u64) -> Result<(), TrampolineError> {
        Err(TrampolineError::NotSupported)
    }
    pub fn set_params(_page: u64, _p: &Params) {}
    pub fn alive(_page: u64) -> u8 {
        0
    }
    pub fn build_low_pml4(_low_pml4: u64, _page: u64, _kernel_root: u64, _alloc_table: &mut dyn FnMut() -> Option<u64>) -> Result<(), TrampolineError> {
        Err(TrampolineError::NotSupported)
    }
}

/// Bytes of trampoline code copied into the page.
pub use imp::code_len;
#[allow(unused_imports)]
pub(crate) use imp::{alive, build_low_pml4, entry, install, set_params};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_checked() {
        assert!(check_pages(0x8000, 0x7FFF_F000).is_ok());
        assert_eq!(check_pages(0x10_0000, 0x1000), Err(TrampolineError::PageNotBelow1M(0x10_0000)));
        assert_eq!(check_pages(0x8800, 0x1000), Err(TrampolineError::PageNotBelow1M(0x8800)));
        assert_eq!(check_pages(0, 0x1000), Err(TrampolineError::PageNotBelow1M(0)));
        assert_eq!(check_pages(0x8000, 0x1_0000_0000), Err(TrampolineError::Pml4NotBelow4G(0x1_0000_0000)));
        assert_eq!(check_pages(0x8000, 0x1234), Err(TrampolineError::Pml4NotBelow4G(0x1234)));
        assert_eq!(sipi_vector(0x9F000), 0x9F);
    }
}
