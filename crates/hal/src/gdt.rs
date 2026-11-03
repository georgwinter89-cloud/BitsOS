//! GDT, TSS and the IST stacks (owner: CP). Selector layout is a contract (SYSCALL/SYSRET depend on it), since
//! AB-10a Linux's 64-bit one for everything a program sees (linux-6.6/arch/x86/include/asm/segment.h:165-220):
//! 0x08 kernel code, 0x10 kernel data, 0x23 32-bit user code (`__USER32_CS`), 0x2B user data (`__USER_DS`),
//! 0x33 64-bit user code (`__USER_CS`), 0x40 TSS (16 bytes), 0x50 LDT (16 bytes, present only while a thread
//! of a process with an LDT runs), 0x63/0x6B/0x73 the three TLS slots of the running thread (`set_thread_area`).
//! Wine's WoW64 hard-codes 0x23 as its 32-bit code selector and reads the 64-bit ones from `%cs`/`%ss`.
//!
//! IST stacks (spec/core.md §6.2): #DF (IST1), NMI (IST2) and #MC (IST3) run on their own stacks, so a kernel
//! stack overflow or an NMI at any instant still has a good stack. Every CPU has its own GDT and TSS
//! (`CpuGdt`, reached through `CpuLocal.gdt`): the BSP's is static and starts on static boot stacks (no guard;
//! only used until memory exists), `install_guarded_ist` (CP's Core initcall) then moves them to stack frames
//! mapped in the CPU area at the top of vmap with an unmapped guard page below, so an overflow of an IST stack
//! faults instead of overwriting memory (V1 `cpu/apguard.asm` idea). Each AP gets its tables from
//! `new_ap_tables` with guarded IST stacks from the start (CP-4).

use crate::paging::MapError;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

pub const KERNEL_CS: u16 = 0x08;
pub const KERNEL_SS: u16 = 0x10;
/// 32-bit user code (compatibility mode), the SYSRET base of STAR.
pub const USER32_CS: u16 = 0x20 | 3;
/// User data and stack: SS of every program, DS/ES of 32-bit code.
pub const USER_DS: u16 = 0x28 | 3;
pub const USER_SS: u16 = USER_DS;
/// 64-bit user code.
pub const USER_CS: u16 = 0x30 | 3;
pub const TSS_SEL: u16 = 0x40;
pub const LDT_SEL: u16 = 0x50;
/// GDT index of the first TLS slot (`GDT_ENTRY_TLS_MIN`), three of them.
pub const TLS_FIRST: usize = 12;
pub const TLS_ENTRIES: usize = 3;
/// Slots in each CPU's GDT (Linux has 16 too; 15, its CPU-number segment, stays empty).
pub const GDT_ENTRIES: usize = 16;
const TSS_SLOT: usize = 8;
const LDT_SLOT: usize = 10;
/// The LDT limit V2 always loads: the whole architectural table (8192 entries). Entries a program never wrote
/// are zero, which faults like a selector beyond Linux's shorter limit (#GP with the selector).
pub const LDT_LIMIT: u64 = 8192 * 8 - 1;

/// IST indices (1-based as in the IDT gate).
pub const IST_DOUBLE_FAULT: u8 = 1;
pub const IST_NMI: u8 = 2;
pub const IST_MACHINE_CHECK: u8 = 3;
/// The IST slots this kernel uses (architectural: the TSS has seven).
pub const IST_USED: [u8; 3] = [IST_DOUBLE_FAULT, IST_NMI, IST_MACHINE_CHECK];

/// Pages per IST stack. 16 KiB: the #DF path formats a panic message with frame-pointer backtraces.
pub const IST_STACK_PAGES: u64 = 4;
/// Unmapped pages below each guarded IST stack.
pub const IST_GUARD_PAGES: u64 = 1;

/// The CPU area: the top of vmap, handed out top-down for per-CPU stacks (Linux's cpu_entry_area idea).
/// `mmio`'s bump grows up from the bottom of vmap and stops at the layout boundary.
pub const CPU_AREA_BASE: u64 = bootinfo::layout::CPU_AREA_BASE;
pub const CPU_AREA_SIZE: u64 = bootinfo::layout::VMAP_END - CPU_AREA_BASE;

#[repr(C, packed(4))]
pub struct Tss {
    _reserved0: u32,
    pub rsp: [u64; 3],
    _reserved1: u64,
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    pub iomap_base: u16,
}

impl Tss {
    const fn new(ist: [u64; 7]) -> Tss {
        Tss { _reserved0: 0, rsp: [0; 3], _reserved1: 0, ist, _reserved2: 0, _reserved3: 0, iomap_base: 104 }
    }
}

/// 64-byte alignment keeps the used IST slots 1-3 (bytes 36..60) inside one cache line, so the CPU never sees
/// a half-written pointer when `install_guarded_ist` replaces one (stores within a line are atomic).
#[repr(C, align(64))]
struct AlignedTss(Tss);

/// Byte offset of `ist[0]` in the TSS.
const TSS_IST_OFFSET: usize = 36;

/// One CPU's descriptor tables: GDT (16 slots, the TSS and LDT descriptors take two each), TSS and what its IST
/// slots hold.
pub struct CpuGdt {
    /// The descriptors as atomics (the same layout as `[u64; GDT_ENTRIES]`, what `lgdt` points at): since AB-10a
    /// the owning CPU rewrites TLS and LDT slots at every switch while `entry` may read them from any CPU.
    gdt: [AtomicU64; GDT_ENTRIES],
    tss: UnsafeCell<AlignedTss>,
    /// Per used IST slot: (guard or 0, bottom, top).
    ist: [[AtomicU64; 3]; 3],
    /// Base of the LDT this CPU's LDTR holds (0: the null LDTR). Only the owning CPU writes it.
    ldt_base: AtomicU64,
    /// The LDT generation (`entry::segs`) this CPU last compared with its running thread's LDT.
    pub(crate) ldt_seen: AtomicU64,
}

// SAFETY: the GDT slots are atomics (any CPU may read them; only the owning CPU stores, `fill` before loading and
// `set_tls_entry`/`load_ldt` after). The TSS is written only before the tables are loaded (`init`,
// `new_ap_tables`), then by the owning CPU (`set_rsp0`, one store, interrupts off at its callers) and by
// `install_guarded_ist` (one aligned store per IST slot within one cache line, interrupts off). Other CPUs read
// only atomics.
unsafe impl Sync for CpuGdt {}

impl CpuGdt {
    const fn new() -> CpuGdt {
        CpuGdt {
            gdt: [const { AtomicU64::new(0) }; GDT_ENTRIES],
            tss: UnsafeCell::new(AlignedTss(Tss::new([0; 7]))),
            ist: [const { [const { AtomicU64::new(0) }; 3] }; 3],
            ldt_base: AtomicU64::new(0),
            ldt_seen: AtomicU64::new(0),
        }
    }

    fn tss_base(&self) -> *mut u8 {
        self.tss.get().cast::<u8>()
    }

    /// Writes the GDT entries and a TSS with `ist` tops. Only before the tables are loaded on any CPU.
    fn fill(&self, ist: [u64; 7]) {
        // SAFETY: the caller guarantees the tables are not loaded yet (no CPU reads them), and this is the only
        // writer at this point (BSP start-up or a fresh AP table set).
        unsafe { self.tss.get().write(AlignedTss(Tss::new(ist))) };
        let tss_addr = self.tss_base() as u64;
        let limit = (core::mem::size_of::<Tss>() - 1) as u64;
        let mut gdt = [0u64; GDT_ENTRIES];
        gdt[1] = seg(0x9A, 0xA); // kernel code: present, ring 0, exec/read; L=1
        gdt[2] = seg(0x92, 0xC); // kernel data
        gdt[4] = seg(0xFA, 0xC); // 32-bit user code (ring 3, D=1, 4 GiB)
        gdt[5] = seg(0xF2, 0xC); // user data (ring 3)
        gdt[6] = seg(0xFA, 0xA); // 64-bit user code (ring 3, L=1)
        // TSS descriptor; base bits 24-31 matter (a truncated TSS base once reset V1 in a loop).
        [gdt[TSS_SLOT], gdt[TSS_SLOT + 1]] = system_desc(tss_addr, limit, 0x89);
        for (slot, d) in self.gdt.iter().zip(gdt) {
            slot.store(d, Ordering::Relaxed);
        }
    }

    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    fn record(&self, k: usize, s: &IstStack) {
        self.ist[k][0].store(s.guard.unwrap_or(0), Ordering::Relaxed);
        self.ist[k][1].store(s.bottom, Ordering::Relaxed);
        self.ist[k][2].store(s.top, Ordering::Release);
    }

    /// The IST stacks these tables hold (#DF, NMI, #MC).
    pub fn ist_stacks(&self) -> [IstStack; 3] {
        core::array::from_fn(|k| {
            let g = self.ist[k][0].load(Ordering::Relaxed);
            IstStack { ist: IST_USED[k], guard: (g != 0).then_some(g), bottom: self.ist[k][1].load(Ordering::Relaxed), top: self.ist[k][2].load(Ordering::Acquire) }
        })
    }

    /// The stack top the TSS holds for IST slot `ist` (1..=7; 0 otherwise).
    pub fn tss_ist(&self, ist: u8) -> u64 {
        if !(1..=7).contains(&ist) {
            return 0;
        }
        // SAFETY: inside this TSS (4-byte aligned by the architecture's layout); a plain read of a slot only
        // `fill`/`install_guarded_ist` write.
        unsafe { self.tss_base().add(TSS_IST_OFFSET + 8 * (ist as usize - 1)).cast::<u64>().read_unaligned() }
    }

    /// Points IST slot `ist` (1..=7) of this TSS at `top`; returns the old value (0 for a slot out of range).
    /// For the NMI hook (RB-12): while an NMI runs, a nested one must land below it, not on its saved state.
    pub fn set_tss_ist(&self, ist: u8, top: u64) -> u64 {
        if !(1..=7).contains(&ist) {
            return 0;
        }
        let old = self.tss_ist(ist);
        // SAFETY: inside this TSS (as `tss_ist`); one 8-byte store, and only this CPU's own NMI/#DF/#MC delivery
        // reads the slot, which cannot split an instruction.
        unsafe { self.tss_base().add(TSS_IST_OFFSET + 8 * (ist as usize - 1)).cast::<u64>().write_unaligned(top) };
        old
    }

    /// Address of the TSS (tests compare it with what `str`/the descriptor say).
    pub fn tss_addr(&self) -> u64 {
        self.tss_base() as u64
    }

    /// The descriptor in GDT slot `i` (0 beyond the table): what a selector load on this CPU reads there.
    pub fn entry(&self, i: usize) -> u64 {
        if i >= GDT_ENTRIES {
            return 0;
        }
        self.gdt[i].load(Ordering::Relaxed)
    }

    /// Writes TLS slot `i` (0..TLS_ENTRIES) with `desc`. Only the owning CPU with interrupts off (AB-10a: the
    /// running thread's slots, `entry::segs`): this CPU reads a GDT slot only when it loads a selector itself.
    pub(crate) fn set_tls_entry(&self, i: usize, desc: u64) {
        if i < TLS_ENTRIES {
            // One 8-byte store by the owning CPU, which cannot be in the middle of a selector load while it runs this.
            self.gdt[TLS_FIRST + i].store(desc, Ordering::Relaxed);
        }
    }

    /// Base of the LDT this CPU has loaded (0: the null LDTR).
    pub fn ldt_base(&self) -> u64 {
        self.ldt_base.load(Ordering::Relaxed)
    }

    /// Points this CPU's LDTR at a table of 8192 descriptors at `base` (kernel memory), or loads the null LDTR
    /// for 0. Only the owning CPU (`this()`), interrupts off.
    pub(crate) fn load_ldt(&self, base: u64) {
        let [lo, hi] = if base == 0 { [0, 0] } else { system_desc(base, LDT_LIMIT, 0x82) };
        // Stores by the owning CPU, which reads the slots only at the `lldt` below.
        self.gdt[LDT_SLOT].store(lo, Ordering::Relaxed);
        self.gdt[LDT_SLOT + 1].store(hi, Ordering::Relaxed);
        imp::lldt(if base == 0 { 0 } else { LDT_SEL });
        self.ldt_base.store(base, Ordering::Relaxed);
    }
}

const fn seg(access: u64, flags: u64) -> u64 {
    (access << 40) | (flags << 52) | (0xF << 48) | 0xFFFF
}

/// A 16-byte system descriptor (TSS 0x89, LDT 0x82): present, ring 0, base and limit split as the CPU wants.
const fn system_desc(base: u64, limit: u64, access: u64) -> [u64; 2] {
    let lo = (limit & 0xFFFF) | ((base & 0xFF_FFFF) << 16) | (access << 40) | (((limit >> 16) & 0xF) << 48) | (((base >> 24) & 0xFF) << 56);
    [lo, base >> 32]
}

static BSP: CpuGdt = CpuGdt::new();

/// The BSP's tables (static).
pub fn bsp_tables() -> &'static CpuGdt {
    &BSP
}

/// This CPU's tables.
pub fn this() -> &'static CpuGdt {
    let p = crate::percpu::this_gdt();
    if p == 0 {
        return &BSP;
    }
    // SAFETY: CpuLocal.gdt holds the address of `BSP` or of a leaked `CpuGdt` from `new_ap_tables`, set before
    // the block was published and never freed.
    unsafe { &*(p as *const CpuGdt) }
}

/// The tables of `cpu` (`None` if that CPU was never prepared).
pub fn of(cpu: usize) -> Option<&'static CpuGdt> {
    let p = crate::percpu::gdt_of(cpu);
    // SAFETY: as in `this`.
    (p != 0).then(|| unsafe { &*(p as *const CpuGdt) })
}

/// One IST stack: [bottom, top) is mapped, the guard page(s) below `bottom` are not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IstStack {
    pub ist: u8,
    /// Start of the unmapped guard range, `None` for the static boot stacks.
    pub guard: Option<u64>,
    pub bottom: u64,
    pub top: u64,
}

impl IstStack {
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.bottom && addr < self.top
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IstError {
    /// The CPU area is used up.
    NoVirtualSpace,
    /// No frame for a stack page.
    NoMemory,
    Map(MapError),
    /// The guard page turned out mapped (would make the guard useless).
    GuardMapped,
    /// Host build.
    NotSupported,
}

/// Top-down allocation in [floor, next): the new base, 4 KiB aligned, or `None` when it does not fit (pure).
pub fn carve_down(next: u64, floor: u64, len: u64) -> Option<u64> {
    let len = len.checked_add(0xFFF)? & !0xFFF;
    let base = next.checked_sub(len)? & !0xFFF;
    (base >= floor && len != 0).then_some(base)
}

static CPU_AREA_NEXT: AtomicU64 = AtomicU64::new(bootinfo::layout::VMAP_END);

/// Reserves `len` bytes of virtual space in the CPU area (never freed: per-CPU stacks live as long as the CPU).
pub fn cpu_area_alloc(len: u64) -> Option<u64> {
    let mut cur = CPU_AREA_NEXT.load(Ordering::Acquire);
    loop {
        let base = carve_down(cur, CPU_AREA_BASE, len)?;
        match CPU_AREA_NEXT.compare_exchange(cur, base, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(base),
            Err(now) => cur = now,
        }
    }
}

/// Gives back the most recent `cpu_area_alloc` (an error path that has not handed the range out). Returns
/// false, leaving the range reserved, when another reservation followed it.
pub fn cpu_area_release(base: u64, len: u64) -> bool {
    let Some(len) = len.checked_add(0xFFF).map(|l| l & !0xFFF) else { return false };
    CPU_AREA_NEXT.compare_exchange(base, base + len, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// This CPU's IST stacks (#DF, NMI, #MC), as `init` / `install_guarded_ist` / `new_ap_tables` set them.
pub fn ist_stacks() -> [IstStack; 3] {
    this().ist_stacks()
}

/// The IST stacks of `cpu` (`None` if that CPU was never prepared).
pub fn ist_stacks_of(cpu: usize) -> Option<[IstStack; 3]> {
    of(cpu).map(|t| t.ist_stacks())
}

pub fn ist_stack(ist: u8) -> Option<IstStack> {
    ist_stacks().into_iter().find(|s| s.ist == ist)
}

/// The stack top this CPU's TSS holds for IST slot `ist` (1..=7; 0 otherwise).
pub fn tss_ist(ist: u8) -> u64 {
    this().tss_ist(ist)
}

/// Stack the CPU switches to when an interrupt or exception arrives from user mode (this CPU's TSS).
pub fn set_rsp0(top: u64) {
    // SAFETY: rsp0 sits at byte offset 4 of this CPU's TSS (unaligned by the architecture's layout); one store
    // by the owning CPU, read by the CPU only on a ring change.
    unsafe { this().tss_base().add(4).cast::<u64>().write_unaligned(top) };
}

#[cfg(target_os = "none")]
mod imp {
    use super::*;
    use crate::paging::{self, PageFlags};

    const BOOT_IST_STACK: usize = (IST_STACK_PAGES * 4096) as usize;

    #[repr(C, align(16))]
    struct Stack([u8; BOOT_IST_STACK]);

    struct BootStacks(UnsafeCell<[Stack; 3]>);
    // SAFETY: only the CPU runs on these (the BSP's IST slots before `install_guarded_ist`); Rust code only takes
    // their addresses.
    unsafe impl Sync for BootStacks {}
    static BOOT_IST: BootStacks = BootStacks(UnsafeCell::new([Stack([0; BOOT_IST_STACK]), Stack([0; BOOT_IST_STACK]), Stack([0; BOOT_IST_STACK])]));

    /// Loads `t` on this CPU: lgdt, data segments, a far return into the new CS, ltr, a null LDTR.
    pub fn load(t: &'static CpuGdt) {
        #[repr(C, packed)]
        struct Ptr {
            limit: u16,
            base: u64,
        }
        let ptr = Ptr { limit: (GDT_ENTRIES * 8 - 1) as u16, base: t.gdt.as_ptr() as u64 };
        // DS and ES are null as Linux's 64-bit programs start (`start_thread`); AB-10a's context switch keeps a
        // program's own DS/ES/FS/GS selectors per thread from then on (`entry::segs`).
        // SAFETY: `t` was filled by `fill` (kernel code/data selectors at 0x08/0x10, an available TSS at 0x40) and
        // lives forever ('static); the far return reloads CS with the same kernel code selector, so execution
        // continues at the next instruction. Interrupts are off at both callers (BSP start-up, AP start-up).
        // `lldt` with selector 0 loads a null LDTR (AB-16 CR, INT-W21): every TI=1 selector a program puts into
        // a sigreturn or iretq frame then raises #GP instead of the CPU reading a descriptor from whatever base
        // and limit firmware or the previous kernel (Reload) left in LDTR, as Linux's clear_LDT does
        // (linux-6.6/arch/x86/kernel/cpu/common.c:2283, arch/x86/include/asm/mmu_context.h:80).
        unsafe {
            core::arch::asm!(
                "lgdt [{p}]",
                "mov ax, {ds}",
                "mov ss, ax",
                "xor eax, eax", "mov ds, ax", "mov es, ax", "mov fs, ax", "mov gs, ax",
                "push {cs}",
                "lea rax, [rip + 2f]",
                "push rax",
                "retfq",
                "2:",
                "mov ax, {tss}",
                "ltr ax",
                "xor eax, eax",
                "lldt ax",
                p = in(reg) &ptr,
                ds = const KERNEL_SS,
                cs = const KERNEL_CS as u64,
                tss = const TSS_SEL,
                out("rax") _,
            );
        }
    }

    /// This CPU's LDTR selector (`sldt`): 0 once `load` ran.
    pub fn ldtr() -> u16 {
        let sel: u16;
        // SAFETY: `sldt` only stores the LDTR selector into a register; it changes no state and cannot fault in
        // ring 0 (no UMIP check there).
        unsafe { core::arch::asm!("sldt {0:x}", out(reg) sel, options(nomem, nostack, preserves_flags)) };
        sel
    }

    /// `lldt sel` on this CPU (0 or LDT_SEL, after `load_ldt` wrote the descriptor).
    pub fn lldt(sel: u16) {
        // SAFETY: `sel` is the null selector or LDT_SEL, whose slot `load_ldt` just filled with a present LDT
        // descriptor of this CPU's loaded GDT, so `lldt` cannot fault; it changes only where this CPU looks up
        // TI=1 selectors.
        unsafe { core::arch::asm!("lldt {0:x}", in(reg) sel, options(nostack, preserves_flags)) };
    }

    pub fn init() {
        // SAFETY: BSP-only start-up, interrupts off; only the addresses of the boot stacks are taken.
        let bottoms: [u64; 3] = unsafe { core::array::from_fn(|k| (*BOOT_IST.0.get())[k].0.as_ptr() as u64) };
        let mut tops = [0u64; 7];
        for (k, &bottom) in bottoms.iter().enumerate() {
            let top = bottom + BOOT_IST_STACK as u64;
            tops[(IST_USED[k] - 1) as usize] = top;
            BSP.record(k, &IstStack { ist: IST_USED[k], guard: None, bottom, top });
        }
        BSP.fill(tops);
        load(&BSP);
    }

    fn set_tss_ist(t: &CpuGdt, ist: u8, top: u64) {
        let p = t.tss_base().wrapping_add(TSS_IST_OFFSET + 8 * (ist as usize - 1));
        // SAFETY: `p` is IST slot `ist` inside `t`'s TSS, 4-byte aligned and within one cache line (AlignedTss),
        // so this single 8-byte `mov` is atomic for the CPU's reads at exception delivery (asm, because Rust has
        // no unaligned volatile store).
        unsafe { core::arch::asm!("mov qword ptr [{p}], {v}", p = in(reg) p, v = in(reg) top, options(nostack, preserves_flags)) };
    }

    /// Maps the three guarded IST stacks in a fresh CPU-area range; on error everything mapped is undone.
    fn map_ist_set(
        alloc_stack_page: &mut dyn FnMut() -> Option<u64>,
        free_stack_page: &mut dyn FnMut(u64),
        alloc_table: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<[IstStack; 3], IstError> {
        let root = paging::current_root();
        let len = (IST_GUARD_PAGES + IST_STACK_PAGES) * 4096;
        let area = cpu_area_alloc(len * IST_USED.len() as u64).ok_or(IstError::NoVirtualSpace)?;
        // Stack pages mapped so far, unmapped and freed again if a later step fails.
        let mut mapped: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
        let mut out = [IstStack::default(); 3];
        let mut build = || -> Result<(), IstError> {
            for (k, &ist) in IST_USED.iter().enumerate() {
                let base = area + k as u64 * len;
                let bottom = base + IST_GUARD_PAGES * 4096;
                for p in 0..IST_STACK_PAGES {
                    let frame = alloc_stack_page().ok_or(IstError::NoMemory)?;
                    let virt = bottom + p * 4096;
                    if let Err(e) = paging::map_4k(root, virt, frame, PageFlags::kernel_data(), alloc_table) {
                        free_stack_page(frame);
                        return Err(IstError::Map(e));
                    }
                    mapped.push(virt);
                }
                if (0..IST_GUARD_PAGES).any(|g| paging::translate(root, base + g * 4096).is_some()) {
                    return Err(IstError::GuardMapped);
                }
                out[k] = IstStack { ist, guard: Some(base), bottom, top: base + len };
            }
            Ok(())
        };
        if let Err(e) = build() {
            // Nothing points at these pages yet: give them back. Page tables created on the way stay; they serve
            // the next mapping in the CPU area.
            for &virt in &mapped {
                if let Some(frame) = paging::unmap_4k(root, virt) {
                    free_stack_page(frame);
                }
            }
            cpu_area_release(area, len * IST_USED.len() as u64);
            return Err(e);
        }
        Ok(out)
    }

    pub fn install_guarded_ist(
        alloc_stack_page: &mut dyn FnMut() -> Option<u64>,
        free_stack_page: &mut dyn FnMut(u64),
        alloc_table: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<[IstStack; 3], IstError> {
        let out = map_ist_set(alloc_stack_page, free_stack_page, alloc_table)?;
        let t = this();
        let _irq = crate::cpu::IrqGuard::new();
        for (k, s) in out.iter().enumerate() {
            set_tss_ist(t, s.ist, s.top);
            t.record(k, s);
        }
        Ok(out)
    }

    pub fn new_ap_tables(
        alloc_stack_page: &mut dyn FnMut() -> Option<u64>,
        free_stack_page: &mut dyn FnMut(u64),
        alloc_table: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<&'static CpuGdt, IstError> {
        let st = map_ist_set(alloc_stack_page, free_stack_page, alloc_table)?;
        let t: &'static CpuGdt = alloc::boxed::Box::leak(alloc::boxed::Box::new(CpuGdt::new()));
        let mut tops = [0u64; 7];
        for (k, s) in st.iter().enumerate() {
            tops[(s.ist - 1) as usize] = s.top;
            t.record(k, s);
        }
        t.fill(tops);
        Ok(t)
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::*;
    pub fn init() {
        BSP.fill([0; 7]);
    }
    #[allow(dead_code)]
    pub fn load(_t: &'static CpuGdt) {}
    pub fn ldtr() -> u16 {
        0
    }
    pub fn lldt(_sel: u16) {}
    pub fn install_guarded_ist(
        _alloc_stack_page: &mut dyn FnMut() -> Option<u64>,
        _free_stack_page: &mut dyn FnMut(u64),
        _alloc_table: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<[IstStack; 3], IstError> {
        Err(IstError::NotSupported)
    }
    pub fn new_ap_tables(
        _alloc_stack_page: &mut dyn FnMut() -> Option<u64>,
        _free_stack_page: &mut dyn FnMut(u64),
        _alloc_table: &mut dyn FnMut() -> Option<u64>,
    ) -> Result<&'static CpuGdt, IstError> {
        Err(IstError::NotSupported)
    }
}

static INIT_DONE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Loads the GDT and TSS on the BSP, IST slots pointing at the static boot stacks. Once: every later call (on
/// the BSP or an AP) does nothing, so the live BSP TSS is never refilled and no AP can load it.
pub fn init() {
    if INIT_DONE.swap(true, core::sync::atomic::Ordering::AcqRel) {
        return;
    }
    imp::init()
}
/// Moves this CPU's (the BSP's) IST stacks to guarded stacks in the CPU area. Frames: `alloc_stack_page` for
/// the stack pages (mm-core kind Stack, given back through `free_stack_page`), `alloc_table` for page tables. On
/// error the boot stacks stay in use (the caller logs the fallback); the stack pages mapped so far are unmapped
/// and freed and the CPU-area range is released (page tables created on the way stay for later use).
pub use imp::install_guarded_ist;
/// A GDT and TSS for an AP with three guarded IST stacks (same frame sources as `install_guarded_ist`); never
/// freed (an AP lives as long as the kernel). Loaded on the AP by `smp`.
pub use imp::new_ap_tables;
#[allow(unused_imports)]
pub(crate) use imp::load;
/// This CPU's LDTR selector; 0 (null) on every CPU once its tables are loaded (host builds: 0).
pub use imp::ldtr;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carving() {
        let end = 0x1_0000_0000u64;
        assert_eq!(carve_down(end, 0, 0x5000), Some(end - 0x5000));
        assert_eq!(carve_down(end, 0, 0x4001), Some(end - 0x5000), "rounded up to pages");
        assert_eq!(carve_down(end, end - 0x4000, 0x5000), None, "below the floor");
        assert_eq!(carve_down(0x3000, 0, 0x5000), None, "underflow");
        assert_eq!(carve_down(end, 0, 0), None);
        assert_eq!(carve_down(end, 0, u64::MAX), None);
    }

    #[test]
    fn cpu_area_is_in_vmap() {
        assert!(CPU_AREA_BASE > bootinfo::layout::VMAP_BASE);
        assert_eq!(CPU_AREA_BASE % (1 << 30), 0);
        assert_eq!(core::mem::size_of::<Tss>(), 104);
        assert_eq!(core::mem::offset_of!(Tss, ist), TSS_IST_OFFSET);
        assert_eq!(core::mem::align_of::<AlignedTss>(), 64);
    }

    #[test]
    fn release_only_the_last_reservation() {
        let a = cpu_area_alloc(0x5000).unwrap();
        let b = cpu_area_alloc(0x3000).unwrap();
        assert!(!cpu_area_release(a, 0x5000), "b was taken after a");
        assert!(cpu_area_release(b, 0x3000));
        assert_eq!(cpu_area_alloc(0x3000), Some(b), "released space is handed out again");
    }

    #[test]
    fn stack_contains() {
        let s = IstStack { ist: 1, guard: Some(0x1000), bottom: 0x2000, top: 0x6000 };
        assert!(s.contains(0x2000) && s.contains(0x5FF8) && !s.contains(0x6000) && !s.contains(0x1FFF));
    }

    #[test]
    fn tables_fill_and_report() {
        let t = CpuGdt::new();
        let mut tops = [0u64; 7];
        tops[0] = 0x1000;
        tops[2] = 0x3000;
        t.fill(tops);
        assert_eq!(t.tss_ist(1), 0x1000);
        assert_eq!(t.tss_ist(3), 0x3000);
        assert_eq!(t.tss_ist(0), 0);
        assert_eq!(t.tss_ist(8), 0);
        let gdt: [u64; GDT_ENTRIES] = core::array::from_fn(|i| t.entry(i));
        assert_eq!(gdt[1] >> 40 & 0xFF, 0x9A);
        assert_eq!(gdt[8] >> 40 & 0xFF, 0x89, "available 64-bit TSS");
        let base = (gdt[8] >> 16 & 0xFF_FFFF) | (gdt[8] >> 56 & 0xFF) << 24 | gdt[9] << 32;
        assert_eq!(base, t.tss_addr());
        assert_eq!(TSS_SEL as usize, TSS_SLOT * 8);
        assert_eq!(LDT_SEL as usize, LDT_SLOT * 8);
        t.record(1, &IstStack { ist: 2, guard: Some(0x10), bottom: 0x20, top: 0x30 });
        assert_eq!(t.ist_stacks()[1], IstStack { ist: 2, guard: Some(0x10), bottom: 0x20, top: 0x30 });
    }

    /// AB-10a: Linux's user selectors (segment.h:192-217) and their descriptors.
    #[test]
    fn linux_user_layout() {
        assert_eq!((USER32_CS, USER_DS, USER_CS), (0x23, 0x2b, 0x33));
        assert_eq!(TLS_FIRST * 8 + 3, 0x63);
        // SYSRET: CS = base + 16, SS = base + 8 from STAR[63:48] = USER32_CS.
        assert_eq!((USER32_CS + 16, USER32_CS + 8), (USER_CS, USER_DS));
        let t = CpuGdt::new();
        t.fill([0; 7]);
        let (c32, d, c64) = (t.entry(4), t.entry(5), t.entry(6));
        assert_eq!((c32 >> 40 & 0xFF, c32 >> 53 & 1, c32 >> 54 & 1), (0xFA, 0, 1), "32-bit code: L=0, D=1");
        assert_eq!(d >> 40 & 0xFF, 0xF2);
        assert_eq!((c64 >> 40 & 0xFF, c64 >> 53 & 1, c64 >> 54 & 1), (0xFA, 1, 0), "64-bit code: L=1, D=0");
        for i in [0, 3, 7, 10, 11, 12, 13, 14, 15] {
            assert_eq!(t.entry(i), 0, "slot {i} empty");
        }
        assert_eq!(t.entry(GDT_ENTRIES), 0);
        t.set_tls_entry(1, 0x1234);
        t.set_tls_entry(3, 0x9999);
        assert_eq!((t.entry(13), t.entry(15)), (0x1234, 0));
        t.load_ldt(0xFFFF_8000_1234_5000);
        let (lo, hi) = (t.entry(10), t.entry(11));
        assert_eq!(lo >> 40 & 0xFF, 0x82, "present LDT");
        assert_eq!((lo & 0xFFFF) | (lo >> 48 & 0xF) << 16, LDT_LIMIT);
        assert_eq!((lo >> 16 & 0xFF_FFFF) | (lo >> 56 & 0xFF) << 24 | hi << 32, 0xFFFF_8000_1234_5000);
        assert_eq!(t.ldt_base(), 0xFFFF_8000_1234_5000);
        t.load_ldt(0);
        assert_eq!((t.entry(10), t.entry(11), t.ldt_base()), (0, 0, 0));
    }
}
