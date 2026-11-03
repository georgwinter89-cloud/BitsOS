//! AB-10a: the user segment state Wine's WoW64 needs, per thread and per process (owner: AB).
//!
//! - **Selectors**: a program's DS, ES, FS and GS stay in the CPU registers while its thread runs (the entry
//!   code never touches them); `switch_to` saves them for the thread it leaves and loads the next thread's,
//!   as Linux's `__switch_to` (linux-6.6/arch/x86/kernel/process_64.c:608-614, `save_fsgs`/`x86_fsgsbase_load`).
//!   The bases a program set stay in `UserContext::{fs_base, gs_base}`, which the return to user writes after
//!   the selector, so loading a selector here never changes what the program sees as its base.
//! - **TLS slots**: three GDT descriptors per thread (`set_thread_area`), written into the running CPU's GDT at
//!   every switch to the thread (Linux `load_TLS`).
//! - **LDT**: one per address space (`LdtSlot`, shared by the threads and a vfork child, copied by fork, gone
//!   at exec). Its table is 8192 descriptors (64 KiB, the architectural maximum) from the first write on and is
//!   changed in place one 8-byte descriptor at a time; nothing is ever freed while a CPU's LDTR points at it (the
//!   table lives as long as a thread of the process holds the slot, and a CPU loads only its running thread's
//!   LDT). After every write `modify_ldt` runs `refresh_cpu_ldt` synchronously on each CPU a sibling thread
//!   runs on (Linux `install_ldt` → `on_each_cpu_mm(flush_ldt)`), so the table and new DS/ES descriptors are
//!   live there before the call returns. Behind that, a generation counter (`sync_ldt`, checked with
//!   interrupts off right before every return to user) catches a thread that was being switched in meanwhile.
//!
//! Every selector the kernel loads itself is checked against the descriptor it names first (a program may
//! leave a selector whose descriptor it cleared since); one that would fault is loaded as 0, as Linux's
//! `loadsegment` fixup does. LDT descriptors are read under the slot's lock, which writers take too.

use crate::gdt::{self, TLS_ENTRIES, TLS_FIRST};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

/// Descriptors in an LDT (`LDT_ENTRIES`).
pub const LDT_ENTRIES: usize = 8192;

/// Tables created since boot: a CPU whose `ldt_seen` differs looks at its running thread's LDT again.
static LDT_EPOCH: AtomicU64 = AtomicU64::new(0);

/// A thread's data segment selectors (Linux `thread.{ds, es, fsindex, gsindex}`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selectors {
    pub ds: u16,
    pub es: u16,
    pub fs: u16,
    pub gs: u16,
}

impl Selectors {
    const fn pack(self) -> u64 {
        self.ds as u64 | (self.es as u64) << 16 | (self.fs as u64) << 32 | (self.gs as u64) << 48
    }
    const fn unpack(v: u64) -> Selectors {
        Selectors { ds: v as u16, es: (v >> 16) as u16, fs: (v >> 32) as u16, gs: (v >> 48) as u16 }
    }
}

/// What a new thread starts with: clone copies its creator's (`copy_thread`), a new program starts empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    sel: Selectors,
    tls: [u64; TLS_ENTRIES],
}

impl Snapshot {
    /// The TLS descriptors `install` stores: any that `user_desc_ok` refuses becomes empty.
    fn user_tls(&self) -> [u64; TLS_ENTRIES] {
        self.tls.map(|d| if user_desc_ok(d) { d } else { 0 })
    }
}

/// One thread's segment state (in its `switch::KernelContext`). `sel` is valid while the thread does not run;
/// `tls` and `ldt` are written only by the thread itself with interrupts off and read by `switch_to` for a
/// thread that does not run, so they never race.
pub struct ThreadSegs {
    sel: AtomicU64,
    tls: [AtomicU64; TLS_ENTRIES],
    /// From `Arc::into_raw` (null: no LDT slot, a kernel thread); released in `Drop`.
    ldt: AtomicPtr<LdtSlot>,
}

impl ThreadSegs {
    pub const fn new() -> ThreadSegs {
        ThreadSegs { sel: AtomicU64::new(0), tls: [const { AtomicU64::new(0) }; TLS_ENTRIES], ldt: AtomicPtr::new(core::ptr::null_mut()) }
    }

    fn ldt(&self) -> Option<&LdtSlot> {
        let p = self.ldt.load(Ordering::Acquire);
        // SAFETY: non-null values come from `Arc::into_raw` in `replace_ldt`; the reference they stand for is
        // released only by a later `replace_ldt` (the owning thread, not while this borrow of `self` runs on it)
        // or by `Drop`.
        (!p.is_null()).then(|| unsafe { &*p })
    }

    /// Puts `new` in place and hands back the old slot (dropped by the caller, outside its interrupt-off part).
    fn replace_ldt(&self, new: Option<Arc<LdtSlot>>) -> Option<Arc<LdtSlot>> {
        let p = new.map_or(core::ptr::null_mut(), |a| Arc::into_raw(a) as *mut LdtSlot);
        let old = self.ldt.swap(p, Ordering::AcqRel);
        // SAFETY: `old` came from `Arc::into_raw` (or is null); the swap hands it out once.
        (!old.is_null()).then(|| unsafe { Arc::from_raw(old) })
    }

    /// The TLS descriptors this thread runs with.
    pub fn tls(&self) -> [u64; TLS_ENTRIES] {
        core::array::from_fn(|i| self.tls[i].load(Ordering::Relaxed))
    }
}

impl Default for ThreadSegs {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ThreadSegs {
    fn drop(&mut self) {
        drop(self.replace_ldt(None));
    }
}

/// Why an LDT write failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LdtError {
    NoMemory,
    BadIndex,
    /// Not a descriptor a program may own (`user_desc_ok`).
    BadDescriptor,
}

/// Whether a program may own descriptor `d` in its LDT or a TLS slot: empty, or a code/data segment (S=1) with
/// DPL 3, as every descriptor Linux's `fill_ldt` builds. Never a system descriptor or gate, never a ring-0
/// segment. The trusted base checks this itself where it stores the descriptor.
pub const fn user_desc_ok(d: u64) -> bool {
    d == 0 || (d >> 44 & 1 == 1 && d >> 45 & 3 == 3)
}

/// The LDT of one address space (Linux `mm->context.ldt`).
pub struct LdtSlot {
    /// `LDT_ENTRIES` descriptors, allocated at the first write and freed with the slot.
    table: AtomicPtr<AtomicU64>,
    /// Linux's `nr_entries`: one past the highest entry ever written (what `modify_ldt` reads back).
    nr: AtomicU32,
    lock: AtomicBool,
}

const TABLE_LAYOUT: core::alloc::Layout = match core::alloc::Layout::from_size_align(LDT_ENTRIES * 8, 64) {
    Ok(l) => l,
    Err(_) => panic!("LDT layout"),
};

impl LdtSlot {
    pub fn new() -> Arc<LdtSlot> {
        Arc::new(LdtSlot { table: AtomicPtr::new(core::ptr::null_mut()), nr: AtomicU32::new(0), lock: AtomicBool::new(false) })
    }

    /// One past the highest entry written (0: no LDT).
    pub fn entries(&self) -> u32 {
        self.nr.load(Ordering::Acquire)
    }

    /// Descriptor `i` (0 when there is no table or `i` is out of range).
    pub fn entry(&self, i: usize) -> u64 {
        match self.table_ref() {
            Some(t) if i < LDT_ENTRIES => t[i].load(Ordering::Acquire),
            _ => 0,
        }
    }

    fn table_ref(&self) -> Option<&[AtomicU64]> {
        let p = self.table.load(Ordering::Acquire);
        // SAFETY: a non-null table is LDT_ENTRIES zero-initialised AtomicU64s from `alloc_table`, freed only in
        // `Drop` (no borrow of `self` outlives it).
        (!p.is_null()).then(|| unsafe { core::slice::from_raw_parts(p as *const AtomicU64, LDT_ENTRIES) })
    }

    /// Kernel address of the table (0: none yet), what the LDT descriptor in the GDT names.
    pub fn table_addr(&self) -> u64 {
        self.table.load(Ordering::Acquire) as u64
    }

    fn alloc_table() -> *mut AtomicU64 {
        // SAFETY: TABLE_LAYOUT has a non-zero size; zeroed memory is a valid AtomicU64 array (all descriptors
        // empty). Null on failure.
        unsafe { alloc::alloc::alloc_zeroed(TABLE_LAYOUT) as *mut AtomicU64 }
    }

    /// Runs `f` with the slot's lock held and interrupts off (writers and kernel loads of LDT selectors).
    fn locked<R>(&self, f: impl FnOnce() -> R) -> R {
        let _irq = crate::cpu::IrqGuard::new();
        // Bounded in practice: every holder runs a few instructions with interrupts off.
        while self.lock.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        let r = f();
        self.lock.store(false, Ordering::Release);
        r
    }

    /// Writes descriptor `i`, creating the table on the first write. True when this call created it (the
    /// caller then kicks the process's other threads so they load it, `sync_ldt`).
    pub fn write(&self, i: usize, desc: u64) -> Result<bool, LdtError> {
        if i >= LDT_ENTRIES {
            return Err(LdtError::BadIndex);
        }
        if !user_desc_ok(desc) {
            return Err(LdtError::BadDescriptor);
        }
        let mut created = false;
        if self.table.load(Ordering::Acquire).is_null() {
            let t = Self::alloc_table();
            if t.is_null() {
                return Err(LdtError::NoMemory);
            }
            match self.table.compare_exchange(core::ptr::null_mut(), t, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => created = true,
                // SAFETY: `t` was allocated above with TABLE_LAYOUT and never published.
                Err(_) => unsafe { alloc::alloc::dealloc(t as *mut u8, TABLE_LAYOUT) },
            }
        }
        let Some(t) = self.table_ref() else { return Err(LdtError::NoMemory) };
        self.locked(|| {
            t[i].store(desc, Ordering::Release);
            self.nr.fetch_max(i as u32 + 1, Ordering::AcqRel);
        });
        if created {
            LDT_EPOCH.fetch_add(1, Ordering::AcqRel);
        }
        Ok(created)
    }

    /// The copy fork makes (`ldt_dup_context`): same descriptors and count, its own table. None: no memory.
    pub fn fork_copy(&self) -> Option<Arc<LdtSlot>> {
        let n = LdtSlot::new();
        if let Some(src) = self.table_ref() {
            let t = Self::alloc_table();
            if t.is_null() {
                return None;
            }
            // SAFETY: `t` is a fresh LDT_ENTRIES table nobody else sees yet.
            let dst = unsafe { core::slice::from_raw_parts(t as *const AtomicU64, LDT_ENTRIES) };
            let nr = self.locked(|| {
                let nr = self.nr.load(Ordering::Acquire);
                for (d, s) in dst.iter().zip(src.iter()).take(nr as usize) {
                    d.store(s.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                nr
            });
            n.table.store(t, Ordering::Release);
            n.nr.store(nr, Ordering::Release);
            LDT_EPOCH.fetch_add(1, Ordering::AcqRel);
        }
        Some(n)
    }
}

impl Drop for LdtSlot {
    fn drop(&mut self) {
        let t = *self.table.get_mut();
        if !t.is_null() {
            // SAFETY: allocated by `alloc_table` with TABLE_LAYOUT; the last reference is gone, and no CPU has it
            // in its LDTR (a CPU loads only its running thread's LDT, and that thread holds a reference).
            unsafe { alloc::alloc::dealloc(t as *mut u8, TABLE_LAYOUT) };
        }
    }
}

/// Whether a data segment register may be loaded with descriptor `d` at CPL 0 with requested privilege `rpl`
/// (Intel SDM `MOV Sreg`: present code or data segment; code must be readable; DPL >= RPL unless conforming).
pub fn desc_loadable(d: u64, rpl: u16) -> bool {
    let typ = d >> 40 & 0xF;
    let (present, system) = (d >> 47 & 1 == 1, d >> 44 & 1 == 0);
    if !present || system {
        return false;
    }
    let code = typ & 8 != 0;
    if code && typ & 2 == 0 {
        return false;
    }
    (code && typ & 4 != 0) || (d >> 45 & 3) >= rpl as u64
}

/// Base address of a segment descriptor (32 bits).
pub fn desc_base(d: u64) -> u64 {
    (d >> 16 & 0xFF_FFFF) | (d >> 56 & 0xFF) << 24
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reg {
    Ds,
    Es,
    Fs,
    Gs,
}

/// Loads `sel` into `r` when its descriptor allows it, else 0; true when `sel` itself went in. Interrupts off;
/// for an LDT selector this CPU's LDTR holds `ldt`'s table.
fn load_checked(r: Reg, sel: u16, ldt: Option<&LdtSlot>) -> bool {
    if sel & !3 == 0 {
        imp::load(r, sel);
        return true;
    }
    let idx = (sel >> 3) as usize;
    if sel & 4 == 0 {
        let ok = desc_loadable(gdt::this().entry(idx), sel & 3);
        imp::load(r, if ok { sel } else { 0 });
        return ok;
    }
    match ldt {
        Some(l) => l.locked(|| {
            let ok = desc_loadable(l.entry(idx), sel & 3);
            imp::load(r, if ok { sel } else { 0 });
            ok
        }),
        None => {
            imp::load(r, 0);
            false
        }
    }
}

/// Loads `want` where it or the register's value `cur` is not null (Linux: `if (next->es | prev->es)`). True
/// when GS was loaded (IA32_KERNEL_GS_BASE then holds that segment's base, not the thread's user GS base).
fn load_selectors(cur: Selectors, want: Selectors, ldt: Option<&LdtSlot>) -> bool {
    for (r, c, w) in [(Reg::Ds, cur.ds, want.ds), (Reg::Es, cur.es, want.es), (Reg::Fs, cur.fs, want.fs)] {
        if c | w != 0 {
            load_checked(r, w, ldt);
        }
    }
    if cur.gs | want.gs != 0 {
        load_checked(Reg::Gs, want.gs, ldt);
        return true;
    }
    false
}

/// Makes this CPU's LDTR the table of `ldt` (or null) and notes the generation it looked at.
fn sync_cpu_ldt(g: &gdt::CpuGdt, ldt: Option<&LdtSlot>) {
    let epoch = LDT_EPOCH.load(Ordering::Acquire);
    let want = ldt.map_or(0, |l| l.table_addr());
    if g.ldt_base() != want {
        g.load_ldt(want);
    }
    g.ldt_seen.store(epoch, Ordering::Relaxed);
}

/// `switch_to`'s part (interrupts off, own CPU): saves the left thread's selectors, writes the next thread's
/// TLS slots, LDT and selectors. True when it loaded GS (the caller writes IA32_KERNEL_GS_BASE afterwards).
pub(crate) fn switch(prev: &ThreadSegs, next: &ThreadSegs, prev_done: bool) -> bool {
    let cur = imp::read();
    if !prev_done {
        prev.sel.store(cur.pack(), Ordering::Relaxed);
    }
    let g = gdt::this();
    for i in 0..TLS_ENTRIES {
        let w = next.tls[i].load(Ordering::Relaxed);
        if g.entry(TLS_FIRST + i) != w {
            g.set_tls_entry(i, w);
        }
    }
    let ldt = next.ldt();
    sync_cpu_ldt(g, ldt);
    load_selectors(cur, Selectors::unpack(next.sel.load(Ordering::Relaxed)), ldt)
}

/// The running thread's segment state, interrupts off while `f` runs. None before hal's switching runs here.
fn current<R>(f: impl FnOnce(&ThreadSegs, &gdt::CpuGdt) -> R) -> Option<R> {
    let _irq = crate::cpu::IrqGuard::new();
    crate::switch::with_current_segs(|s| f(s, gdt::this()))
}

/// The selectors in this CPU's registers now (the running thread's).
pub fn live() -> Selectors {
    imp::read()
}

/// What a thread created now by the running one starts with (`copy_thread`): its selectors and TLS slots;
/// `settls` (CLONE_SETTLS from 64-bit code) gives the child FS 0, as `do_set_thread_area_64(ARCH_SET_FS)`.
pub fn snapshot(settls: bool) -> Snapshot {
    let _irq = crate::cpu::IrqGuard::new();
    let mut sel = imp::read();
    if settls {
        sel.fs = 0;
    }
    let tls = crate::switch::with_current_segs(|s| s.tls()).unwrap_or_default();
    Snapshot { sel, tls }
}

/// Gives the running thread `snap` and the LDT `ldt` and loads them on this CPU (a new thread before its first
/// user instruction; exec with an empty snapshot and the new image's slot, Linux `flush_thread` +
/// `start_thread`).
pub fn install(snap: Snapshot, ldt: Option<Arc<LdtSlot>>) {
    let old = current(|s, g| {
        for (i, d) in snap.user_tls().into_iter().enumerate() {
            s.tls[i].store(d, Ordering::Relaxed);
            g.set_tls_entry(i, d);
        }
        let old = s.replace_ldt(ldt);
        let l = s.ldt();
        sync_cpu_ldt(g, l);
        load_selectors(imp::read(), snap.sel, l);
        old
    });
    drop(old);
}

/// The running thread's TLS descriptor `i` (0..3).
pub fn tls(i: usize) -> u64 {
    current(|s, _| s.tls.get(i).map_or(0, |t| t.load(Ordering::Relaxed))).unwrap_or(0)
}

/// `set_tls_desc` + the refresh of `do_set_thread_area`: the running thread's TLS slot `i` becomes `desc`, on
/// this CPU at once; a data segment register holding that slot's selector is loaded again, and for FS/GS the
/// new base is what `ctx` returns to (the return to user writes the bases after the selectors). False (nothing
/// changed) for a slot out of range or a descriptor a program may not own (`user_desc_ok`).
pub fn set_tls(i: usize, desc: u64, ctx: &mut super::UserContext) -> bool {
    if i >= TLS_ENTRIES || !user_desc_ok(desc) {
        return false;
    }
    let sel = (((TLS_FIRST + i) << 3) | 3) as u16;
    let _ = current(|s, g| {
        s.tls[i].store(desc, Ordering::Relaxed);
        g.set_tls_entry(i, desc);
        let cur = imp::read();
        for (r, c) in [(Reg::Ds, cur.ds), (Reg::Es, cur.es), (Reg::Fs, cur.fs), (Reg::Gs, cur.gs)] {
            if c == sel && load_checked(r, sel, None) {
                match r {
                    Reg::Fs => ctx.fs_base = desc_base(desc),
                    Reg::Gs => ctx.gs_base = desc_base(desc),
                    _ => {}
                }
            }
        }
    });
    true
}

/// arch_prctl(ARCH_SET_FS/ARCH_SET_GS) for the running thread: the selector becomes 0 (Linux `loadseg(FS, 0)`),
/// the base is the caller's to set in `UserContext`.
pub fn clear_selector(gs: bool) {
    let _irq = crate::cpu::IrqGuard::new();
    imp::load(if gs { Reg::Gs } else { Reg::Fs }, 0);
}

/// After the running thread wrote its process's LDT (`slot`): this CPU loads it if it had none, and DS/ES
/// holding an LDT selector are loaded again (Linux `install_ldt` → `flush_ldt` + `refresh_ldt_segments`).
pub fn ldt_written(slot: &LdtSlot) {
    refresh_cpu_ldt(slot_id(slot));
}

/// Identifies `slot` for `refresh_cpu_ldt` (an address, only ever compared).
pub fn slot_id(slot: &LdtSlot) -> u64 {
    slot as *const LdtSlot as u64
}

/// Linux `flush_ldt` + `refresh_ldt_segments` on the CPU this runs on, for the LDT `slot` (`slot_id`): when the
/// thread running here uses that LDT, the LDTR is made its table and DS/ES holding an LDT selector are loaded
/// again from the new descriptors. Meant as the `f` of `smp::call_on` from `modify_ldt` (hard-IRQ context:
/// atomics and the LDT lock, which every holder takes with interrupts off; no heap). Returns 1 when it applied.
pub fn refresh_cpu_ldt(slot: u64) -> u64 {
    current(|s, g| {
        let Some(l) = s.ldt().filter(|l| slot_id(l) == slot) else { return 0 };
        sync_cpu_ldt(g, Some(l));
        let cur = imp::read();
        for (r, c) in [(Reg::Ds, cur.ds), (Reg::Es, cur.es)] {
            if c & 4 != 0 {
                load_checked(r, c, Some(l));
            }
        }
        1
    })
    .unwrap_or(0)
}

/// Before each entry to user mode (`execute_or_stop`): a CPU that has not seen the newest LDT generation
/// compares its LDTR with the running thread's LDT. One relaxed load while no program ever made an LDT.
pub(crate) fn sync_ldt() {
    let e = LDT_EPOCH.load(Ordering::Relaxed);
    if e == 0 || gdt::this().ldt_seen.load(Ordering::Relaxed) == e {
        return;
    }
    let _ = current(|s, g| sync_cpu_ldt(g, s.ldt()));
}

#[cfg(target_os = "none")]
mod imp {
    use super::{Reg, Selectors};

    pub fn read() -> Selectors {
        let (ds, es, fs, gs): (u16, u16, u16, u16);
        // SAFETY: reading segment selectors changes no state and cannot fault.
        unsafe {
            core::arch::asm!(
                "mov {0:x}, ds", "mov {1:x}, es", "mov {2:x}, fs", "mov {3:x}, gs",
                out(reg) ds, out(reg) es, out(reg) fs, out(reg) gs,
                options(nomem, nostack, preserves_flags)
            )
        };
        Selectors { ds, es, fs, gs }
    }

    /// Interrupts off. `sel` is null or was checked against its descriptor (`load_checked`).
    pub fn load(r: Reg, sel: u16) {
        // SAFETY: a null selector always loads into DS/ES/FS/GS; any other `sel` names a present, readable,
        // privilege-compatible descriptor (`desc_loadable`) of this CPU's GDT or of the LDT its LDTR holds, read
        // under the LDT lock, so the load cannot fault. In 64-bit mode the kernel uses none of these bases. For
        // GS the two `swapgs` keep the kernel GS base in place: the load lands in the user half (then in
        // IA32_KERNEL_GS_BASE, which the caller rewrites); interrupts are off and NMI/#MC find their GS base by
        // APIC ID (`percpu::paranoid_gs_enter`).
        unsafe {
            match r {
                Reg::Ds => core::arch::asm!("mov ds, {0:x}", in(reg) sel, options(nostack, preserves_flags)),
                Reg::Es => core::arch::asm!("mov es, {0:x}", in(reg) sel, options(nostack, preserves_flags)),
                Reg::Fs => core::arch::asm!("mov fs, {0:x}", in(reg) sel, options(nostack, preserves_flags)),
                Reg::Gs => core::arch::asm!("swapgs", "mov gs, {0:x}", "swapgs", in(reg) sel, options(nostack, preserves_flags)),
            }
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::{Reg, Selectors};
    pub fn read() -> Selectors {
        Selectors::default()
    }
    pub fn load(_r: Reg, _sel: u16) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors_pack() {
        let s = Selectors { ds: 0x2b, es: 0x0f, fs: 0x63, gs: 0x73 };
        assert_eq!(Selectors::unpack(s.pack()), s);
        assert_eq!(Selectors::default().pack(), 0);
    }

    #[test]
    fn what_a_data_register_may_hold() {
        let t = gdt::bsp_tables();
        gdt::init();
        assert!(desc_loadable(t.entry(5), 3), "user data");
        assert!(desc_loadable(t.entry(4), 3), "readable 32-bit code");
        assert!(desc_loadable(t.entry(6), 3), "readable 64-bit code");
        assert!(desc_loadable(t.entry(2), 0) && !desc_loadable(t.entry(2), 3), "kernel data only with RPL 0");
        assert!(!desc_loadable(t.entry(8), 0), "TSS (system)");
        assert!(!desc_loadable(t.entry(9), 0), "the TSS's upper half");
        assert!(!desc_loadable(0, 3), "empty");
        let tls = 0x00CF_F300_0000_FFFFu64; // fill_ldt of a 32-bit writable data segment
        assert!(desc_loadable(tls, 3));
        assert!(!desc_loadable(tls & !(1 << 47), 3), "not present");
        assert!(!desc_loadable(0x00CF_F800_0000_FFFF, 3), "execute-only code");
        assert!(desc_loadable(0x00CF_FE00_0000_FFFF, 3), "readable code");
        assert!(desc_loadable(0x00CF_9E00_0000_FFFF, 3), "conforming readable code ignores DPL");
        assert_eq!(desc_base(0x12CF_F334_5678_FFFF), 0x1234_5678);
    }

    #[test]
    fn ldt_slot_writes_in_place() {
        let s = LdtSlot::new();
        assert_eq!((s.entries(), s.table_addr(), s.entry(3)), (0, 0, 0));
        assert_eq!(s.write(LDT_ENTRIES, 1), Err(LdtError::BadIndex));
        let e0 = LDT_EPOCH.load(Ordering::Relaxed);
        const A: u64 = 0x00CF_F300_0000_FFFF; // 32-bit data, DPL 3
        const B: u64 = 0x00CF_FA00_0000_FFFF; // 32-bit code, DPL 3
        assert_eq!(s.write(3, A), Ok(true), "the first write creates the table");
        assert!(LDT_EPOCH.load(Ordering::Relaxed) > e0);
        let at = s.table_addr();
        assert_ne!(at, 0);
        assert_eq!(s.write(1, B), Ok(false));
        assert_eq!((s.entries(), s.entry(1), s.entry(3), s.entry(2)), (4, B, A, 0));
        assert_eq!(s.table_addr(), at, "never moves");
        let c = s.fork_copy().unwrap();
        assert_ne!(c.table_addr(), at);
        assert_eq!((c.entries(), c.entry(1), c.entry(3)), (4, B, A));
        s.write(1, 0).unwrap();
        assert_eq!((s.entry(1), c.entry(1), s.entries()), (0, B, 4), "the copy is its own");
        let empty = LdtSlot::new().fork_copy().unwrap();
        assert_eq!(empty.table_addr(), 0);
    }

    #[test]
    fn only_user_descriptors_are_stored() {
        const GATE: u64 = 0x0000_EC00_0008_0000; // DPL-3 call gate (S=0)
        const RING0_CODE: u64 = 0x00CF_9A00_0000_FFFF;
        const RING0_DATA: u64 = 0x00CF_9200_0000_FFFF;
        const LDT_SYS: u64 = 0x0000_E200_0000_FFFF; // an LDT descriptor (system, DPL 3)
        for bad in [GATE, RING0_CODE, RING0_DATA, LDT_SYS, 1 << 47] {
            assert!(!user_desc_ok(bad), "{bad:#x}");
        }
        assert!(user_desc_ok(0) && user_desc_ok(0x00CF_F300_0000_FFFF) && user_desc_ok(0x00CF_FA00_0000_FFFF));
        assert!(user_desc_ok(0x00CF_7300_0000_FFFF), "not present, still S=1 DPL 3 (seg_not_present)");
        let s = LdtSlot::new();
        assert_eq!(s.write(0, GATE), Err(LdtError::BadDescriptor));
        assert_eq!(s.write(0, RING0_CODE), Err(LdtError::BadDescriptor));
        assert_eq!((s.table_addr(), s.entries()), (0, 0), "a refused write creates nothing");
        let mut ctx = super::super::UserContext::default();
        assert!(!set_tls(0, GATE, &mut ctx));
        assert!(!set_tls(1, RING0_DATA, &mut ctx));
        assert!(!set_tls(TLS_ENTRIES, 0, &mut ctx));
        // `install` keeps only user descriptors from a snapshot (built here as only hal can).
        let snap = Snapshot { sel: Selectors::default(), tls: [GATE, 0x00CF_F300_0000_FFFF, RING0_CODE] };
        assert_eq!(snap.user_tls(), [0, 0x00CF_F300_0000_FFFF, 0]);
    }

    #[test]
    fn thread_segs_hold_the_slot() {
        let t = ThreadSegs::new();
        let s = LdtSlot::new();
        assert!(t.replace_ldt(Some(s.clone())).is_none());
        assert_eq!(Arc::strong_count(&s), 2);
        assert!(core::ptr::eq(t.ldt().unwrap(), &*s));
        drop(t);
        assert_eq!(Arc::strong_count(&s), 1, "dropping the context releases it");
    }
}
