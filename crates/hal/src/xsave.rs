//! User FPU/SIMD state (owner: SC, item SC-1; spec/core.md §3.3, Linux `arch/x86/kernel/fpu/xstate.c`).
//!
//! Every thread owns one XSAVE area, sized from CPUID 0xD for the XCR0 that CP-1 set (x87, SSE, AVX and, on
//! the Mini-PC, the three AVX-512 states). `switch::switch_to` saves the outgoing thread's registers and
//! restores the incoming thread's eagerly at every switch. The kernel itself never uses vector registers (R10,
//! soft-float code generation), so the registers between two switches always belong to the thread that runs.
//!
//! Instruction choice (the best the CPU offers): XSAVES/XRSTORS (compacted format, init and modified
//! optimisation; IA32_XSS is kept 0, so only user states are in the area; needs XSAVEC too, see below), else
//! XSAVEOPT/XRSTOR, else XSAVE/XRSTOR (standard format). No FXSAVE path: R10 requires XSAVE, and the kernel
//! refuses CPUs without it.
//!
//! Modified optimisation (SDM 13.6, 13.11): XSAVEOPT and XSAVES skip a component that is unchanged since the last
//! XRSTOR(S) *from the same linear address*. That is only correct if the area still holds what was restored.
//! Invariant kept here: the optimising instruction is used only when this area was the last one restored on this
//! CPU (per-CPU `LAST_RESTORED`, keyed by a unique area id, not by address, so a new area at a freed area's
//! address never matches). Every other save (the boot thread's first switch, test areas) uses the
//! non-optimising twin with the same format: XSAVEC for XSAVES (identical compacted image because IA32_XSS = 0),
//! XSAVE for XSAVEOPT. Software that writes into an area (today only `write_initial`; later AB's sigreturn)
//! must give it a new id (`renew_id`) so a stale match is impossible (Linux: `__cpu_invalidate_fpregs_state`).
//!
//! Access rule: `save` and `restore` touch the live registers and are `pub(crate)` (switch.rs only); kernel
//! code outside hal can read a copy of an area but never load another thread's state or overwrite a saved one.
//! Every access to an area holds its flag with interrupts off, so it is short and bounded (B4).
//!
//! Rule for whoever changes XCR0 later (PKRU, AMX, APX per CP-1's notes): every existing area is sized for the
//! XCR0 of `init_this_cpu`; enabling a state needs new areas (or Linux's per-process XFD permission scheme).
//! Signal frames (AB) use the standard format: a user-supplied image must be validated (XSTATE_BV within XCR0,
//! MXCSR reserved bits clear) and converted before it is restored through an XSAVES area.

use alloc::vec;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use crate::cpu;
use crate::percpu::PerCpu;

/// Which save/restore instruction pair is used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// CR4.OSXSAVE is off (never on a supported CPU): nothing is saved.
    None = 0,
    Xsave = 1,
    Xsaveopt = 2,
    Xsaves = 3,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Xsave => "xsave",
            Mode::Xsaveopt => "xsaveopt",
            Mode::Xsaves => "xsaves",
        }
    }
    fn from_u8(v: u8) -> Mode {
        match v {
            1 => Mode::Xsave,
            2 => Mode::Xsaveopt,
            3 => Mode::Xsaves,
            _ => Mode::None,
        }
    }
    fn compacted(self) -> bool {
        self == Mode::Xsaves
    }
}

/// The best mode for a CPU (pure, host-tested). XSAVES is chosen only with XSAVEC, its non-optimising twin
/// (every CPU with XSAVES has it; a CPU without falls back to XSAVEOPT).
pub fn choose_mode(osxsave: bool, xsaveopt: bool, xsaves: bool, xsavec: bool) -> Mode {
    let xsaves = xsaves && xsavec;
    match (osxsave, xsaves, xsaveopt) {
        (false, _, _) => Mode::None,
        (true, true, _) => Mode::Xsaves,
        (true, false, true) => Mode::Xsaveopt,
        (true, false, false) => Mode::Xsave,
    }
}

/// Legacy region (FXSAVE image) plus the XSAVE header: the fixed start of every area.
pub const LEGACY_AND_HEADER: u32 = 512 + 64;
const XSTATE_BV_OFFSET: usize = 512;
const XCOMP_BV_OFFSET: usize = 520;
const XCOMP_BV_COMPACTED: u64 = 1 << 63;

/// Size of a compacted area for `mask` (SDM 13.4.3): components 2.. follow the header in bit order, each
/// 64-byte aligned when CPUID 0xD.i ECX[1] says so. `comp(i)` = (size, align64). Pure, host-tested.
pub fn compacted_size(mask: u64, comp: &dyn Fn(u32) -> (u32, bool)) -> u32 {
    let mut off = LEGACY_AND_HEADER;
    for i in 2..63 {
        if mask & (1 << i) != 0 {
            let (size, align) = comp(i);
            if align {
                off = (off + 63) & !63;
            }
            off += size;
        }
    }
    off
}

/// Size of a standard-format area for `mask`: the end of the last component. `comp(i)` = (size, offset).
pub fn standard_size(mask: u64, comp: &dyn Fn(u32) -> (u32, u32)) -> u32 {
    let mut end = LEGACY_AND_HEADER;
    for i in 2..63 {
        if mask & (1 << i) != 0 {
            let (size, offset) = comp(i);
            end = end.max(offset + size);
        }
    }
    end
}

/// What `init_this_cpu` found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XLayout {
    pub mode: Mode,
    /// The states saved (= XCR0): the requested-feature bitmap of every save and restore.
    pub mask: u64,
    /// Bytes per area for `mode` (64-byte multiple).
    pub size: u32,
    /// What CPUID 0xD reports for the mode (0.EBX standard, 1.EBX compacted).
    pub cpuid_size: u32,
    /// What the per-component sums give (should equal `cpuid_size`; the larger one is used).
    pub computed_size: u32,
    /// Standard-format size, used by the fallback modes and their self-test.
    pub standard_size: u32,
}

const UNSET: u8 = 0xFF;
const BUSY: u8 = 0xFE;
static MODE: AtomicU8 = AtomicU8::new(UNSET);
static MASK: AtomicU64 = AtomicU64::new(0);
static SIZE: AtomicU32 = AtomicU32::new(0);
static STD_SIZE: AtomicU32 = AtomicU32::new(0);
static CPUID_SIZE: AtomicU32 = AtomicU32::new(0);
static COMPUTED_SIZE: AtomicU32 = AtomicU32::new(0);

fn round64(v: u32) -> u32 {
    (v + 63) & !63
}

fn probe() -> XLayout {
    let v = cpu::live();
    let osxsave = cpu::read_cr4() & cpu::CR4_OSXSAVE != 0;
    let mode = choose_mode(osxsave, v.has("xsaveopt"), v.has("xsaves"), v.has("xsavec"));
    let mask = if mode == Mode::None { 0 } else { cpu::read_xcr0() };
    let std_cpuid = v.get(0xD, 0).ebx;
    let std_comp = standard_size(mask, &|i| {
        let r = v.get(0xD, i);
        (r.eax, r.ebx)
    });
    let (cpuid_size, computed_size) = match mode {
        Mode::None => (0, 0),
        Mode::Xsaves => (
            v.get(0xD, 1).ebx,
            compacted_size(mask, &|i| {
                let r = v.get(0xD, i);
                (r.eax, r.ecx & 2 != 0)
            }),
        ),
        Mode::Xsave | Mode::Xsaveopt => (std_cpuid, std_comp),
    };
    let size = if mode == Mode::None { 0 } else { round64(cpuid_size.max(computed_size).max(LEGACY_AND_HEADER)) };
    XLayout { mode, mask, size, cpuid_size, computed_size, standard_size: round64(std_cpuid.max(std_comp).max(LEGACY_AND_HEADER)) }
}

/// Per-CPU set-up (sched::init on the BSP; CP-4 calls it on every AP after `cpu::init_this_cpu`): IA32_XSS = 0
/// when XSAVES is used (no supervisor state in the areas), then the layout. The first CPU publishes it; later
/// CPUs must match (CP refuses CPUs whose CPUID differs), and each CPU's own finding is returned for the log.
pub fn init_this_cpu() -> XLayout {
    if cpu::read_cr4() & cpu::CR4_OSXSAVE != 0 && cpu::live().has("xsaves") {
        crate::msr::write(crate::msr::IA32_XSS, 0);
    }
    let l = probe();
    if MODE.compare_exchange(UNSET, BUSY, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        MASK.store(l.mask, Ordering::Relaxed);
        SIZE.store(l.size, Ordering::Relaxed);
        STD_SIZE.store(l.standard_size, Ordering::Relaxed);
        CPUID_SIZE.store(l.cpuid_size, Ordering::Relaxed);
        COMPUTED_SIZE.store(l.computed_size, Ordering::Relaxed);
        MODE.store(l.mode as u8, Ordering::Release);
    }
    l
}

/// The published layout (sets it up on first use).
pub fn layout() -> XLayout {
    if MODE.load(Ordering::Acquire) == UNSET {
        init_this_cpu();
    }
    let mut m = MODE.load(Ordering::Acquire);
    while m == UNSET || m == BUSY {
        core::hint::spin_loop();
        m = MODE.load(Ordering::Acquire);
    }
    XLayout {
        mode: Mode::from_u8(m),
        mask: MASK.load(Ordering::Relaxed),
        size: SIZE.load(Ordering::Relaxed),
        cpuid_size: CPUID_SIZE.load(Ordering::Relaxed),
        computed_size: COMPUTED_SIZE.load(Ordering::Relaxed),
        standard_size: STD_SIZE.load(Ordering::Relaxed),
    }
}

/// Whether this CPU can use `mode` (for the self-test of the fallback modes).
pub fn mode_supported(mode: Mode) -> bool {
    let v = cpu::live();
    let osx = cpu::read_cr4() & cpu::CR4_OSXSAVE != 0;
    match mode {
        Mode::None => true,
        Mode::Xsave => osx,
        Mode::Xsaveopt => osx && v.has("xsaveopt"),
        Mode::Xsaves => osx && v.has("xsaves") && v.has("xsavec"),
    }
}

/// Id of the area each CPU restored last (0 = none or unknown): the modified-optimisation invariant above.
static LAST_RESTORED: PerCpu<AtomicU64> = PerCpu::new(|_| AtomicU64::new(0));
/// Area ids, unique for the life of the kernel (0 is never handed out).
static NEXT_AREA_ID: AtomicU64 = AtomicU64::new(1);

/// One thread's saved user FPU/SIMD state: a heap area, 64-byte aligned.
pub struct FpuState {
    area: NonNull<u8>,
    size: u32,
    mode: Mode,
    mask: u64,
    /// Unique id of the area's current contents (see the module doc); renewed whenever software writes it.
    id: AtomicU64,
    /// Held (with interrupts off) while the area is read or written, so shared references never race on it
    /// (uncontended in the switch path: a thread runs on one CPU at a time).
    busy: AtomicBool,
}

// SAFETY: the area is owned by this value; `write_initial` runs before the value exists for anybody else, and
// every later access (`save`, `restore`, `to_vec`) holds `busy`, so it is never read and written at the same
// time from two CPUs.
unsafe impl Send for FpuState {}
// SAFETY: as above.
unsafe impl Sync for FpuState {}

impl FpuState {
    /// The state a new Linux process starts with (FCW 0x37F, MXCSR 0x1F80, every other state in its init
    /// configuration). `None` when the heap has no room (the caller reports a spawn failure).
    pub fn initial() -> Option<Self> {
        Self::initial_with(layout().mode)
    }

    /// As `initial`, for a given mode (the fallback-mode self-test); `None` if this CPU cannot use it.
    pub fn initial_with(mode: Mode) -> Option<Self> {
        let l = layout();
        if !mode_supported(mode) || (mode != Mode::None && l.mode == Mode::None) {
            return None;
        }
        let size = match mode {
            Mode::None => LEGACY_AND_HEADER,
            Mode::Xsaves if l.mode == Mode::Xsaves => l.size,
            Mode::Xsaves => return None,
            Mode::Xsave | Mode::Xsaveopt => l.standard_size,
        };
        let lay = core::alloc::Layout::from_size_align(size as usize, 64).ok()?;
        // SAFETY: the layout is valid and non-zero (>= 576 bytes).
        let p = unsafe { alloc::alloc::alloc_zeroed(lay) };
        let s = FpuState { area: NonNull::new(p)?, size, mode, mask: l.mask, id: AtomicU64::new(0), busy: AtomicBool::new(false) };
        s.write_initial();
        Some(s)
    }

    /// Gives the area a new id after software wrote it, so no CPU's `LAST_RESTORED` matches it any more and the
    /// next save does not use the modified optimisation on stale contents.
    fn renew_id(&self) {
        self.id.store(NEXT_AREA_ID.fetch_add(1, Ordering::Relaxed), Ordering::Release);
    }

    fn write_initial(&self) {
        self.renew_id();
        // SAFETY: `size` bytes allocated by `initial_with`; called before the value is shared, so this is the
        // only access.
        let a = unsafe { core::slice::from_raw_parts_mut(self.area.as_ptr(), self.size as usize) };
        a.fill(0);
        a[0..2].copy_from_slice(&0x037Fu16.to_le_bytes());
        a[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        a[28..32].copy_from_slice(&0xFFFFu32.to_le_bytes());
        // x87 and SSE present with the values above; every other state in its init configuration.
        let bv = cpu::XSTATE_X87 | cpu::XSTATE_SSE;
        a[XSTATE_BV_OFFSET..XSTATE_BV_OFFSET + 8].copy_from_slice(&bv.to_le_bytes());
        let comp = if self.mode.compacted() { XCOMP_BV_COMPACTED | self.mask } else { 0 };
        a[XCOMP_BV_OFFSET..XCOMP_BV_OFFSET + 8].copy_from_slice(&comp.to_le_bytes());
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn size(&self) -> u32 {
        self.size
    }

    fn hold(&self) -> Held<'_> {
        // Interrupts off first: the holder cannot be preempted or interrupted while it has the flag, so a waiter
        // (another CPU, or the switch path) spins for one copy of the area at most (B4).
        let irq = cpu::IrqGuard::new();
        while self.busy.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        Held { flag: &self.busy, _irq: irq }
    }

    /// A copy of the area (tests; later the signal frame and core dumps, after format conversion).
    pub fn to_vec(&self) -> Vec<u8> {
        let _h = self.hold();
        // SAFETY: `size` bytes we own, read while `busy` is held.
        unsafe { core::slice::from_raw_parts(self.area.as_ptr(), self.size as usize) }.to_vec()
    }

    /// XSTATE_BV from the header: which states hold non-init values.
    pub fn xstate_bv(&self) -> u64 {
        let v = self.to_vec();
        u64::from_le_bytes(v[XSTATE_BV_OFFSET..XSTATE_BV_OFFSET + 8].try_into().unwrap_or([0; 8]))
    }

    /// XCOMP_BV from the header (bit 63 = compacted format).
    pub fn xcomp_bv(&self) -> u64 {
        let v = self.to_vec();
        u64::from_le_bytes(v[XCOMP_BV_OFFSET..XCOMP_BV_OFFSET + 8].try_into().unwrap_or([0; 8]))
    }

    /// Saves this CPU's user registers into the area. The optimising instruction only when this area was the
    /// last one restored on this CPU (module doc).
    pub(crate) fn save(&self) {
        let _h = self.hold();
        let last = LAST_RESTORED.get().load(Ordering::Relaxed);
        imp::save(save_insn(self.mode, last == self.id.load(Ordering::Acquire)), self.area.as_ptr(), self.mask);
    }

    /// Loads the area into this CPU's user registers.
    pub(crate) fn restore(&self) {
        let _h = self.hold();
        imp::restore(self.mode, self.area.as_ptr(), self.mask);
        LAST_RESTORED.get().store(self.id.load(Ordering::Acquire), Ordering::Relaxed);
    }
}

/// The save instruction for `mode`; `optimise` = the area is the one this CPU restored last. Pure, host-tested.
fn save_insn(mode: Mode, optimise: bool) -> Insn {
    match (mode, optimise) {
        (Mode::None, _) => Insn::None,
        (Mode::Xsaves, true) => Insn::Xsaves,
        (Mode::Xsaves, false) => Insn::Xsavec,
        (Mode::Xsaveopt, true) => Insn::Xsaveopt,
        (Mode::Xsaveopt, false) | (Mode::Xsave, _) => Insn::Xsave,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Insn {
    None,
    Xsave,
    Xsaveopt,
    Xsavec,
    Xsaves,
}

struct Held<'a> {
    flag: &'a AtomicBool,
    // Dropped after `drop` below cleared the flag: interrupts come back only once the area is free.
    _irq: cpu::IrqGuard,
}
impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// Self-test of one mode (ktest `sc.xsave.modes`). Uses private areas only and overwrites the calling thread's
/// own user registers (like `load_user_regs`), nothing else. Checks: a save into an area never restored here
/// (non-optimising instruction) and a save right after its restore (optimising instruction) both give the
/// registers back after a clobber; a fresh area restores the clean start state. `Ok(false)`: this CPU cannot
/// use `mode`.
pub fn selftest_roundtrip(mode: Mode) -> Result<bool, alloc::string::String> {
    use alloc::format;
    let Some(area) = FpuState::initial_with(mode) else { return Ok(false) };
    let _irq = cpu::IrqGuard::new();
    let a = RegDump::pattern(0xA11CE ^ mode as u64);
    let b = RegDump::pattern(0xB0B ^ mode as u64);
    let c = RegDump::pattern(0xC0C0A ^ mode as u64);
    if !load_user_regs(&a) {
        return Err("pattern shape".into());
    }
    area.save();
    load_user_regs(&b);
    area.restore();
    if let Some(d) = read_user_regs().diff(&a) {
        return Err(format!("{}: after a first save/restore {d}", mode.name()));
    }
    // Now the area is the last restored one: the save below may skip unchanged components.
    load_user_regs(&c);
    area.save();
    load_user_regs(&b);
    area.restore();
    if let Some(d) = read_user_regs().diff(&c) {
        return Err(format!("{}: after an optimised save/restore {d}", mode.name()));
    }
    let fresh = FpuState::initial_with(mode).ok_or("no memory")?;
    fresh.restore();
    let got = read_user_regs();
    let mut clean = RegDump::pattern(0);
    clean.mxcsr = 0x1F80;
    clean.fcw = 0x037F;
    clean.regs.fill(0);
    clean.kmask.fill(0);
    if let Some(d) = got.diff(&clean) {
        return Err(format!("{}: initial state not clean: {d}", mode.name()));
    }
    Ok(true)
}

impl Drop for FpuState {
    fn drop(&mut self) {
        if let Ok(lay) = core::alloc::Layout::from_size_align(self.size as usize, 64) {
            // SAFETY: allocated in `initial_with` with exactly this layout, freed once.
            unsafe { alloc::alloc::dealloc(self.area.as_ptr(), lay) };
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Test helpers: put known values into the user vector registers and read them back (SC-1's ktests, later the
// signal-frame tests). They overwrite the live user state of the calling thread, nothing else.

/// Registers as the test helpers see them: MXCSR, x87 control word, the vector registers (16 x 32 bytes with
/// AVX, 32 x 64 bytes with AVX-512) and the AVX-512 mask registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegDump {
    pub mxcsr: u32,
    pub fcw: u16,
    pub reg_bytes: usize,
    pub regs: Vec<u8>,
    pub kmask: Vec<u64>,
}

/// (vector register count, bytes each, mask register count) for this CPU's XCR0. The mask registers are
/// read with 64-bit `kmovq`, which needs AVX512BW (every AVX-512 CPU BitsOS targets has it); without it they
/// are left out of the dumps.
pub fn reg_shape() -> (usize, usize, usize) {
    let (n, b, k) = shape_for(cpu::read_xcr0());
    (n, b, if cpu::live().has("avx512bw") { k } else { 0 })
}

fn shape_for(xcr0: u64) -> (usize, usize, usize) {
    if xcr0 & cpu::XSTATE_AVX512 == cpu::XSTATE_AVX512 {
        (32, 64, 8)
    } else if xcr0 & cpu::XSTATE_AVX != 0 {
        (16, 32, 0)
    } else {
        (16, 16, 0)
    }
}

impl RegDump {
    /// A dump of this CPU's shape filled from `seed` (every byte differs between seeds and registers).
    pub fn pattern(seed: u64) -> RegDump {
        Self::pattern_for(reg_shape(), seed)
    }

    fn pattern_for((n, b, k): (usize, usize, usize), seed: u64) -> RegDump {
        let mut regs = vec![0u8; n * b];
        let mut x = seed ^ 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            // xorshift64: deterministic, never zero for a non-zero start.
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for c in regs.chunks_mut(8) {
            c.copy_from_slice(&next().to_le_bytes()[..c.len()]);
        }
        let kmask = (0..k).map(|_| next()).collect();
        // Masked exceptions and a rounding mode from the seed: a leak between threads shows up here too.
        let rc = (seed & 3) as u32;
        RegDump { mxcsr: 0x1F80 | rc << 13, fcw: 0x037F | (rc as u16) << 10, reg_bytes: b, regs, kmask }
    }

    /// The bytes of vector register `i`.
    pub fn reg(&self, i: usize) -> &[u8] {
        &self.regs[i * self.reg_bytes..(i + 1) * self.reg_bytes]
    }
    pub fn reg_mut(&mut self, i: usize) -> &mut [u8] {
        let b = self.reg_bytes;
        &mut self.regs[i * b..(i + 1) * b]
    }

    /// The first difference to `other`, as text (`None` = equal).
    pub fn diff(&self, other: &RegDump) -> Option<alloc::string::String> {
        if self.mxcsr != other.mxcsr {
            return Some(alloc::format!("MXCSR {:#x}, expected {:#x}", self.mxcsr, other.mxcsr));
        }
        if self.fcw != other.fcw {
            return Some(alloc::format!("FCW {:#x}, expected {:#x}", self.fcw, other.fcw));
        }
        if self.reg_bytes != other.reg_bytes || self.regs.len() != other.regs.len() {
            return Some("register shape differs".into());
        }
        for i in 0..self.regs.len() / self.reg_bytes.max(1) {
            if self.reg(i) != other.reg(i) {
                return Some(alloc::format!("vector register {i}: {:02x?}, expected {:02x?}", self.reg(i), other.reg(i)));
            }
        }
        for (i, (a, b)) in self.kmask.iter().zip(&other.kmask).enumerate() {
            if a != b {
                return Some(alloc::format!("k{i} {a:#x}, expected {b:#x}"));
            }
        }
        None
    }
}

/// Reads this CPU's user vector registers, MXCSR and x87 control word.
pub fn read_user_regs() -> RegDump {
    let (n, b, k) = reg_shape();
    let mut d = RegDump { mxcsr: 0, fcw: 0, reg_bytes: b, regs: vec![0u8; n * b], kmask: vec![0u64; k] };
    imp::store_regs(&mut d);
    d
}

/// Loads `d` into this CPU's user registers. Returns false (nothing loaded) if `d` has another shape.
pub fn load_user_regs(d: &RegDump) -> bool {
    let (n, b, k) = reg_shape();
    if d.reg_bytes != b || d.regs.len() != n * b || d.kmask.len() != k {
        return false;
    }
    imp::load_regs(d);
    true
}

#[cfg(target_os = "none")]
mod imp {
    use super::{Insn, Mode, RegDump};
    use core::arch::asm;

    pub fn save(insn: Insn, p: *mut u8, mask: u64) {
        let (lo, hi) = (mask as u32, (mask >> 32) as u32);
        // SAFETY (all four): `p` is a 64-byte aligned area of the size CPUID 0xD gives for its mode and `mask`
        // (= XCR0, set once per CPU before any area exists), held by the caller; the instruction writes only
        // that area. XSAVEC writes the same compacted image as XSAVES because IA32_XSS = 0 (`init_this_cpu`).
        // CPL 0 (XSAVES needs it), CR4.OSXSAVE on (insn != None), XSAVEC/XSAVES present (mode choice).
        match insn {
            Insn::None => {}
            Insn::Xsaves => unsafe { asm!("xsaves64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags)) },
            Insn::Xsavec => unsafe { asm!("xsavec64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags)) },
            Insn::Xsaveopt => unsafe { asm!("xsaveopt64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags)) },
            Insn::Xsave => unsafe { asm!("xsave64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags)) },
        }
    }

    pub fn restore(mode: Mode, p: *const u8, mask: u64) {
        let (lo, hi) = (mask as u32, (mask >> 32) as u32);
        // SAFETY (both): the area was written by `write_initial` (valid header: XSTATE_BV = x87|SSE, XCOMP_BV =
        // bit 63 | XCR0 for XSAVES and 0 otherwise, MXCSR reserved bits clear) or by the matching save
        // instruction, so XRSTOR(S) cannot #GP; it loads only user registers, which the kernel does not use.
        match mode {
            Mode::None => {}
            Mode::Xsaves => unsafe { asm!("xrstors64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags, readonly)) },
            Mode::Xsave | Mode::Xsaveopt => unsafe { asm!("xrstor64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags, readonly)) },
        }
    }

    unsafe extern "C" {
        fn hal_vregs_store_avx512(regs: *mut u8);
        fn hal_vregs_load_avx512(regs: *const u8);
        fn hal_kregs_store(k: *mut u64);
        fn hal_kregs_load(k: *const u64);
        fn hal_vregs_store_avx(regs: *mut u8);
        fn hal_vregs_load_avx(regs: *const u8);
        fn hal_vregs_store_sse(regs: *mut u8);
        fn hal_vregs_load_sse(regs: *const u8);
    }

    pub fn store_regs(d: &mut RegDump) {
        // SAFETY: `d` has the shape `reg_shape` reported (the callers build it so): `regs` holds every vector
        // register, `kmask` every mask register; the routines only store into them, and the instructions
        // exist because XCR0 enables their states. stmxcsr/fnstcw write 4 and 2 bytes into `d`.
        unsafe {
            match d.reg_bytes {
                64 => hal_vregs_store_avx512(d.regs.as_mut_ptr()),
                32 => hal_vregs_store_avx(d.regs.as_mut_ptr()),
                _ => hal_vregs_store_sse(d.regs.as_mut_ptr()),
            }
            if d.kmask.len() == 8 {
                hal_kregs_store(d.kmask.as_mut_ptr());
            }
            asm!("stmxcsr [{}]", "fnstcw [{}]", in(reg) &mut d.mxcsr, in(reg) &mut d.fcw, options(nostack, preserves_flags));
        }
    }

    pub fn load_regs(d: &RegDump) {
        // Only the defined MXCSR bits (the upper 16 are reserved and would #GP).
        let mxcsr = d.mxcsr & 0xFFFF;
        // SAFETY: as store_regs, reading instead of writing (`load_user_regs` checked the shape); ldmxcsr gets
        // reserved bits clear, fldcw accepts any value. Only user registers change; the kernel does not use them.
        unsafe {
            match d.reg_bytes {
                64 => hal_vregs_load_avx512(d.regs.as_ptr()),
                32 => hal_vregs_load_avx(d.regs.as_ptr()),
                _ => hal_vregs_load_sse(d.regs.as_ptr()),
            }
            if d.kmask.len() == 8 {
                hal_kregs_load(d.kmask.as_ptr());
            }
            asm!("ldmxcsr [{}]", "fldcw [{}]", in(reg) &mxcsr, in(reg) &d.fcw, options(nostack, preserves_flags, readonly));
        }
    }

    core::arch::global_asm!(
        r#"
        .text
        .global hal_vregs_store_avx512
    hal_vregs_store_avx512:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vmovdqu64 %zmm\n, \n*64(%rdi)
        .endr
        ret

        .global hal_vregs_load_avx512
    hal_vregs_load_avx512:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vmovdqu64 \n*64(%rdi), %zmm\n
        .endr
        ret

        .global hal_kregs_store
    hal_kregs_store:
        .irp n,0,1,2,3,4,5,6,7
        kmovq %k\n, \n*8(%rdi)
        .endr
        ret

        .global hal_kregs_load
    hal_kregs_load:
        .irp n,0,1,2,3,4,5,6,7
        kmovq \n*8(%rdi), %k\n
        .endr
        ret

        .global hal_vregs_store_avx
    hal_vregs_store_avx:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        vmovdqu %ymm\n, \n*32(%rdi)
        .endr
        ret

        .global hal_vregs_load_avx
    hal_vregs_load_avx:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        vmovdqu \n*32(%rdi), %ymm\n
        .endr
        ret

        .global hal_vregs_store_sse
    hal_vregs_store_sse:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        movdqu %xmm\n, \n*16(%rdi)
        .endr
        ret

        .global hal_vregs_load_sse
    hal_vregs_load_sse:
        .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
        movdqu \n*16(%rdi), %xmm\n
        .endr
        ret
    "#,
        options(att_syntax)
    );
}

#[cfg(not(target_os = "none"))]
mod imp {
    use super::{Insn, Mode, RegDump};
    pub fn save(_insn: Insn, _p: *mut u8, _mask: u64) {}
    pub fn restore(_mode: Mode, _p: *const u8, _mask: u64) {}
    pub fn store_regs(_d: &mut RegDump) {}
    pub fn load_regs(_d: &RegDump) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_choice() {
        assert_eq!(choose_mode(false, true, true, true), Mode::None);
        assert_eq!(choose_mode(true, true, true, true), Mode::Xsaves);
        assert_eq!(choose_mode(true, true, true, false), Mode::Xsaveopt, "XSAVES without its XSAVEC twin");
        assert_eq!(choose_mode(true, true, false, true), Mode::Xsaveopt);
        assert_eq!(choose_mode(true, false, false, false), Mode::Xsave);
    }

    #[test]
    fn save_instruction() {
        // The optimising instruction only for the area restored last; the twin has the same format.
        assert_eq!(save_insn(Mode::Xsaves, true), Insn::Xsaves);
        assert_eq!(save_insn(Mode::Xsaves, false), Insn::Xsavec);
        assert_eq!(save_insn(Mode::Xsaveopt, true), Insn::Xsaveopt);
        assert_eq!(save_insn(Mode::Xsaveopt, false), Insn::Xsave);
        assert_eq!(save_insn(Mode::Xsave, true), Insn::Xsave);
        assert_eq!(save_insn(Mode::None, true), Insn::None);
    }

    // CPUID 0xD sub-leaves (EAX size, EBX standard offset) for AVX and the AVX-512 states. AMD (Zen 4) packs
    // the offsets without MPX's gap, Intel keeps the gap at 960..1088.
    fn amd(i: u32) -> (u32, u32) {
        match i {
            2 => (256, 576),
            5 => (64, 832),
            6 => (512, 896),
            7 => (1024, 1408),
            _ => (0, 0),
        }
    }
    fn intel(i: u32) -> (u32, u32) {
        match i {
            2 => (256, 576),
            5 => (64, 1088),
            6 => (512, 1152),
            7 => (1024, 1664),
            _ => (0, 0),
        }
    }

    #[test]
    fn sizes_avx512() {
        let mask = 0xE7;
        // CP-1's Mini-PC expectation: 2432 = 0x980 bytes for XCR0 0xE7.
        assert_eq!(standard_size(mask, &amd), 2432);
        assert_eq!(standard_size(mask, &intel), 2688);
        // Compacted: 576 + 256 + 64 + 512 + 1024, no gaps, no alignment.
        assert_eq!(compacted_size(mask, &|i| (amd(i).0, false)), 2432);
        assert_eq!(compacted_size(mask, &|i| (intel(i).0, false)), 2432);
    }

    #[test]
    fn sizes_avx_only_and_alignment() {
        // QEMU on the Gaming-PC: XCR0 7, 832 bytes both ways (the boot log's xsave_bytes/xsaves_bytes).
        assert_eq!(compacted_size(0x7, &|i| (intel(i).0, false)), 832);
        assert_eq!(standard_size(0x7, &intel), 832);
        assert_eq!(standard_size(0x3, &|_| (0, 0)), LEGACY_AND_HEADER);
        // An aligned component after an odd size starts at the next 64-byte boundary.
        let c = |i: u32| match i {
            2 => (8, false),
            3 => (16, true),
            _ => (0, false),
        };
        assert_eq!(compacted_size(0b1111, &c), 576 + 8 + 56 + 16);
    }

    #[test]
    fn shapes() {
        assert_eq!(shape_for(0x3), (16, 16, 0));
        assert_eq!(shape_for(0x7), (16, 32, 0));
        assert_eq!(shape_for(0xE7), (32, 64, 8));
        // Partial AVX-512 is not AVX-512 (XSETBV refuses it anyway).
        assert_eq!(shape_for(0x27), (16, 32, 0));
    }

    #[test]
    fn patterns_differ_and_keep_mxcsr_valid() {
        for shape in [(16, 32, 0), (32, 64, 8)] {
            let a = RegDump::pattern_for(shape, 1);
            let b = RegDump::pattern_for(shape, 2);
            assert_ne!(a.regs, b.regs);
            assert_eq!(a.kmask.len(), shape.2);
            assert!(a.diff(&b).is_some() && a.diff(&a.clone()).is_none());
            assert_eq!(a.mxcsr & 0xFFFF_0000, 0);
            assert_eq!(a.mxcsr & 0x1F80, 0x1F80, "every SIMD exception stays masked");
            assert_eq!(a.fcw & 0x3F, 0x3F, "every x87 exception stays masked");
            assert!(a.regs.chunks(8).all(|c| c.iter().any(|&x| x != 0)));
        }
    }
}
