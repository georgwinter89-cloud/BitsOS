//! Last-resort paths (owner: RB): halting forever, leaving QEMU through `isa-debug-exit`, the reset, the
//! frame-pointer backtrace and the crash page (RB-2, spec/core.md §10.3).
//!
//! Nothing here allocates or takes a lock: the panic path runs with the heap in emergency mode, and the NMI
//! handler (`nmi.rs`) walks the stack of whatever the CPU was doing.

use core::sync::atomic::{AtomicU64, Ordering};

/// Stops this CPU for good (interrupts off, hlt in a loop).
pub fn halt_forever() -> ! {
    crate::cpu::disable_interrupts();
    loop {
        crate::cpu::halt();
    }
}

/// QEMU's isa-debug-exit device at 0xF4: QEMU exits with `(code << 1) | 1`. On real hardware the port is
/// unused and the write does nothing, so callers continue with `halt_forever`.
pub fn qemu_exit(code: u32) {
    crate::portio::raw::outl(0xF4, code);
}

/// Exit codes of the ktest protocol (`plan/CONTRACTS.md`, xtask maps them back).
pub const EXIT_PASS: u32 = 0x10;
pub const EXIT_FAIL: u32 = 0x11;
pub const EXIT_PANIC: u32 = 0x12;

/// TSC rate assumed when the caller knows none: the fastest clock of the two machines, so the wait is never
/// shorter than asked (on slower clocks it is longer).
const ASSUMED_TSC_HZ: u64 = 4_000_000_000;

/// Waits about `secs` seconds (TSC, assuming at most 4 GHz), then resets the machine (`reset_after_ns`).
pub fn reset_after(secs: u64) -> ! {
    reset_after_ns(secs.saturating_mul(1_000_000_000), 0)
}

/// TSC ticks for `ns` at `tsc_hz` (0 = unknown: 4 GHz assumed).
pub fn ticks_for(ns: u64, tsc_hz: u64) -> u64 {
    let hz = if tsc_hz == 0 { ASSUMED_TSC_HZ } else { tsc_hz };
    ((ns as u128 * hz as u128) / 1_000_000_000).min(u64::MAX as u128) as u64
}

/// Busy-waits `ns` nanoseconds on the TSC at `tsc_hz` (0 = unknown: 4 GHz assumed), interrupts off. For the
/// panic path only (nothing else runs on this CPU any more).
pub fn spin_ns(ns: u64, tsc_hz: u64) {
    crate::cpu::disable_interrupts();
    let end = crate::cpu::rdtsc().saturating_add(ticks_for(ns, tsc_hz));
    while crate::cpu::rdtsc() < end {
        core::hint::spin_loop();
    }
}

/// Waits `ns` nanoseconds (`spin_ns`), then resets the machine (`reset_now`).
pub fn reset_after_ns(ns: u64, tsc_hz: u64) -> ! {
    spin_ns(ns, tsc_hz);
    reset_now()
}

/// The last-resort resets, for when the firmware's own reset register (FADT, which `kexec::crash` tries first,
/// like the MCP `reboot`) did nothing: 0xCF9 hard reset, the 8042 reset line, finally a triple fault. For real
/// hardware without COM1, where a silent halt after a fatal error looks exactly like a hang (A0 on the Mini-PC).
/// Whether RAM keeps its contents (the crash page) is the board's business (measured once on the Mini-PC, RB-2
/// notes); the crash page's lines were written back to RAM when written (`crash_page_write`), and the caches are
/// written back here once more before the reset.
pub fn reset_now() -> ! {
    crate::cpu::disable_interrupts();
    imp::write_back_caches();
    // CP-5's CF9 sequence (read-modify-write, Linux arch/x86/kernel/reboot.c:694-700), as the console reboot path.
    use crate::cpu::reset;
    let (arm, go) = reset::cf9_writes(reset::read_cf9(), reset::CF9_CODE_HARD);
    reset::write_cf9(arm);
    // Linux waits 50 us between the two writes; no TSC rate here, so a pause loop well above that.
    for _ in 0..10_000 {
        core::hint::spin_loop();
    }
    reset::write_cf9(go);
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }
    crate::portio::raw::outb(0x64, 0xFE);
    for _ in 0..1_000_000 {
        core::hint::spin_loop();
    }
    reset::triple_fault()
}

// ---------------------------------------------------------------------------------------------------------
// Frame-pointer backtrace (the kernel is built with `force-frame-pointers`, `.cargo/config.toml`).

/// One step of a backtrace, innermost first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Step {
    /// Nothing (unused slots of a buffer).
    #[default]
    None,
    /// A return address: the call site is the instruction before it.
    Call(u64),
    /// A return address read at a function's first instructions (`entry_return`): right if the interrupted rip
    /// really is a compiled function's entry, a stack word otherwise (hand-written asm may `push rbp` anywhere).
    /// The report checks it against the symbol table (`kexec::crash`) and marks it `~` without one.
    Guess(u64),
    /// The walk crossed a trap frame (`idt::TrapFrame`): the interrupted code was at `rip`. A trap from user
    /// mode ends the walk (`user`).
    Trap { vector: u64, error: u64, rip: u64, user: bool },
}

/// What the walker may touch: 8-byte stack reads (bounded by the walker to the stack it walks, see `Window`),
/// 8-byte code reads, whether a page is ordinary mapped memory, the test for "this return address is the trap
/// stub's call of `hal_kernel_trap`", and the test for "this is kernel code".
pub struct WalkEnv<'a> {
    pub read: &'a dyn Fn(u64) -> Option<u64>,
    pub code: &'a dyn Fn(u64) -> Option<u64>,
    pub page_ok: &'a dyn Fn(u64) -> bool,
    pub is_trap_return: &'a dyn Fn(u64) -> bool,
    pub is_text: &'a dyn Fn(u64) -> bool,
}

/// Largest distance between two frame pointers on one stack: a sanity bound against garbage (no frame is this
/// big), not a limit on stack size.
pub const MAX_FRAME_BYTES: u64 = 1 << 20;
/// Largest stretch of one stack the walker reads above the interrupted rsp (a sanity bound like the one above:
/// kernel stacks are a few pages).
pub const MAX_STACK_WINDOW: u64 = 4 << 20;

const PAGE: u64 = 4096;

/// Byte offsets inside `idt::TrapFrame` (checked against the struct by a host test).
const TF_RBP: u64 = 8 * 8;
const TF_VECTOR: u64 = 15 * 8;
const TF_ERROR: u64 = 16 * 8;
const TF_RIP: u64 = 17 * 8;
const TF_CS: u64 = 18 * 8;
const TF_RSP: u64 = 20 * 8;

/// The part of one stack the walker may read: from the interrupted (or current) rsp up through pages that are
/// all mapped as ordinary memory, without a hole. That stack is the one the CPU runs on, so nobody frees or
/// unmaps it meanwhile; every vmap window (stacks included) has an unmapped guard page below it, so the run
/// ends before the next one. A frame pointer outside it (garbage, or a program's rbp in the entry stubs, where
/// the CPU is in kernel mode with the user's rbp) is never followed into device windows or memory another CPU
/// may unmap. The first page may be missing (rsp in the guard page after a stack overflow, the #DF case): the
/// window then starts on the page above it.
struct Window {
    lo: u64,
    hi: u64,
}

impl Window {
    fn new(env: &WalkEnv<'_>, rsp: u64) -> Option<Window> {
        if rsp < KERNEL_HALF {
            return None;
        }
        let p = rsp & !(PAGE - 1);
        if (env.page_ok)(p) {
            return Some(Window { lo: rsp, hi: p.checked_add(PAGE)? });
        }
        let up = p.checked_add(PAGE)?;
        (env.page_ok)(up).then_some(Window { lo: up, hi: up.checked_add(PAGE)? })
    }

    fn read(&mut self, env: &WalkEnv<'_>, addr: u64) -> Option<u64> {
        let end = addr.checked_add(8)?;
        if addr < self.lo || end - self.lo > MAX_STACK_WINDOW {
            return None;
        }
        while end > self.hi {
            if !(env.page_ok)(self.hi) {
                return None;
            }
            self.hi = self.hi.checked_add(PAGE)?;
        }
        (env.read)(addr)
    }
}

/// Byte at `addr` through the walker's 8-byte code reads.
fn code_byte_with(env: &WalkEnv<'_>, addr: u64) -> Option<u8> {
    let w = (env.code)(addr & !7)?;
    Some(w.to_le_bytes()[(addr & 7) as usize])
}

/// The return address of a function interrupted where its own frame does not exist (yet or any more): at its
/// first instruction (`push rbp`: the return address is at `[rsp]`), right after it (`mov rbp, rsp` next: at
/// `[rsp+8]`), or at its `ret` (`rbp` popped already: at `[rsp]`). There `rbp` still is the caller's frame, so the
/// chain alone would skip the interrupted function's caller (an NMI caught `time::now_ns+0x0` that way). The
/// kernel is built with frame pointers: every compiled function starts with `push rbp; mov rbp, rsp`. A `ret`
/// always returns to `[rsp]` (a `Call`); the two entry cases are a `Guess` (hand-written asm may push rbp
/// mid-routine, where `[rsp]` is a saved register).
pub fn entry_return_with(env: &WalkEnv<'_>, rip: u64, rsp: u64) -> Option<Step> {
    if !(env.is_text)(rip) || !rsp.is_multiple_of(8) {
        return None;
    }
    let mut w = Window::new(env, rsp)?;
    let at = |o: u64| rip.checked_add(o).and_then(|a| code_byte_with(env, a));
    let (slot, sure) = match at(0)? {
        0xC3 => (rsp, true),
        0x55 => (rsp, false),
        0x48 if at(1) == Some(0x89) && at(2) == Some(0xE5) && rip.checked_sub(1).and_then(|a| code_byte_with(env, a)) == Some(0x55) => {
            (rsp.checked_add(8)?, false)
        }
        _ => return None,
    };
    let ret = w.read(env, slot)?;
    (env.is_text)(ret).then_some(if sure { Step::Call(ret) } else { Step::Guess(ret) })
}

/// Walks the frame-pointer chain from `rbp` (a frame's base: `[rbp]` = the caller's rbp, `[rbp+8]` = the return
/// address) into `out`, reading only the stack `rsp` is on (`Window`); returns the steps written. Stops at a
/// null, misaligned or unreadable frame pointer, a return address outside kernel code, a frame pointer that does
/// not grow (one stack grows down, so callers sit higher) or jumps more than `MAX_FRAME_BYTES`, a trap from user
/// mode, or a full `out` (a bound on one report). At the trap stub's call of `hal_kernel_trap` the trap frame
/// right above it gives the interrupted rip, rsp and rbp, and the walk goes on there (possibly on another stack:
/// NMI, #DF and #MC run on IST stacks).
pub fn walk_with(env: &WalkEnv<'_>, rsp: u64, mut rbp: u64, out: &mut [Step]) -> usize {
    let mut n = 0;
    let mut prev: Option<u64> = None;
    let Some(mut win) = Window::new(env, rsp) else { return 0 };
    while n < out.len() {
        if rbp == 0 || !rbp.is_multiple_of(8) {
            break;
        }
        if let Some(p) = prev
            && (rbp <= p || rbp - p > MAX_FRAME_BYTES)
        {
            break;
        }
        let (Some(next), Some(ret)) = (win.read(env, rbp), rbp.checked_add(8).and_then(|a| win.read(env, a))) else { break };
        if (env.is_trap_return)(ret) {
            let f = rbp.wrapping_add(16);
            let mut rd = |o: u64| f.checked_add(o).and_then(|a| win.read(env, a));
            let (Some(int_rbp), Some(vector), Some(error), Some(rip), Some(cs)) =
                (rd(TF_RBP), rd(TF_VECTOR), rd(TF_ERROR), rd(TF_RIP), rd(TF_CS))
            else {
                break;
            };
            let user = cs & 3 != 0;
            out[n] = Step::Trap { vector, error, rip, user };
            n += 1;
            let Some(int_rsp) = rd(TF_RSP).filter(|_| !user) else { break };
            if n < out.len()
                && let Some(s) = entry_return_with(env, rip, int_rsp)
            {
                out[n] = s;
                n += 1;
            }
            // The interrupted code's stack (the IST stack held the frame): a new window, no ordering against it.
            let Some(w) = Window::new(env, int_rsp) else { break };
            win = w;
            prev = None;
            rbp = int_rbp;
            continue;
        }
        if !(env.is_text)(ret) {
            break;
        }
        out[n] = Step::Call(ret);
        n += 1;
        prev = Some(rbp);
        rbp = next;
    }
    n
}

fn live_env<R>(f: impl FnOnce(&WalkEnv<'_>) -> R) -> R {
    let read = |a: u64| read_kernel_u64(a);
    let code = |a: u64| read_code_u64(a);
    let env = WalkEnv { read: &read, code: &code, page_ok: &page_is_memory, is_trap_return: &is_trap_return, is_text: &is_kernel_text };
    f(&env)
}

/// Walks this CPU's own stack from `rbp`, a frame of a caller (see `walk_with`; the window starts at this
/// function's own stack pointer, below every caller's frame).
#[inline(never)]
pub fn walk(rbp: u64, out: &mut [Step]) -> usize {
    let rsp = imp::stack_pointer();
    live_env(|env| walk_with(env, rsp, rbp, out))
}

/// Walks the stack of interrupted code (`rsp`, `rbp` from its trap frame; the NMI handler). Crate-private: the
/// reads are sound only for a stack this CPU runs on or interrupted (`read_kernel_u64`), which hal's own NMI
/// handler guarantees and safe code elsewhere could not.
pub(crate) fn walk_from(rsp: u64, rbp: u64, out: &mut [Step]) -> usize {
    live_env(|env| walk_with(env, rsp, rbp, out))
}

/// `entry_return_with` on this kernel's memory (the NMI handler's first step).
pub fn entry_return(rip: u64, rsp: u64) -> Option<Step> {
    live_env(|env| entry_return_with(env, rip, rsp))
}

/// The frame pointer of the function this is inlined into (its own `rbp`; `[rbp+8]` is its return address).
#[inline(always)]
pub fn frame_pointer() -> u64 {
    imp::frame_pointer()
}

/// The lowest kernel-half address (canonical upper half).
const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

/// True if the page at `virt` is mapped in the live tables as ordinary write-back memory (not a device window:
/// a read there could clear a status register or end in a completion timeout).
pub fn page_is_memory(virt: u64) -> bool {
    virt >= KERNEL_HALF
        && crate::paging::translate(crate::paging::current_root(), virt)
            .is_some_and(|(_, fl)| fl.cache() == Some(crate::paging::CacheType::WriteBack))
}

/// Reads 8 aligned bytes of kernel memory if the current page tables map them as write-back memory (`None` for
/// user addresses, non-canonical ones, holes and device windows). Lock-free: walks the live tables through the
/// direct map. Whether the page can be unmapped between the check and the read is the caller's business: the
/// walker reads only the stack it runs on or interrupted (`Window`) and kernel code. Crate-private for that
/// reason: a safe caller elsewhere could hand it another thread's stack or a heap page that is unmapped or
/// written between the check and the read.
pub(crate) fn read_kernel_u64(addr: u64) -> Option<u64> {
    if !addr.is_multiple_of(8) || !page_is_memory(addr) {
        return None;
    }
    Some(imp::read_u64(addr))
}

/// Reads 8 aligned bytes of the kernel's code (the image's text, mapped for the kernel's whole life).
pub(crate) fn read_code_u64(addr: u64) -> Option<u64> {
    let (s, e) = kernel_text();
    if addr < (s & !7) || addr >= e {
        return None;
    }
    read_kernel_u64(addr)
}

/// True if `ret` is the return address of the trap stub's `call hal_kernel_trap` (`idt.rs`, and the paranoid
/// user path in `entry.rs`): the 5 bytes before it are `E8 rel32` and the call lands on `hal_kernel_trap`.
pub fn is_trap_return(ret: u64) -> bool {
    let target = imp::kernel_trap_addr();
    let at = ret.wrapping_sub(5);
    if target == 0 || !is_kernel_text(at) {
        return false;
    }
    let base = at & !7;
    let (Some(w0), Some(w1)) = (read_code_u64(base), read_code_u64(base + 8)) else { return false };
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&w0.to_le_bytes());
    b[8..].copy_from_slice(&w1.to_le_bytes());
    let o = (at & 7) as usize;
    if b[o] != 0xE8 {
        return false;
    }
    let rel = i32::from_le_bytes([b[o + 1], b[o + 2], b[o + 3], b[o + 4]]);
    ret.wrapping_add(rel as i64 as u64) == target
}

/// The kernel's code range `[__text_start, __text_end)` (linker symbols; empty in host builds).
pub fn kernel_text() -> (u64, u64) {
    imp::text_range()
}

pub fn is_kernel_text(a: u64) -> bool {
    let (s, e) = kernel_text();
    a >= s && a < e
}

// ---------------------------------------------------------------------------------------------------------
// The crash page: bitsboot's `CRASH_PAGE` KEEP record (never zeroed by the loader, never handed out by the frame
// allocator), where the panic path leaves its record for the next boot.

static CRASH_PHYS: AtomicU64 = AtomicU64::new(0);
static CRASH_LEN: AtomicU64 = AtomicU64::new(0);

/// Why `set_crash_page` refused a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPageError {
    /// BootInfo names no `CRASH_PAGE`.
    None,
    /// Empty, not 4 KiB aligned, or wrapping.
    Shape,
    /// BootInfo has no memory map, or the range is not wholly `BITS_KEEP` memory in it.
    NotKeep,
    /// A page of the range is not direct-mapped write-back RAM at its own physical address.
    NotMapped,
    /// A different crash page was registered already.
    Other,
}

/// True if `[start, end)` lies wholly inside `BITS_KEEP` descriptors of the map `descs` yields (adjacent ones may
/// share it). No allocation: repeatedly extends the covered prefix by a descriptor that contains its end.
pub fn keep_covers<I: Iterator<Item = bootinfo::MemDesc>>(descs: impl Fn() -> I, start: u64, end: u64) -> bool {
    if end <= start {
        return false;
    }
    let d_end = |d: &bootinfo::MemDesc| d.pages.checked_mul(PAGE).and_then(|l| d.phys.checked_add(l));
    let mut at = start;
    while at < end {
        let next = descs().filter(|d| d.ty == bootinfo::memtype::BITS_KEEP && d.phys <= at).filter_map(|d| d_end(&d)).filter(|&e| e > at).max();
        match next {
            Some(e) => at = e,
            None => return false,
        }
    }
    true
}

/// Pure part of `set_crash_page`: shape and KEEP coverage of `phys..phys+size`.
pub fn check_crash_page<I: Iterator<Item = bootinfo::MemDesc>>(descs: impl Fn() -> I, phys: u64, size: u64) -> Result<(), CrashPageError> {
    let end = phys.checked_add(size).ok_or(CrashPageError::Shape)?;
    if phys == 0 || size == 0 || !phys.is_multiple_of(PAGE) || !size.is_multiple_of(PAGE) || size > usize::MAX as u64 {
        return Err(CrashPageError::Shape);
    }
    if !keep_covers(descs, phys, end) {
        return Err(CrashPageError::NotKeep);
    }
    Ok(())
}

/// Registers the crash page from BootInfo's `CRASH_PAGE` record, after checking it: page-aligned, wholly
/// `BITS_KEEP` memory of BootInfo's memory map (bitsboot's pages, never handed to the frame allocator, so no
/// Rust object lives there), and direct-mapped write-back RAM at its own address (`kexec::crash` calls it at
/// boot). The same page again is fine; a different one is refused. The range comes from BootInfo, never from a
/// caller, so safe code cannot point the crash page's raw copies anywhere else.
pub fn set_crash_page(bi: &bootinfo::BootInfo<'_>) -> Result<(u64, u64), CrashPageError> {
    let r = bi.crash_page().ok_or(CrashPageError::None)?;
    let map = bi.memory_map().ok_or(CrashPageError::NotKeep)?;
    check_crash_page(|| map.iter(), r.phys, r.size)?;
    if !imp::direct_mapped(r.phys, r.size) {
        return Err(CrashPageError::NotMapped);
    }
    match CRASH_PHYS.compare_exchange(0, r.phys, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            CRASH_LEN.store(r.size, Ordering::Release);
            Ok((r.phys, r.size))
        }
        Err(p) if p == r.phys && CRASH_LEN.load(Ordering::Acquire) == r.size => Ok((r.phys, r.size)),
        Err(_) => Err(CrashPageError::Other),
    }
}

/// (physical address, length) of the crash page, once registered.
pub fn crash_page() -> Option<(u64, u64)> {
    let (p, l) = (CRASH_PHYS.load(Ordering::Acquire), CRASH_LEN.load(Ordering::Acquire));
    (p != 0 && l != 0).then_some((p, l))
}

/// Copies `data` into the crash page at `off` and writes the touched cache lines back to RAM (a reset drops
/// dirty lines; Linux ramoops maps its region uncached for the same reason); false (nothing written) without a
/// page or outside it. Two writers at once (a panic during a ktest or `crash_info`) only tear the record, which
/// its checksum shows.
pub fn crash_page_write(off: u64, data: &[u8]) -> bool {
    let Some((p, l)) = crash_page() else { return false };
    if off.checked_add(data.len() as u64).is_none_or(|e| e > l) {
        return false;
    }
    imp::copy_to_phys(p + off, data);
    crate::dma::flush_cache_range(crate::phys_to_virt(p + off), data.len() as u64);
    true
}

/// Copies from the crash page at `off` into `out`; false (nothing read) without a page or outside it.
pub fn crash_page_read(off: u64, out: &mut [u8]) -> bool {
    let Some((p, l)) = crash_page() else { return false };
    if off.checked_add(out.len() as u64).is_none_or(|e| e > l) {
        return false;
    }
    imp::copy_from_phys(p + off, out);
    true
}

#[cfg(target_os = "none")]
mod imp {
    unsafe extern "C" {
        static __text_start: u8;
        static __text_end: u8;
        fn hal_kernel_trap();
    }

    pub fn text_range() -> (u64, u64) {
        // Only the addresses of the linker-script labels are taken, never their contents.
        (&raw const __text_start as u64, &raw const __text_end as u64)
    }

    pub fn kernel_trap_addr() -> u64 {
        hal_kernel_trap as *const () as u64
    }

    #[inline(always)]
    pub fn frame_pointer() -> u64 {
        let v: u64;
        // SAFETY: reads the rbp register; no memory access.
        unsafe { core::arch::asm!("mov {}, rbp", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    #[inline(always)]
    pub fn stack_pointer() -> u64 {
        let v: u64;
        // SAFETY: reads the rsp register; no memory access.
        unsafe { core::arch::asm!("mov {}, rsp", out(reg) v, options(nomem, nostack, preserves_flags)) };
        v
    }

    pub fn read_u64(addr: u64) -> u64 {
        // SAFETY: `read_kernel_u64` checked that `addr` is 8-aligned and mapped as write-back memory in the live
        // tables, whose kernel half every root shares: no device register is read. Callers read only the kernel's
        // code (mapped for its whole life) or the stack this CPU runs on or interrupted (the walker's `Window`:
        // a contiguous mapped run from that rsp up, which nobody unmaps while the CPU is on it, and which ends at
        // the guard page below the next vmap window), so the mapping still holds when the read happens. The value
        // may be stale or garbage, which the walker's bounds absorb; volatile, because the word may change under
        // us (it is not a Rust object of ours).
        unsafe { core::ptr::read_volatile(addr as *const u64) }
    }

    /// Every page of `phys..phys+len` is mapped at its direct-map address as write-back memory onto itself.
    pub fn direct_mapped(phys: u64, len: u64) -> bool {
        let root = crate::paging::current_root();
        let mut p = phys;
        while p < phys + len {
            let v = crate::phys_to_virt(p);
            let ok = crate::paging::translate(root, v)
                .is_some_and(|(at, fl)| at == p && fl.cache() == Some(crate::paging::CacheType::WriteBack));
            if !ok {
                return false;
            }
            p += super::PAGE;
        }
        true
    }

    pub fn copy_to_phys(phys: u64, data: &[u8]) {
        let dst = crate::phys_to_virt(phys) as *mut u8;
        for (i, b) in data.iter().enumerate() {
            // SAFETY: `crash_page_write` bounded `phys..phys+len` by the registered crash page, which
            // `set_crash_page` took from BootInfo and checked: wholly `BITS_KEEP` memory of the memory map
            // (bitsboot's pages, never handed to the frame allocator, so no Rust object lives there) and
            // direct-mapped write-back RAM at its own address. Volatile byte stores: other CPUs may copy into the
            // same bytes at once (a ktest, a panic), which tears the record (its checksum shows it) but touches
            // nothing else.
            unsafe { dst.add(i).write_volatile(*b) };
        }
    }

    pub fn copy_from_phys(phys: u64, out: &mut [u8]) {
        let src = crate::phys_to_virt(phys) as *const u8;
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: as copy_to_phys (reading the same checked page into a buffer the caller owns).
            *b = unsafe { src.add(i).read_volatile() };
        }
    }

    pub fn write_back_caches() {
        // SAFETY: wbinvd writes every modified cache line back to memory and invalidates the caches; no other
        // effect (slow, which does not matter right before a reset).
        unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    extern crate std;
    use alloc::vec::Vec;
    use std::sync::Mutex;

    /// The host's stand-in for the crash page (tests).
    static PAGE: Mutex<Vec<u8>> = Mutex::new(Vec::new());

    pub fn text_range() -> (u64, u64) {
        (0, 0)
    }
    pub fn kernel_trap_addr() -> u64 {
        0
    }
    pub fn frame_pointer() -> u64 {
        0
    }
    pub fn stack_pointer() -> u64 {
        0
    }
    pub fn read_u64(_addr: u64) -> u64 {
        0
    }
    pub fn direct_mapped(_phys: u64, _len: u64) -> bool {
        true
    }
    fn with_page<R>(phys: u64, f: impl FnOnce(&mut Vec<u8>, usize) -> R) -> R {
        let (base, len) = super::crash_page().unwrap_or((phys, 0));
        let mut p = PAGE.lock().unwrap_or_else(|e| e.into_inner());
        if p.len() < len as usize {
            p.resize(len as usize, 0);
        }
        f(&mut p, (phys - base) as usize)
    }
    pub fn copy_to_phys(phys: u64, data: &[u8]) {
        with_page(phys, |p, o| p[o..o + data.len()].copy_from_slice(data))
    }
    pub fn copy_from_phys(phys: u64, out: &mut [u8]) {
        with_page(phys, |p, o| out.copy_from_slice(&p[o..o + out.len()]))
    }
    pub fn write_back_caches() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idt::TrapFrame;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    const TEXT: (u64, u64) = (0xFFFF_FFFF_8000_0000, 0xFFFF_FFFF_8040_0000);
    const STUB_RET: u64 = 0xFFFF_FFFF_8030_0040;

    struct Mem(RefCell<BTreeMap<u64, u64>>);
    impl Mem {
        fn new() -> Self {
            Mem(RefCell::new(BTreeMap::new()))
        }
        fn put(&self, a: u64, v: u64) {
            self.0.borrow_mut().insert(a, v);
        }
        fn frame(&self, rbp: u64, next: u64, ret: u64) {
            self.put(rbp, next);
            self.put(rbp + 8, ret);
        }
    }

    /// Every kernel-half page is ordinary memory except the ones in `holes`.
    fn run_in(m: &Mem, holes: &[u64], rsp: u64, rbp: u64, cap: usize) -> Vec<Step> {
        let read = |a: u64| m.0.borrow().get(&a).copied();
        let ok = |p: u64| p >= KERNEL_HALF && !holes.contains(&(p & !(PAGE - 1)));
        let trap = |r: u64| r == STUB_RET;
        let text = |a: u64| a >= TEXT.0 && a < TEXT.1;
        let env = WalkEnv { read: &read, code: &read, page_ok: &ok, is_trap_return: &trap, is_text: &text };
        let mut out = alloc::vec![Step::None; cap];
        let n = walk_with(&env, rsp, rbp, &mut out);
        out.truncate(n);
        out
    }

    fn run(m: &Mem, rbp: u64, cap: usize) -> Vec<Step> {
        run_in(m, &[], rbp, rbp, cap)
    }

    #[test]
    fn trap_frame_offsets_match_the_struct() {
        assert_eq!(core::mem::offset_of!(TrapFrame, rbp) as u64, TF_RBP);
        assert_eq!(core::mem::offset_of!(TrapFrame, vector) as u64, TF_VECTOR);
        assert_eq!(core::mem::offset_of!(TrapFrame, error) as u64, TF_ERROR);
        assert_eq!(core::mem::offset_of!(TrapFrame, rip) as u64, TF_RIP);
        assert_eq!(core::mem::offset_of!(TrapFrame, cs) as u64, TF_CS);
        assert_eq!(core::mem::offset_of!(TrapFrame, rsp) as u64, TF_RSP);
    }

    #[test]
    fn a_plain_chain_ends_at_the_null_frame() {
        let m = Mem::new();
        let s = 0xFFFF_C900_0001_0000u64;
        m.frame(s, s + 0x40, TEXT.0 + 0x100);
        m.frame(s + 0x40, s + 0x100, TEXT.0 + 0x200);
        m.frame(s + 0x100, 0, TEXT.0 + 0x300);
        let st = run(&m, s, 16);
        assert_eq!(st, [Step::Call(TEXT.0 + 0x100), Step::Call(TEXT.0 + 0x200), Step::Call(TEXT.0 + 0x300)]);
        // A full buffer stops the walk (a bound on one report).
        assert_eq!(run(&m, s, 2).len(), 2);
    }

    #[test]
    fn garbage_stops_the_walk() {
        let s = 0xFFFF_C900_0001_0000u64;
        // A return address outside the kernel's code.
        let m = Mem::new();
        m.frame(s, s + 0x40, 0x1234);
        assert!(run(&m, s, 8).is_empty());
        // A frame pointer that goes down (a loop) or jumps too far.
        let m = Mem::new();
        m.frame(s, s - 0x40, TEXT.0 + 1);
        m.frame(s - 0x40, s, TEXT.0 + 2);
        assert_eq!(run(&m, s, 8), [Step::Call(TEXT.0 + 1)]);
        let m = Mem::new();
        m.frame(s, s + MAX_FRAME_BYTES + 8, TEXT.0 + 1);
        m.frame(s + MAX_FRAME_BYTES + 8, 0, TEXT.0 + 2);
        assert_eq!(run(&m, s, 8), [Step::Call(TEXT.0 + 1)]);
        // Misaligned, unmapped, null.
        let m = Mem::new();
        m.frame(s, s + 0x41, TEXT.0 + 1);
        assert_eq!(run(&m, s, 8), [Step::Call(TEXT.0 + 1)]);
        assert!(run(&Mem::new(), s, 8).is_empty());
        assert!(run(&Mem::new(), 0, 8).is_empty());
    }

    #[test]
    fn a_trap_frame_is_crossed_onto_another_stack() {
        let m = Mem::new();
        let ist = 0xFFFF_C900_0100_0000u64;
        let thread = 0xFFFF_C900_0000_8000u64; // below the IST stack: no ordering across the crossing
        m.frame(ist, ist + 0x30, TEXT.0 + 0x10);
        // hal_kernel_trap's frame: its return address is the stub's call, the trap frame sits right above.
        m.frame(ist + 0x30, 0xdead_0000, STUB_RET);
        let tf = ist + 0x30 + 16;
        m.put(tf + TF_RBP, thread);
        m.put(tf + TF_VECTOR, 13);
        m.put(tf + TF_ERROR, 0);
        m.put(tf + TF_RIP, TEXT.0 + 0x500);
        m.put(tf + TF_CS, 0x08);
        m.put(tf + TF_RSP, thread - 0x40);
        m.frame(thread, thread + 0x20, TEXT.0 + 0x600);
        m.frame(thread + 0x20, 0, TEXT.0 + 0x700);
        assert_eq!(
            run(&m, ist, 16),
            [
                Step::Call(TEXT.0 + 0x10),
                Step::Trap { vector: 13, error: 0, rip: TEXT.0 + 0x500, user: false },
                Step::Call(TEXT.0 + 0x600),
                Step::Call(TEXT.0 + 0x700),
            ]
        );
    }

    #[test]
    fn a_function_caught_at_its_entry_or_ret_keeps_its_caller() {
        let m = Mem::new();
        let text = |a: u64| a >= TEXT.0 && a < TEXT.1;
        let read = |a: u64| m.0.borrow().get(&a).copied();
        let trap = |_: u64| false;
        let ok = |p: u64| p >= KERNEL_HALF;
        let env = WalkEnv { read: &read, code: &read, page_ok: &ok, is_trap_return: &trap, is_text: &text };
        // Code: f at +0x100 = 55 48 89 e5 .. c3; something else at +0x108.
        m.put(TEXT.0 + 0x100, u64::from_le_bytes([0x55, 0x48, 0x89, 0xE5, 0x90, 0xC3, 0x90, 0x90]));
        m.put(TEXT.0 + 0x108, u64::from_le_bytes([0x31, 0xC0, 0x90, 0x90, 0x90, 0x90, 0x90, 0x90]));
        let rsp = 0xFFFF_C900_0000_8000u64;
        m.put(rsp, TEXT.0 + 0x777);
        m.put(rsp + 8, TEXT.0 + 0x888);
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x100, rsp), Some(Step::Guess(TEXT.0 + 0x777)), "at push rbp");
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x101, rsp), Some(Step::Guess(TEXT.0 + 0x888)), "at mov rbp, rsp");
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x105, rsp), Some(Step::Call(TEXT.0 + 0x777)), "at ret: always right");
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x108, rsp), None, "inside the body: the chain is right");
        assert_eq!(entry_return_with(&env, 0x1000, rsp), None, "not kernel code");
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x105, 0x7fff_0000), None, "a user-mode rsp is not read");
        m.put(rsp, 0x40_0000);
        assert_eq!(entry_return_with(&env, TEXT.0 + 0x100, rsp), None, "a non-code word is no return address");
    }

    #[test]
    fn a_trap_from_user_mode_ends_the_walk() {
        let m = Mem::new();
        let ist = 0xFFFF_C900_0100_0000u64;
        m.frame(ist, 0, STUB_RET);
        let tf = ist + 16;
        m.put(tf + TF_RBP, 0x7fff_0000);
        m.put(tf + TF_VECTOR, 2);
        m.put(tf + TF_ERROR, 0);
        m.put(tf + TF_RIP, 0x40_1000);
        m.put(tf + TF_CS, 0x33);
        assert_eq!(run(&m, ist, 8), [Step::Trap { vector: 2, error: 0, rip: 0x40_1000, user: true }]);
    }

    #[test]
    fn ticks_follow_the_rate() {
        assert_eq!(ticks_for(1_000_000_000, 2_000_000_000), 2_000_000_000);
        assert_eq!(ticks_for(1_000_000_000, 0), ASSUMED_TSC_HZ);
        assert_eq!(ticks_for(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn the_walk_reads_only_the_stack_it_runs_on() {
        let m = Mem::new();
        let s = 0xFFFF_C900_0001_0000u64;
        m.frame(s + 0x100, s + 0x1100, TEXT.0 + 0x100);
        m.frame(s + 0x1100, 0, TEXT.0 + 0x200);
        // A frame pointer below the stack pointer (a program's rbp in the entry stubs, or garbage) is not read.
        assert!(run_in(&m, &[], s + 0x200, s + 0x100, 8).is_empty());
        // A user-mode stack pointer: nothing is read.
        assert!(run_in(&m, &[], 0x7fff_0000, s + 0x100, 8).is_empty());
        // A hole between the stack pointer and a frame ends the walk there (the next vmap window's guard page).
        assert_eq!(run_in(&m, &[s + 0x1000], s, s + 0x100, 8), [Step::Call(TEXT.0 + 0x100)]);
        assert_eq!(run_in(&m, &[], s, s + 0x100, 8), [Step::Call(TEXT.0 + 0x100), Step::Call(TEXT.0 + 0x200)]);
        // The stack pointer in an unmapped guard page (a stack overflow, the #DF case): the window starts on the
        // page above it, so the frames of the overflowing stack are still found.
        assert_eq!(run_in(&m, &[s - 0x1000], s - 0x10, s + 0x100, 8).len(), 2);
        // Two holes: nothing.
        assert!(run_in(&m, &[s - 0x1000, s], s - 0x10, s + 0x100, 8).is_empty());
        // Too far above the stack pointer for one stack.
        let far = s + MAX_STACK_WINDOW;
        m.frame(far, 0, TEXT.0 + 0x300);
        assert!(run_in(&m, &[], s, far, 8).is_empty());
    }

    fn map_with<'a>(descs: &'a [bootinfo::MemDesc]) -> impl Fn() -> core::iter::Copied<core::slice::Iter<'a, bootinfo::MemDesc>> + 'a {
        move || descs.iter().copied()
    }

    fn d(ty: u32, phys: u64, pages: u64) -> bootinfo::MemDesc {
        bootinfo::MemDesc { ty, phys, virt: 0, pages, attr: 0 }
    }

    #[test]
    fn only_a_keep_page_can_become_the_crash_page() {
        use bootinfo::memtype::{BITS_KEEP, CONVENTIONAL};
        let map = [d(CONVENTIONAL, 0x10_0000, 0x100), d(BITS_KEEP, 0x1F00_0000, 1), d(BITS_KEEP, 0x1F00_1000, 2), d(BITS_KEEP, 0x2000_0000, u64::MAX / 2)];
        assert_eq!(check_crash_page(map_with(&map), 0x1F00_0000, 4096), Ok(()));
        assert_eq!(check_crash_page(map_with(&map), 0x1F00_0000, 3 * 4096), Ok(()), "adjacent KEEP descriptors");
        assert_eq!(check_crash_page(map_with(&map), 0x1F00_0000, 4 * 4096), Err(CrashPageError::NotKeep));
        assert_eq!(check_crash_page(map_with(&map), 0x10_0000, 4096), Err(CrashPageError::NotKeep), "free RAM");
        assert_eq!(check_crash_page(map_with(&map), 0x1F00_0800, 4096), Err(CrashPageError::Shape));
        assert_eq!(check_crash_page(map_with(&map), 0x1F00_0000, 100), Err(CrashPageError::Shape));
        assert_eq!(check_crash_page(map_with(&map), 0, 4096), Err(CrashPageError::Shape));
        assert_eq!(check_crash_page(map_with(&map), u64::MAX & !0xFFF, 8192), Err(CrashPageError::Shape));
        // A descriptor whose end overflows is ignored, never a panic.
        assert_eq!(check_crash_page(map_with(&map), 0x2000_0000, 4096), Err(CrashPageError::NotKeep));
    }

    fn bootinfo_with(buf: &mut [u8], crash: Option<bootinfo::PhysRange>, keep: u64) -> usize {
        let mut w = bootinfo::Writer::new(buf).unwrap();
        let map = [d(bootinfo::memtype::BITS_KEEP, keep, 1)];
        w.memory_map(map.iter().copied()).unwrap();
        if let Some(r) = crash {
            w.crash_page(r).unwrap();
        }
        w.len()
    }

    #[test]
    fn the_crash_page_needs_registering_and_stays_in_bounds() {
        // Host: one registration for the whole test binary; only this test registers it.
        assert!(!crash_page_write(0, &[1]), "nothing registered yet");
        let mut buf = alloc::vec![0u8; 4096];
        let n = bootinfo_with(&mut buf, None, 0x1F00_0000);
        assert_eq!(set_crash_page(&bootinfo::BootInfo::parse(&buf[..n]).unwrap()), Err(CrashPageError::None));
        let n = bootinfo_with(&mut buf, Some(bootinfo::PhysRange { phys: 0x1F00_0000, size: 4096 }), 0x3000_0000);
        assert_eq!(set_crash_page(&bootinfo::BootInfo::parse(&buf[..n]).unwrap()), Err(CrashPageError::NotKeep));
        assert_eq!(crash_page(), None);
        let n = bootinfo_with(&mut buf, Some(bootinfo::PhysRange { phys: 0x1F00_0000, size: 4096 }), 0x1F00_0000);
        assert_eq!(set_crash_page(&bootinfo::BootInfo::parse(&buf[..n]).unwrap()), Ok((0x1F00_0000, 4096)));
        assert_eq!(set_crash_page(&bootinfo::BootInfo::parse(&buf[..n]).unwrap()), Ok((0x1F00_0000, 4096)), "the same page again");
        let n = bootinfo_with(&mut buf, Some(bootinfo::PhysRange { phys: 0x2000_0000, size: 4096 }), 0x2000_0000);
        assert_eq!(set_crash_page(&bootinfo::BootInfo::parse(&buf[..n]).unwrap()), Err(CrashPageError::Other), "a second page");
        assert_eq!(crash_page(), Some((0x1F00_0000, 4096)));
        assert!(crash_page_write(4000, &[7; 96]));
        assert!(!crash_page_write(4001, &[0; 96]));
        assert!(!crash_page_write(u64::MAX, &[0; 2]));
        let mut b = [0u8; 8];
        assert!(crash_page_read(4000, &mut b) && b == [7; 8]);
        assert!(!crash_page_read(4090, &mut b));
    }
}
