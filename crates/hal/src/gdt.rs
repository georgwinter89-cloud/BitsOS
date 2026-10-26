//! GDT, TSS and the IST stacks (owner: CP). Selector layout is a contract (SYSCALL/SYSRET depend on it):
//! 0x08 kernel code, 0x10 kernel data, 0x18 user data (0x1B), 0x20 user code (0x23), 0x28 TSS (16 bytes).
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
pub const USER_SS: u16 = 0x18 | 3;
pub const USER_CS: u16 = 0x20 | 3;
pub const TSS_SEL: u16 = 0x28;

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

/// One CPU's descriptor tables: GDT (7 slots, the TSS descriptor takes two), TSS and what its IST slots hold.
pub struct CpuGdt {
    gdt: UnsafeCell<[u64; 7]>,
    tss: UnsafeCell<AlignedTss>,
    /// Per used IST slot: (guard or 0, bottom, top).
    ist: [[AtomicU64; 3]; 3],
}

// SAFETY: `gdt` and the TSS are written only before the tables are loaded (`init`, `new_ap_tables`), then by the
// owning CPU (`set_rsp0`, one store, interrupts off at its callers) and by `install_guarded_ist` (one aligned
// store per IST slot within one cache line, interrupts off). Other CPUs only read the atomics in `ist`.
unsafe impl Sync for CpuGdt {}

impl CpuGdt {
    const fn new() -> CpuGdt {
        CpuGdt {
            gdt: UnsafeCell::new([0; 7]),
            tss: UnsafeCell::new(AlignedTss(Tss::new([0; 7]))),
            ist: [const { [const { AtomicU64::new(0) }; 3] }; 3],
        }
    }

    fn tss_base(&self) -> *mut u8 {
        self.tss.get().cast::<u8>()
    }

    /// Writes the GDT entries and a TSS with `ist` tops. Only before the tables are loaded on any CPU.
    fn fill(&self, ist: [u64; 7]) {
        // SAFETY: the caller guarantees the tables are not loaded yet (no CPU reads them), and this is the only
        // writer at this point (BSP start-up or a fresh AP table set).
        unsafe {
            self.tss.get().write(AlignedTss(Tss::new(ist)));
            let tss_addr = self.tss_base() as u64;
            let limit = (core::mem::size_of::<Tss>() - 1) as u64;
            let gdt = &mut *self.gdt.get();
            gdt[0] = 0;
            gdt[1] = seg(0x9A, 0xA); // kernel code: present, ring 0, exec/read; L=1
            gdt[2] = seg(0x92, 0xC); // kernel data
            gdt[3] = seg(0xF2, 0xC); // user data (ring 3)
            gdt[4] = seg(0xFA, 0xA); // user code (ring 3, L=1)
            // TSS descriptor; base bits 24-31 matter (a truncated TSS base once reset V1 in a loop).
            gdt[5] = (limit & 0xFFFF)
                | ((tss_addr & 0xFF_FFFF) << 16)
                | (0x89 << 40)
                | (((limit >> 16) & 0xF) << 48)
                | (((tss_addr >> 24) & 0xFF) << 56);
            gdt[6] = tss_addr >> 32;
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

    /// Address of the TSS (tests compare it with what `str`/the descriptor say).
    pub fn tss_addr(&self) -> u64 {
        self.tss_base() as u64
    }
}

const fn seg(access: u64, flags: u64) -> u64 {
    (access << 40) | (flags << 52) | (0xF << 48) | 0xFFFF
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

    /// Loads `t` on this CPU: lgdt, data segments, a far return into the new CS, ltr.
    pub fn load(t: &'static CpuGdt) {
        #[repr(C, packed)]
        struct Ptr {
            limit: u16,
            base: u64,
        }
        let ptr = Ptr { limit: (7 * 8 - 1) as u16, base: t.gdt.get() as u64 };
        // SAFETY: `t` was filled by `fill` (kernel code/data selectors at 0x08/0x10, an available TSS at 0x28) and
        // lives forever ('static); the far return reloads CS with the same kernel code selector, so execution
        // continues at the next instruction. Interrupts are off at both callers (BSP start-up, AP start-up).
        unsafe {
            core::arch::asm!(
                "lgdt [{p}]",
                "mov ax, {ds}",
                "mov ds, ax", "mov es, ax", "mov ss, ax",
                "xor eax, eax", "mov fs, ax", "mov gs, ax",
                "push {cs}",
                "lea rax, [rip + 2f]",
                "push rax",
                "retfq",
                "2:",
                "mov ax, {tss}",
                "ltr ax",
                p = in(reg) &ptr,
                ds = const KERNEL_SS,
                cs = const KERNEL_CS as u64,
                tss = const TSS_SEL,
                out("rax") _,
            );
        }
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
        // SAFETY: test-local tables, not loaded anywhere.
        let gdt = unsafe { &*t.gdt.get() };
        assert_eq!(gdt[1] >> 40 & 0xFF, 0x9A);
        assert_eq!(gdt[5] >> 40 & 0xFF, 0x89, "available 64-bit TSS");
        let base = (gdt[5] >> 16 & 0xFF_FFFF) | (gdt[5] >> 56 & 0xFF) << 24 | gdt[6] << 32;
        assert_eq!(base, t.tss_addr());
        t.record(1, &IstStack { ist: 2, guard: Some(0x10), bottom: 0x20, top: 0x30 });
        assert_eq!(t.ist_stacks()[1], IstStack { ist: 2, guard: Some(0x10), bottom: 0x20, top: 0x30 });
    }
}
