//! Reload's trusted part (owner: RB, items RB-5a/RB-5b): byte access to the frames Reload stages the new kernel
//! in, the relocation stub bitsboot's `KEEP` page holds, the CPU's interrupt sources made quiet, the other CPUs
//! stopped (`park`: park IPI or NMI, LAPIC quiet, INIT), and the jump. `shutdown` (RB-quiesce) parks the other CPUs
//! with the same stop before a reboot's or power-off's quiesce.
//!
//! Rule for callers (the convention of `dma_buf` and `mm_core::zero_frame`): `write_phys`/`zero_phys` take frames
//! the frame allocator handed to Reload for the new kernel (image, initramfs, stack, page tables) and nothing
//! else; `read_phys` reads RAM the BootInfo memory map names (`BITS_MODULE`, `BITS_KEEP`) or frames Reload itself
//! staged (written only by Reload, on the same thread, before it reads them back). The kexec crate is the only
//! caller. The initramfs module counts as `BITS_MODULE` only until the kernel releases it to the frame allocator
//! after unpacking (INT-A1, `kexec::boot::note_initrd_released`): from then on its range is ordinary allocator
//! memory and must not be read through `read_phys` (kexec's module readers check `kexec::boot::module_readable`).
//! hal cannot enforce that by range, because frames Reload stages later may lie inside the released range.
//!
//! The stub (`bits_kexec_stub_*`) is position independent and runs from the `KEEP` stub page, which the running
//! kernel's tables (bitsboot maps it identity, executable) and the new tables (kexec builds them like bitsboot)
//! both map at its physical address. In this order it
//! - switches to the new stack (direct map, which both roots map) and clears RFLAGS (IF, DF, TF, AC, NT, IOPL);
//! - loads a GDT kept inside the stub page (code 0x08, data 0x10) and an empty IDT *before* it leaves the old
//!   tables: from then on no descriptor of the old kernel is reachable, and an NMI or machine check resets the
//!   machine instead of being dispatched through the old IDT's address, which in the new tables is the new
//!   kernel's image (V1's rule "from the quiesce on, a fault resets the machine"; Linux runs idt_invalidate()
//!   and set_gdt() before relocate_kernel for the same reason);
//! - loads every data segment register with 0x10 and zeroes IA32_FS_BASE, IA32_GS_BASE and
//!   IA32_KERNEL_GS_BASE, so the old kernel's per-CPU pointer does not survive;
//! - writes CR4 to a known value: PAE | OSFXSR | OSXMMEXCPT, keeping only MCE and the bits that cannot change
//!   here (LA57, VMXE, SMXE). PGE and PCIDE go off, which drops every TLB entry of the old kernel, global ones
//!   and those of every PCID (Intel SDM vol. 3 §4.10.4.1); SMEP, SMAP, UMIP, FSGSBASE, OSXSAVE, PKE and CET go
//!   off too, so the new kernel's CP-1 set-up starts from a defined state (it writes CR4, XCR0 and EFER whole);
//! - loads the new CR3 and jumps to the entry with bitsboot's register contract (rdi = BootInfo virt,
//!   rsi = phys, rsp = stack top, IF = 0, DF = 0).
//!
//! Not reset by the stub: XCR0 (unreachable with CR4.OSXSAVE off; CP-1 writes it whole), STAR/LSTAR/FMASK and
//! EFER (written whole by the new kernel's `init_syscall` and CP-1 before any program runs).
//!
//! Reference: Linux `arch/x86/kernel/relocate_kernel_64.S` (CR4 to a known value, identity page, own GDT in
//! purgatory) and `machine_kexec_64.c` (IDT invalidated before the jump).

use core::convert::Infallible;

mod park;
mod shutdown;
pub use shutdown::{ParkRefusal, ShutdownPark, live_stop_options, lock_safe, park_for_shutdown, stop_options};
pub use park::{InitReport, PARK_VECTOR, init_allowed, Quiet, StopError, StopOptions, StopReport, Stopped, others_stopped, park_guard, set_park_guard, stop_others, stray_and_deferred};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StubError {
    /// The stub page is not mapped in the running kernel's tables.
    NotMapped,
    /// Mapped, but to another physical page (not identity).
    NotIdentity { phys: u64 },
    /// Mapped no-execute (at the leaf or at any level above it).
    NotExecutable,
    /// The stub code does not fit the page (a build problem).
    TooLarge { len: usize },
    /// The new BootInfo is larger than the `KEEP` block bitsboot reserved.
    BootInfoTooLarge { len: usize, capacity: u64 },
    /// An address is not page aligned where it must be.
    NotAligned,
    /// The kernel root given for the jump does not map the running code or stack as the live tables do.
    RootDiffers,
    /// Other CPUs still run kernel code at the jump: `park` did not send INIT to every one of them.
    OtherCpusRunning { count: usize },
    /// The Reload runs on another CPU than the boot CPU: INIT would send the boot CPU to the firmware's reset
    /// vector, so only the boot CPU stops the others and jumps.
    NotBootCpu { cpu: usize },
    /// Other CPUs are online but this CPU's LAPIC is not set up (no IPI can stop them).
    NoLapic,
    /// An online CPU has no APIC ID (no IPI can reach it).
    NoApicId { cpu: usize },
    /// The APs were handed to a scheduler (`smp::hand_over_aps`) but nobody set a park guard: a park IPI could
    /// stop an AP holding a plain `SpinLock` the boot CPU then waits for in the quiesce (change request to SC:
    /// `set_park_guard` at scheduler start).
    NoParkGuard,
    /// Host build: there is nothing to jump to.
    NotBareMetal,
}

/// Everything the jump needs; all addresses are physical except `stack_top` (direct map, as bitsboot hands over).
#[derive(Debug, Clone, Copy)]
pub struct Handoff<'a> {
    /// The bridge root the jump switches to before the stub (`kexec::stage::bridge_root`): a separate PML4 whose
    /// kernel half is copied from the kernel root (MM-2 pre-populates all 256 upper-half PDPTs, so mappings made
    /// later, such as new vmap stacks, are visible through it) plus the `KEEP` stub page identity-mapped and
    /// executable. Its frames belong to the `Staged` that calls `jump` and stay valid while it lives; neither
    /// MM-2's root (no lower half) nor bitsboot's (no vmap thread stacks) nor a process root would do.
    pub kernel_root: u64,
    pub stub_phys: u64,
    pub new_root: u64,
    pub entry: u64,
    pub stack_top: u64,
    pub bootinfo_phys: u64,
    /// The `KEEP` block's size. kexec checks that `[bootinfo_phys, +capacity)` and the stub page lie wholly
    /// inside `BITS_KEEP` descriptors of the memory map before it builds a `Handoff` (`stage::stage_with`).
    pub bootinfo_capacity: u64,
    pub bootinfo: &'a [u8],
}

#[cfg(target_os = "none")]
core::arch::global_asm!(
    ".pushsection .text.bits_kexec_stub, \"ax\"",
    ".global bits_kexec_stub_start",
    ".global bits_kexec_stub_end",
    ".p2align 4",
    "bits_kexec_stub_start:",
    "mov rsp, rcx",
    "mov r9, rdx",
    "push 2",
    "popfq",
    "lea rax, [rip + 3f]",
    "sub rsp, 16",
    "mov word ptr [rsp + 6], 23",
    "mov qword ptr [rsp + 8], rax",
    "lgdt [rsp + 6]",
    "mov word ptr [rsp + 6], 0",
    "mov qword ptr [rsp + 8], 0",
    "lidt [rsp + 6]",
    "add rsp, 16",
    "push 0x08",
    "lea rax, [rip + 2f]",
    "push rax",
    "retfq",
    "2:",
    "mov eax, 0x10",
    "mov ds, ax",
    "mov es, ax",
    "mov ss, ax",
    "mov fs, ax",
    "mov gs, ax",
    "xor eax, eax",
    "xor edx, edx",
    "mov ecx, 0xC0000100",
    "wrmsr",
    "mov ecx, 0xC0000101",
    "wrmsr",
    "mov ecx, 0xC0000102",
    "wrmsr",
    // Keep MCE (bit 6), LA57 (12), VMXE (13), SMXE (14); set PAE (5), OSFXSR (9), OSXMMEXCPT (10).
    "mov rax, cr4",
    "and eax, 0x7040",
    "or eax, 0x620",
    "mov cr4, rax",
    "mov cr3, r9",
    "xor ebp, ebp",
    "jmp r8",
    ".p2align 3",
    "3:",
    ".quad 0",
    // Accessed bits preset (type 0x9B/0x93): the page is mapped read-only, and loading a descriptor whose
    // accessed bit is clear makes the CPU write it (a #PF here would be a triple fault; Linux presets them too).
    ".quad 0x00AF9B000000FFFF",
    ".quad 0x00CF93000000FFFF",
    "bits_kexec_stub_end:",
    ".popsection",
);

#[cfg(target_os = "none")]
unsafe extern "C" {
    static bits_kexec_stub_start: u8;
    static bits_kexec_stub_end: u8;
}

/// The stub's machine code (with its GDT), as copied into the `KEEP` page.
pub fn stub_code() -> &'static [u8] {
    #[cfg(target_os = "none")]
    {
        // Two labels are two Rust statics, so the length comes from their addresses and the slice from an
        // exposed-provenance pointer (no `offset_from` between different statics).
        let start = (&raw const bits_kexec_stub_start).expose_provenance();
        let len = (&raw const bits_kexec_stub_end).addr().saturating_sub(start);
        // SAFETY: both symbols are labels of the global_asm block above, in this order, in one read-only
        // section, so the linker places the `len` bytes between them contiguously: the stub's code and its GDT,
        // never written. The pointer takes the provenance exposed above (the label symbol's), the pattern for
        // linker-defined ranges.
        unsafe { core::slice::from_raw_parts(core::ptr::with_exposed_provenance::<u8>(start), len) }
    }
    #[cfg(not(target_os = "none"))]
    &[]
}

/// Walks `root` for `virt` as the MMU does: `(phys, executable)`, where NX on any level above the leaf (or on
/// the leaf) makes it no-execute (`paging::translate` reports the leaf's NX only).
#[cfg(target_os = "none")]
fn walk_exec(root: u64, virt: u64) -> Option<(u64, bool)> {
    use crate::paging::PageFlags;
    const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
    let mut t = root & ADDR;
    let mut nx = false;
    for level in (1..=4u32).rev() {
        let shift = 12 + 9 * (level - 1);
        let idx = ((virt >> shift) & 0x1FF) as usize;
        // SAFETY: `t` is a table frame of a live root (the root itself or an address read from a present
        // entry), reached through the direct map like `paging::translate`; index < 512 within its 4 KiB.
        let e = unsafe { (crate::phys_to_virt(t) as *const u64).add(idx).read_volatile() };
        if e & PageFlags::PRESENT == 0 {
            return None;
        }
        nx |= e & PageFlags::NX != 0;
        if level == 1 || (e & PageFlags::HUGE != 0 && level <= 3) {
            let page = 1u64 << shift;
            return Some(((e & ADDR & !(page - 1)) + (virt & (page - 1)), !nx));
        }
        t = e & ADDR;
    }
    None
}

/// Whether a stop of the other CPUs needs a park guard first (pure; host-tested): once the APs run a scheduler
/// they can hold plain `SpinLock`s with interrupts on, so the park IPI must ask the guard.
fn park_guard_missing(handed_over: bool, guard_set: bool) -> bool {
    handed_over && !guard_set
}

/// Whether this CPU can stop the others before a jump (RB-5b): with one CPU always; with more, only the boot CPU
/// (CPU 0 with IA32_APIC_BASE.BSP) with its LAPIC up, every other CPU reachable by APIC ID, and a park guard set
/// once the APs belong to a scheduler. Allocation- and lock-free (a ktest runs it on an AP through
/// `smp::call_on`).
pub fn check_cpus() -> Result<(), StubError> {
    let count = crate::percpu::cpu_count();
    if count <= 1 {
        return Ok(());
    }
    if park_guard_missing(crate::smp::handed_over(), park_guard().is_some()) {
        return Err(StubError::NoParkGuard);
    }
    let cpu = crate::percpu::this_cpu();
    if cpu != 0 || crate::msr::read(crate::msr::IA32_APIC_BASE) & crate::apic::APIC_BASE_BSP == 0 {
        return Err(StubError::NotBootCpu { cpu });
    }
    if crate::apic::mode() == crate::apic::Mode::Off {
        return Err(StubError::NoLapic);
    }
    match (1..count).find(|&c| crate::smp::apic_id(c).is_none()) {
        Some(cpu) => Err(StubError::NoApicId { cpu }),
        None => Ok(()),
    }
}

/// `check_stub_tables`, and that this CPU can stop the others (`check_cpus`): what a Reload needs before it
/// starts stopping anything.
pub fn check_stub_page(root: u64, stub_phys: u64) -> Result<(), StubError> {
    check_cpus()?;
    check_stub_tables(root, stub_phys)
}

/// Checks that `root` (the bridge root kexec built for the jump, see `Handoff::kernel_root`) maps the stub page at its own physical address, executable on
/// every level, and maps this code and the current stack exactly as the live tables do (so switching to it is
/// harmless). Staging needs only this; the number of CPUs matters for the jump (`check_stub_page`).
pub fn check_stub_tables(root: u64, stub_phys: u64) -> Result<(), StubError> {
    if stub_phys & 0xFFF != 0 || stub_phys == 0 {
        return Err(StubError::NotAligned);
    }
    if stub_code().len() > 4096 {
        return Err(StubError::TooLarge { len: stub_code().len() });
    }
    #[cfg(target_os = "none")]
    {
        use crate::paging::{current_root, translate};
        if root & 0xFFF != 0 || root == 0 {
            return Err(StubError::NotAligned);
        }
        let here = check_stub_tables as *const () as usize as u64;
        let stack = &raw const here as u64;
        for v in [here, stack] {
            if translate(root, v).map(|t| t.0) != translate(current_root(), v).map(|t| t.0) {
                return Err(StubError::RootDiffers);
            }
        }
        let (phys, exec) = walk_exec(root, stub_phys).ok_or(StubError::NotMapped)?;
        if phys != stub_phys {
            return Err(StubError::NotIdentity { phys });
        }
        if !exec {
            return Err(StubError::NotExecutable);
        }
        Ok(())
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = root;
        Err(StubError::NotBareMetal)
    }
}

/// Whether `root` maps `virt` executable on every level (tests read the new tables with it).
pub fn executable(root: u64, virt: u64) -> Option<(u64, bool)> {
    #[cfg(target_os = "none")]
    {
        walk_exec(root, virt)
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (root, virt);
        None
    }
}

/// Every check `jump` makes except that the other CPUs were stopped, without changing anything: kexec runs it
/// before it stops the other CPUs and quiesces the devices, so a refusal leaves the running kernel working.
pub fn check(h: &Handoff<'_>) -> Result<(), StubError> {
    check_stub_page(h.kernel_root, h.stub_phys)?;
    for a in [h.new_root, h.bootinfo_phys] {
        if a & 0xFFF != 0 {
            return Err(StubError::NotAligned);
        }
    }
    if h.bootinfo.len() as u64 > h.bootinfo_capacity {
        return Err(StubError::BootInfoTooLarge { len: h.bootinfo.len(), capacity: h.bootinfo_capacity });
    }
    Ok(())
}

/// What `quiet_local_apic` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApicQuiet {
    /// The LVT timer is masked with initial count 0 now (written here in x2APIC mode, found so in xAPIC mode).
    pub timer_masked: bool,
    /// The timer was running (unmasked) and this call stopped it.
    pub timer_stopped: bool,
    /// LINT0 and the error LVT are masked.
    pub lint0_masked: bool,
    /// Thermal, performance-counter, CMCI and AMD extended LVTs masked (those this LAPIC has).
    pub other_lvts_masked: u32,
}

/// Reload's quiesce of this CPU's LAPIC (spec/core.md §2.4: "LAPIC quiet"; every CPU runs it, the APs in their
/// park loop): the timer masked with initial count 0, LINT0, the error, thermal, performance-counter and CMCI
/// LVTs and the AMD extended LVTs masked; LINT1 (NMI) stays. The timer and LINT0 are
/// written by CP's `apic::quiet_local` in either mode (the timer stopped, the TSC deadline cleared). Call with
/// interrupts off; no lock, no allocation.
pub fn quiet_local_apic() -> ApicQuiet {
    use crate::apic::{self, LVT_MASKED, REG_EILVT0, REG_LVT_CMCI, REG_LVT_ERROR, REG_LVT_LINT0, REG_LVT_PERF, REG_LVT_THERMAL, REG_LVT_TIMER};
    let masked = |reg| apic::read(reg).is_none_or(|v| v & LVT_MASKED != 0);
    let was_running = !masked(REG_LVT_TIMER);
    apic::mask_lvt(REG_LVT_ERROR);
    let mut others = 0;
    for reg in [REG_LVT_THERMAL, REG_LVT_PERF, REG_LVT_CMCI] {
        others += apic::mask_lvt(reg) as u32;
    }
    for n in 0..apic::eilvt_count() {
        others += apic::mask_lvt(REG_EILVT0 + n * 0x10) as u32;
    }
    // CP-5 change request 4a (INT-A1): CP's `quiet_local` stops and masks the timer (TSC deadline cleared too) and
    // masks LINT0 in both x2APIC and xAPIC mode; since CP-5 the timer is live, so xAPIC mode needs it as well.
    apic::quiet_local();
    let timer_masked = masked(REG_LVT_TIMER);
    ApicQuiet { timer_masked, timer_stopped: was_running && timer_masked, lint0_masked: masked(REG_LVT_LINT0), other_lvts_masked: others }
}

/// The boot CPU's LINT0 back to virtual wire (ExtINT, unmasked) before the jump, as V1's `apic_quiesce` and
/// Linux's `disable_local_APIC` leave it: the new kernel then finds the LAPIC as the firmware hands it over at a
/// cold boot. Harmless: the 8259s are masked (`quiet_legacy_pic` first) and interrupts stay off until the new
/// kernel's LAPIC set-up masks LINT0 again. LINT1 (NMI) is left as it is. Only x2APIC mode can write it here (the
/// xAPIC window is CP's, change request `hal::apic::quiet_local`): returns whether LINT0 is ExtINT now. Boot CPU,
/// interrupts off; no lock, no allocation.
pub fn virtual_wire() -> bool {
    use crate::apic::{self, DM_EXTINT, DM_MASK, LVT_MASKED, Mode, REG_LVT_LINT0};
    if crate::percpu::this_cpu() != 0 || crate::cpu::interrupts_enabled() {
        return false;
    }
    if apic::mode() == Mode::X2Apic {
        crate::msr::write(apic::x2apic_msr(REG_LVT_LINT0), DM_EXTINT as u64);
    }
    apic::read(REG_LVT_LINT0).is_some_and(|v| v & (LVT_MASKED | DM_MASK) == DM_EXTINT)
}

/// Copies `data` to physical `phys` through the direct map (frames Reload owns, see the module rule).
pub fn write_phys(phys: u64, data: &[u8]) -> bool {
    if phys.checked_add(data.len() as u64).is_none() {
        return false;
    }
    #[cfg(target_os = "none")]
    // SAFETY: by the module rule `phys..phys+len` are frames the frame allocator gave Reload for the new kernel:
    // RAM, direct-mapped, no Rust object of this kernel lives there.
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), crate::phys_to_virt(phys) as *mut u8, data.len())
    };
    true
}

/// Zeroes `len` bytes at physical `phys` (same rule as `write_phys`).
pub fn zero_phys(phys: u64, len: u64) -> bool {
    if phys.checked_add(len).is_none() || len > usize::MAX as u64 {
        return false;
    }
    #[cfg(target_os = "none")]
    // SAFETY: as write_phys.
    unsafe {
        core::ptr::write_bytes(crate::phys_to_virt(phys) as *mut u8, 0, len as usize)
    };
    true
}

/// Copies RAM at physical `phys` into `out` (BootInfo `MODULE`/`KEEP` ranges, or frames Reload staged; see the
/// module rule).
pub fn read_phys(phys: u64, out: &mut [u8]) -> bool {
    if phys.checked_add(out.len() as u64).is_none() {
        return false;
    }
    #[cfg(target_os = "none")]
    // SAFETY: by the module rule the range is RAM nobody else writes while Reload reads it: modules and KEEP
    // pages from the BootInfo memory map (not handed out by the frame allocator; the initramfs module only until
    // its release, after which the rule forbids reading it), or frames the allocator gave
    // Reload, which only Reload writes, on this thread, before reading them back; direct-mapped.
    unsafe {
        core::ptr::copy_nonoverlapping(crate::phys_to_virt(phys) as *const u8, out.as_mut_ptr(), out.len())
    };
    #[cfg(not(target_os = "none"))]
    out.fill(0);
    true
}

/// The point of no return: interrupts off, the stub into its page, the new BootInfo over the old one in its
/// `KEEP` block, then into the stub. Returns only if a check fails before anything was written. The caller runs
/// `check` with the same `Handoff` before it quiesces the devices, so in practice it does not return.
pub fn jump(h: &Handoff<'_>) -> Result<Infallible, StubError> {
    check(h)?;
    if !others_stopped() {
        return Err(StubError::OtherCpusRunning { count: crate::percpu::cpu_count() });
    }
    #[cfg(target_os = "none")]
    {
        crate::cpu::disable_interrupts();
        // The bridge root (`Handoff::kernel_root`): the kernel half as the live tables map it (code and stack
        // checked above), plus the identity-mapped stub page; its frames live as long as the caller's `Staged`.
        // Not `paging::activate`: that refuses a root with no context that is no kernel root (INT-A2a2).
        crate::paging::tlb::load_for_jump(h.kernel_root);
        let code = stub_code();
        // SAFETY: kexec built the Handoff only after checking that the stub page and `[bootinfo_phys,
        // +bootinfo_capacity)` lie wholly inside `BITS_KEEP` descriptors of the BootInfo memory map
        // (`stage::stage_with`): bitsboot's KEEP pages, RAM, direct-mapped, never handed out by the frame
        // allocator. `check` above bounds the copies by the stub page (4096) and that capacity. Every other CPU
        // was sent INIT (`others_stopped`, checked above) and interrupts are off, so from here on no Rust code of
        // this kernel runs again (the asm below does not return) and no reference into the old BootInfo is read
        // after the copy.
        unsafe {
            core::ptr::copy_nonoverlapping(code.as_ptr(), crate::phys_to_virt(h.stub_phys) as *mut u8, code.len());
            core::ptr::copy_nonoverlapping(h.bootinfo.as_ptr(), crate::phys_to_virt(h.bootinfo_phys) as *mut u8, h.bootinfo.len());
        }
        // Code written through the direct map runs through the identity map: serialize before fetching it
        // (Intel SDM vol. 3 §8.1.3, cross-linear-address modification).
        let _ = crate::cpu::cpuid(0, 0);
        // SAFETY: the stub page maps identity + executable in the live tables (checked above) and in the new
        // ones (built by kexec); the registers follow the stub's contract (rdx = new CR3, rcx = stack top,
        // r8 = entry, rdi/rsi = BootInfo virt/phys). It never returns.
        unsafe {
            core::arch::asm!(
                "jmp {stub}",
                stub = in(reg) h.stub_phys,
                in("rdi") crate::phys_to_virt(h.bootinfo_phys),
                in("rsi") h.bootinfo_phys,
                in("rdx") h.new_root,
                in("rcx") h.stack_top,
                in("r8") h.entry,
                options(noreturn),
            )
        }
    }
    #[cfg(not(target_os = "none"))]
    Err(StubError::NotBareMetal)
}

#[cfg(test)]
mod tests {
    #[test]
    fn park_guard_needed_once_aps_are_handed_over() {
        assert!(!super::park_guard_missing(false, false), "APs idle in smp's loop: no guard needed");
        assert!(super::park_guard_missing(true, false), "APs run a scheduler without a guard: refuse");
        assert!(!super::park_guard_missing(true, true));
    }
}
