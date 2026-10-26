//! Linux-compatible signal frames on the user stack (owner: AB, item AB-6).
//!
//! The x86-64 `struct rt_sigframe` (linux-6.6/arch/x86/include/asm/sigframe.h:59; `ucontext` and `sigcontext`
//! from include/uapi/asm-generic/ucontext.h and arch/x86/include/uapi/asm/sigcontext.h) as bytes, and the user
//! FPU image Linux puts below it (arch/x86/kernel/fpu/signal.c): the XSAVE standard format of every user state
//! XCR0 enables, as large as the CPUID 0xD sub-leaf offsets give (SC-1's `xsave::layout` mask), with Linux's
//! software bytes in the legacy area (`_fpx_sw_bytes` at 464) and FP_XSTATE_MAGIC2 after it.
//!
//! The image a program hands back to rt_sigreturn is never trusted: it is copied into a kernel buffer first
//! and checked there for everything XRSTOR would fault on (XSTATE_BV outside XCR0, a compacted or non-zero
//! XCOMP_BV, reserved header bytes, MXCSR bits outside MXCSR_MASK, a misaligned address), so the restore
//! cannot fault in ring 0 and nothing but the calling thread's own user registers changes. A failed check is a
//! bad frame (SIGSEGV), as Linux's faulting XRSTOR is.
//!
//! Live registers: SC switches FPU state eagerly, so between two switches the vector registers are the running
//! thread's; the save and restore here act on the calling thread only and never touch a saved `FpuState` area.
//! SC's modified-optimisation invariant (`xsave.rs`) therefore holds: an XRSTOR from a buffer here makes that
//! buffer's address the last restore, and the next XSAVES of the thread's own area (another address) saves in
//! full.

use crate::cpu;
use crate::entry::GeneralRegs;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

/// `sizeof(struct rt_sigframe)`: pretcode 8 + ucontext 304 + siginfo 128.
pub const FRAME_SIZE: u64 = 440;
/// The bytes `encode` writes and `decode` reads: pretcode up to and including uc_sigmask.
pub const HEAD_SIZE: usize = 312;
/// Offset of the siginfo in the frame (written only with SA_SIGINFO, as Linux).
pub const INFO_OFFSET: u64 = 312;
/// Offset of the ucontext in the frame (the handler's third argument).
pub const UC_OFFSET: u64 = 8;
/// The x86-64 ABI red zone below the interrupted stack pointer.
pub const REDZONE: u64 = 128;

pub const UC_FP_XSTATE: u64 = 1;
pub const UC_SIGCONTEXT_SS: u64 = 2;
pub const UC_STRICT_RESTORE_SS: u64 = 4;

/// RFLAGS bits sigreturn takes from the frame (`FIX_EFLAGS`, signal_64.c: AC OF DF TF SF ZF AF PF CF RF);
/// everything else keeps its current value (IOPL, IF, VM, NT, ID never come from a program).
pub const FIX_EFLAGS: u64 = 0x0005_0DD5;

/// Linux's own user selectors: a program that writes them into a context it builds means 64-bit user mode.
pub const LINUX_USER_CS: u16 = 0x33;
pub const LINUX_USER_DS: u16 = 0x2B;

pub const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
pub const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;
/// The software bytes' offset in an image (48 bytes; the caller reads them first).
pub const SW_BYTES_AT: usize = 464;
/// Where XSTATE_BV sits in an image.
pub const XSTATE_BV_AT: usize = LEGACY_SIZE;
const LEGACY_SIZE: usize = 512;
const HEADER_END: usize = 576;
const XSTATE_FPSSE: u64 = cpu::XSTATE_X87 | cpu::XSTATE_SSE;

// ucontext / sigcontext offsets in the frame.
const O_UC_FLAGS: usize = 8;
const O_UC_LINK: usize = 16;
const O_UC_STACK: usize = 24;
const O_MCTX: usize = 48;
const O_SIGMASK: usize = 304;

/// `struct sigcontext` (uc_mcontext) as V2 fills and reads it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mcontext {
    pub regs: GeneralRegs,
    pub cs: u16,
    pub ss: u16,
    pub err: u64,
    pub trapno: u64,
    pub oldmask: u64,
    pub cr2: u64,
    pub fpstate: u64,
}

/// The frame's head: everything up to uc_sigmask.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameHead {
    pub restorer: u64,
    pub uc_flags: u64,
    /// uc_stack as Linux saves it (`__save_altstack`: ss_sp, the stored ss_flags, ss_size).
    pub stack: [u8; 24],
    pub mctx: Mcontext,
    pub sigmask: u64,
}

/// sigcontext's order of the general registers (r8..r15, rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip, eflags).
fn reg_slots(r: &GeneralRegs) -> [u64; 18] {
    [r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15, r.rdi, r.rsi, r.rbp, r.rbx, r.rdx, r.rax, r.rcx, r.rsp, r.rip, r.rflags]
}

fn regs_from(s: &[u64; 18]) -> GeneralRegs {
    GeneralRegs {
        r8: s[0],
        r9: s[1],
        r10: s[2],
        r11: s[3],
        r12: s[4],
        r13: s[5],
        r14: s[6],
        r15: s[7],
        rdi: s[8],
        rsi: s[9],
        rbp: s[10],
        rbx: s[11],
        rdx: s[12],
        rax: s[13],
        rcx: s[14],
        rsp: s[15],
        rip: s[16],
        rflags: s[17],
    }
}

fn put(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn get(b: &[u8], off: usize) -> u64 {
    b.get(off..off + 8).and_then(|s| s.try_into().ok()).map_or(0, u64::from_le_bytes)
}
fn put16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn get16(b: &[u8], off: usize) -> u16 {
    b.get(off..off + 2).and_then(|s| s.try_into().ok()).map_or(0, u16::from_le_bytes)
}
fn get32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).and_then(|s| s.try_into().ok()).map_or(0, u32::from_le_bytes)
}

/// The frame head as bytes (`x64_setup_rt_frame`): gs and fs are 0, reserved1 stays zero.
pub fn encode(h: &FrameHead) -> [u8; HEAD_SIZE] {
    let mut b = [0u8; HEAD_SIZE];
    put(&mut b, 0, h.restorer);
    put(&mut b, O_UC_FLAGS, h.uc_flags);
    put(&mut b, O_UC_LINK, 0);
    b[O_UC_STACK..O_UC_STACK + 24].copy_from_slice(&h.stack);
    let m = &h.mctx;
    for (i, v) in reg_slots(&m.regs).into_iter().enumerate() {
        put(&mut b, O_MCTX + i * 8, v);
    }
    put16(&mut b, O_MCTX + 144, m.cs);
    put16(&mut b, O_MCTX + 150, m.ss);
    put(&mut b, O_MCTX + 152, m.err);
    put(&mut b, O_MCTX + 160, m.trapno);
    put(&mut b, O_MCTX + 168, m.oldmask);
    put(&mut b, O_MCTX + 176, m.cr2);
    put(&mut b, O_MCTX + 184, m.fpstate);
    put(&mut b, O_SIGMASK, h.sigmask);
    b
}

/// The head of a frame a program hands to rt_sigreturn.
pub fn decode(b: &[u8; HEAD_SIZE]) -> FrameHead {
    let mut s = [0u64; 18];
    for (i, v) in s.iter_mut().enumerate() {
        *v = get(b, O_MCTX + i * 8);
    }
    let mut stack = [0u8; 24];
    stack.copy_from_slice(&b[O_UC_STACK..O_UC_STACK + 24]);
    FrameHead {
        restorer: get(b, 0),
        uc_flags: get(b, O_UC_FLAGS),
        stack,
        mctx: Mcontext {
            regs: regs_from(&s),
            cs: get16(b, O_MCTX + 144),
            ss: get16(b, O_MCTX + 150),
            err: get(b, O_MCTX + 152),
            trapno: get(b, O_MCTX + 160),
            oldmask: get(b, O_MCTX + 168),
            cr2: get(b, O_MCTX + 176),
            fpstate: get(b, O_MCTX + 184),
        },
        sigmask: get(b, O_SIGMASK),
    }
}

/// RFLAGS after sigreturn (`restore_sigcontext`).
pub fn restored_rflags(current: u64, frame: u64) -> u64 {
    (current & !FIX_EFLAGS) | (frame & FIX_EFLAGS)
}

/// Whether a frame's selectors name 64-bit user mode (V2's own or Linux's numbers): anything else faults at
/// the return on Linux (or runs 32-bit code, which V2 does not have), so sigreturn treats it as a bad frame.
/// SS is looked at only with UC_STRICT_RESTORE_SS (otherwise Linux forces it valid, `force_valid_ss`).
pub fn selectors_ok(cs: u16, ss: u16, uc_flags: u64) -> bool {
    let cs = cs | 3;
    let ss = ss | 3;
    let cs_ok = cs == crate::gdt::USER_CS || cs == LINUX_USER_CS;
    let ss_ok = uc_flags & UC_STRICT_RESTORE_SS == 0 || ss == crate::gdt::USER_SS || ss == LINUX_USER_DS;
    cs_ok && ss_ok
}

/// A thread's alternate stack as `place` needs it (`sas_ss_sp`, `sas_ss_size`, the stored `sas_ss_flags`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Alt {
    pub sp: u64,
    pub size: u64,
    pub autodisarm: bool,
}

impl Alt {
    /// `__on_sig_stack`.
    fn contains(&self, sp: u64) -> bool {
        sp > self.sp && sp - self.sp <= self.size
    }
    /// `on_sig_stack`.
    fn on(&self, sp: u64) -> bool {
        !self.autodisarm && self.contains(sp)
    }
}

/// `get_sigframe` (arch/x86/kernel/signal.c:57), pure: `sp` = the interrupted stack pointer, `onstack` = the
/// action has SA_ONSTACK, `alt` = the thread's alternate stack, `fpu_bytes` = the FPU image with MAGIC2.
/// Returns (frame, fpstate), or None when the frame would overflow an alternate stack (Linux answers an
/// always-bogus address: the frame write fails, SIGSEGV).
pub fn place(sp: u64, onstack: bool, alt: Alt, fpu_bytes: u64) -> Option<(u64, u64)> {
    let nested = alt.on(sp);
    let mut s = sp.wrapping_sub(REDZONE);
    let mut entering = false;
    if onstack && alt.size != 0 && !alt.on(s) {
        s = alt.sp.wrapping_add(alt.size);
        entering = true;
    }
    let fp = s.wrapping_sub(fpu_bytes) & !63;
    let frame = (fp.wrapping_sub(FRAME_SIZE) & !15).wrapping_sub(8);
    if (nested || entering) && !alt.contains(frame) {
        return None;
    }
    Some((frame, fp))
}

// ---------------------------------------------------------------------------------------------------------
// The FPU image

/// What the FPU part of a frame looks like on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpuShape {
    /// The user states (XCR0; Linux `fpstate->user_xfeatures`).
    pub xfeatures: u64,
    /// The standard-format size for them (`fpstate->user_size`: the end of the last state, CPUID 0xD).
    pub user_size: u32,
}

impl FpuShape {
    /// Bytes below the frame (`xstate_sigframe_size`): the image and MAGIC2.
    pub fn frame_bytes(&self) -> u64 {
        self.user_size as u64 + 4
    }
}

static USER_SIZE: AtomicU32 = AtomicU32::new(0);
static MXCSR_MASK: AtomicU32 = AtomicU32::new(0);

/// End of the state components of `mask` in the standard format (CPUID 0xD.i: EAX size, EBX offset), at
/// least the legacy area and the header.
fn standard_end(mask: u64) -> usize {
    let v = cpu::live();
    crate::xsave::standard_size(mask, &|i| {
        let r = v.get(0xD, i);
        (r.eax, r.ebx)
    }) as usize
}

/// The shape for the published XCR0 (computed once from CPUID 0xD).
pub fn shape() -> FpuShape {
    let l = crate::xsave::layout();
    let mut size = USER_SIZE.load(Ordering::Relaxed);
    if size == 0 {
        size = standard_end(l.mask) as u32;
        USER_SIZE.store(size, Ordering::Relaxed);
    }
    FpuShape { xfeatures: l.mask, user_size: size }
}

/// A 64-byte aligned, zeroed scratch area of `len` bytes inside a plain Vec (no unsafe allocation).
struct Aligned {
    v: Vec<u8>,
    off: usize,
    len: usize,
}

impl Aligned {
    fn new(len: usize) -> Option<Aligned> {
        let mut v = Vec::new();
        v.try_reserve_exact(len + 64).ok()?;
        v.resize(len + 64, 0);
        let off = (64 - (v.as_ptr() as usize & 63)) & 63;
        Some(Aligned { v, off, len })
    }
    fn bytes(&self) -> &[u8] {
        &self.v[self.off..self.off + self.len]
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        let (o, l) = (self.off, self.len);
        &mut self.v[o..o + l]
    }
}

/// The MXCSR bits a restore may set (`mxcsr_feature_mask`: MXCSR_MASK of an FXSAVE image, 0xFFBF when it is
/// 0), read once from a save of this CPU.
pub fn mxcsr_mask() -> u32 {
    let m = MXCSR_MASK.load(Ordering::Relaxed);
    if m != 0 {
        return m;
    }
    let mut m = 0xFFBF;
    if let Some(mut a) = Aligned::new(shape().user_size as usize) {
        imp::xsave(a.bytes_mut().as_mut_ptr(), XSTATE_FPSSE);
        let v = get32(a.bytes(), 28);
        if v != 0 {
            m = v;
        }
    }
    MXCSR_MASK.store(m, Ordering::Relaxed);
    m
}

/// The calling thread's user FPU state as Linux writes it into a frame (`copy_fpstate_to_sigframe`): the XSAVE
/// image (header cleared first, XSTATE_BV with x87 and SSE always set), the software bytes, MAGIC2 after it;
/// `frame_bytes()` long. None when the heap has no room (the frame then fails: SIGSEGV).
pub fn save_user_fpu() -> Option<Vec<u8>> {
    let s = shape();
    let n = s.user_size as usize;
    let mut a = Aligned::new(n)?;
    imp::xsave(a.bytes_mut().as_mut_ptr(), s.xfeatures);
    let mut img = Vec::new();
    img.try_reserve_exact(n + 4).ok()?;
    img.extend_from_slice(a.bytes());
    finish_image(&mut img, s);
    Some(img)
}

/// `save_xstate_epilog` on an XSAVE image of `s.user_size` bytes: software bytes, XSTATE_BV |= x87|SSE,
/// MAGIC2 appended. Pure.
pub fn finish_image(img: &mut Vec<u8>, s: FpuShape) {
    let n = s.user_size as usize;
    img.resize(n.max(HEADER_END), 0);
    let sw = &mut img[SW_BYTES_AT..SW_BYTES_AT + 48];
    sw.fill(0);
    sw[0..4].copy_from_slice(&FP_XSTATE_MAGIC1.to_le_bytes());
    sw[4..8].copy_from_slice(&(s.user_size + 4).to_le_bytes());
    sw[8..16].copy_from_slice(&s.xfeatures.to_le_bytes());
    sw[16..20].copy_from_slice(&s.user_size.to_le_bytes());
    let bv = get(img, LEGACY_SIZE) | XSTATE_FPSSE;
    put(img, LEGACY_SIZE, bv);
    img.extend_from_slice(&FP_XSTATE_MAGIC2.to_le_bytes());
}

/// The clean state a handler starts with (`fpu__clear_user_states`): FCW 0x37F, MXCSR 0x1F80, every
/// register zero.
pub fn init_user_fpu() {
    let s = shape();
    if let Some(mut a) = Aligned::new(s.user_size as usize) {
        init_image(a.bytes_mut());
        imp::xrstor(a.bytes().as_ptr(), s.xfeatures);
    }
}

/// An image whose XRSTOR gives Linux's initial user FPU state (XSTATE_BV 0: every state initialised; MXCSR
/// is loaded from the image). Pure.
pub fn init_image(b: &mut [u8]) {
    b.fill(0);
    b[0..2].copy_from_slice(&0x037Fu16.to_le_bytes());
    b[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
}

/// What `check_xstate_in_sigframe` decided from the software bytes (and MAGIC2, which the caller reads at
/// `magic2_at` when that is Some).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestorePlan {
    /// Only the legacy area counts (FXRSTOR semantics; every other state to its initial configuration).
    pub fx_only: bool,
    /// The states to take from the image (`fx_sw_user.xfeatures`).
    pub xfeatures: u64,
    /// Where MAGIC2 must be (offset from the image's start).
    pub magic2_at: Option<usize>,
}

const FX_ONLY: RestorePlan = RestorePlan { fx_only: true, xfeatures: XSTATE_FPSSE, magic2_at: None };

/// The software bytes' verdict (pure).
pub fn plan_from_sw(sw: &[u8; 48], s: FpuShape) -> RestorePlan {
    let magic1 = get32(sw, 0);
    let extended = get32(sw, 4);
    let xfeatures = get(sw, 8);
    let xsize = get32(sw, 16);
    if magic1 != FP_XSTATE_MAGIC1 || (xsize as usize) < HEADER_END || xsize > s.user_size || xsize > extended {
        return FX_ONLY;
    }
    RestorePlan { fx_only: false, xfeatures, magic2_at: Some(xsize as usize) }
}

/// The plan after MAGIC2 was read (a wrong one: only the legacy area counts).
pub fn plan_with_magic2(p: RestorePlan, magic2: u32) -> RestorePlan {
    if p.fx_only || magic2 == FP_XSTATE_MAGIC2 { p } else { FX_ONLY }
}

/// How many bytes of the program's image the restore reads (XRSTOR reads only the states it loads; FXRSTOR
/// the legacy area): the caller copies exactly these, so a short image that Linux accepts is accepted too.
pub fn bytes_needed(p: RestorePlan, xstate_bv: u64, s: FpuShape) -> usize {
    if p.fx_only { LEGACY_SIZE } else { standard_end(xstate_bv & p.xfeatures & s.xfeatures).max(HEADER_END) }
}

/// Why a program's FPU image was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadFpu {
    Misaligned,
    TooShort,
    /// XSTATE_BV names a state XCR0 does not enable.
    StateBits,
    /// XCOMP_BV is not 0 (V2 accepts only the standard format a frame carries).
    Compacted,
    /// Header bytes 16..63 are not zero.
    ReservedHeader,
    /// MXCSR has bits outside MXCSR_MASK.
    Mxcsr,
    NoMemory,
}

impl BadFpu {
    pub fn text(self) -> &'static str {
        match self {
            BadFpu::Misaligned => "fpstate not aligned",
            BadFpu::TooShort => "fpstate image too short",
            BadFpu::StateBits => "XSTATE_BV outside XCR0",
            BadFpu::Compacted => "XCOMP_BV not zero",
            BadFpu::ReservedHeader => "reserved XSAVE header bytes set",
            BadFpu::Mxcsr => "reserved MXCSR bits set",
            BadFpu::NoMemory => "no memory for the image",
        }
    }
}

/// Checks a program's image against everything XRSTOR/FXRSTOR would fault on and builds the area the restore
/// loads (pure): with `fx_only` the legacy area and XSTATE_BV = x87|SSE; otherwise the image's states that the
/// plan names, every other state in its initial configuration; MXCSR 0x1F80 when neither SSE nor AVX is taken
/// (Linux's second XRSTOR from `init_fpstate`). `addr` = the image's user address (alignment).
pub fn check_image(img: &[u8], addr: u64, p: RestorePlan, s: FpuShape, mxcsr_mask: u32) -> Result<Vec<u8>, BadFpu> {
    let align = if p.fx_only { 16 } else { 64 };
    if addr % align != 0 {
        return Err(BadFpu::Misaligned);
    }
    if img.len() < if p.fx_only { LEGACY_SIZE } else { HEADER_END } {
        return Err(BadFpu::TooShort);
    }
    let size = (s.user_size as usize).max(HEADER_END);
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| BadFpu::NoMemory)?;
    out.resize(size, 0);
    let take = if p.fx_only { LEGACY_SIZE } else { img.len().min(size) };
    out[..take].copy_from_slice(&img[..take]);
    let bv = if p.fx_only {
        XSTATE_FPSSE
    } else {
        let bv = get(img, LEGACY_SIZE);
        if bv & !s.xfeatures != 0 {
            return Err(BadFpu::StateBits);
        }
        if get(img, LEGACY_SIZE + 8) != 0 {
            return Err(BadFpu::Compacted);
        }
        if img[LEGACY_SIZE + 16..HEADER_END].iter().any(|&x| x != 0) {
            return Err(BadFpu::ReservedHeader);
        }
        bv & p.xfeatures
    };
    if p.fx_only || p.xfeatures & (cpu::XSTATE_SSE | cpu::XSTATE_AVX) != 0 {
        if get32(img, 24) & !mxcsr_mask != 0 {
            return Err(BadFpu::Mxcsr);
        }
    } else {
        out[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
    }
    out[LEGACY_SIZE..HEADER_END].fill(0);
    put(&mut out, LEGACY_SIZE, bv);
    Ok(out)
}

/// Loads a checked area (`check_image`'s result) into the calling thread's user FPU registers; the checks
/// are repeated on the kernel copy that is loaded (nothing else can reach it).
pub fn restore_user_fpu(area: &[u8]) -> Result<(), BadFpu> {
    let s = shape();
    let n = s.user_size as usize;
    if area.len() < n || area.len() < HEADER_END {
        return Err(BadFpu::TooShort);
    }
    let mut a = Aligned::new(n).ok_or(BadFpu::NoMemory)?;
    a.bytes_mut().copy_from_slice(&area[..n]);
    let b = a.bytes();
    if get(b, LEGACY_SIZE) & !s.xfeatures != 0 || get(b, LEGACY_SIZE + 8) != 0 || b[LEGACY_SIZE + 16..HEADER_END].iter().any(|&x| x != 0) {
        return Err(BadFpu::ReservedHeader);
    }
    if get32(b, 24) & !mxcsr_mask() != 0 {
        return Err(BadFpu::Mxcsr);
    }
    imp::xrstor(b.as_ptr(), s.xfeatures);
    Ok(())
}

/// `fpu__exception_code` (fpu/core.c) for #MF (16) and #XM (19) from the calling thread's live state: the
/// SIGFPE si_code, 0 for a spurious trap (no signal).
pub fn fpu_exception_code(vector: u8) -> i32 {
    let Some(mut a) = Aligned::new(shape().user_size as usize) else { return 0 };
    imp::xsave(a.bytes_mut().as_mut_ptr(), XSTATE_FPSSE);
    exception_code(vector, a.bytes())
}

/// Pure part of `fpu_exception_code` on a legacy area (cwd at 0, swd at 2, MXCSR at 24).
pub fn exception_code(vector: u8, legacy: &[u8]) -> i32 {
    // SIGFPE si_codes (include/uapi/asm-generic/siginfo.h; hal does not see uapi).
    const FPE_FLTDIV: i32 = 3;
    const FPE_FLTOVF: i32 = 4;
    const FPE_FLTUND: i32 = 5;
    const FPE_FLTRES: i32 = 6;
    const FPE_FLTINV: i32 = 7;
    let cwd = get16(legacy, 0) as u32;
    let swd = get16(legacy, 2) as u32;
    let mxcsr = get32(legacy, 24);
    let err = if vector == 16 { swd & !cwd } else { !(mxcsr >> 7) & mxcsr };
    if err & 0x001 != 0 {
        FPE_FLTINV
    } else if err & 0x004 != 0 {
        FPE_FLTDIV
    } else if err & 0x008 != 0 {
        FPE_FLTOVF
    } else if err & 0x012 != 0 {
        FPE_FLTUND
    } else if err & 0x020 != 0 {
        FPE_FLTRES
    } else {
        0
    }
}

#[cfg(target_os = "none")]
mod imp {
    use core::arch::asm;

    pub fn xsave(p: *mut u8, mask: u64) {
        let (lo, hi) = (mask as u32, (mask >> 32) as u32);
        // SAFETY: `p` is a 64-byte aligned buffer of `shape().user_size` bytes (the standard-format size CPUID
        // 0xD gives for XCR0, which bounds every component `mask` can name), owned by the caller; XSAVE writes
        // only there. CR4.OSXSAVE is on (R10: the kernel refuses CPUs without XSAVE). It only reads the live
        // user registers.
        unsafe { asm!("xsave64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags)) }
    }

    pub fn xrstor(p: *const u8, mask: u64) {
        let (lo, hi) = (mask as u32, (mask >> 32) as u32);
        // SAFETY: `p` is a 64-byte aligned kernel buffer of `shape().user_size` bytes whose header is the
        // standard format with XSTATE_BV inside XCR0, XCOMP_BV 0 and reserved bytes zero, and whose MXCSR has
        // no bit outside MXCSR_MASK (`restore_user_fpu` checked this very copy, which no program can reach;
        // `init_user_fpu` builds it so): XRSTOR cannot #GP. It loads only the calling thread's user registers,
        // which the kernel does not use (R10).
        unsafe { asm!("xrstor64 [{}]", in(reg) p, in("eax") lo, in("edx") hi, options(nostack, preserves_flags, readonly)) }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn xsave(_p: *mut u8, _mask: u64) {}
    pub fn xrstor(_p: *const u8, _mask: u64) {}
}

#[cfg(test)]
mod tests;
