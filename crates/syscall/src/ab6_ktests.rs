//! AB-6 ktests (Safe, rerunnable): signal delivery with the real frame and a round trip through
//! rt_sigreturn, default actions, and a hostile-sigreturn test that forges frames and checks a program can
//! never escalate (a bad frame ends it with SIGSEGV, never a kernel fault; an accepted one leaves no
//! privileged flag or MXCSR bit). Kill rules, SIGCHLD, kill_pgrp and timers: `ab6_more_ktests.rs`.
//!
//! Each test plays a held process itself (as the wave-A and AB-5 tests do): it sets the registers, calls the
//! dispatcher or `proc::deliver` directly, and reads the process's memory back. No real user code runs, so the
//! tests are deterministic and safe on the Mini-PC (pinned to CPU 0, rerunnable).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::entry::UserContext;
use hal::signal_frame as sf;
use ktest::{TestResult, kassert, ktest};
use proc::signal::{SigInfo, To};
use proc::{ExitStatus, Process};
use uapi::nr;

const SEC: u64 = 1_000_000_000;
const EXIT0: [u8; 9] = [0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05];
const SCRATCH: u64 = 1 << 16;
const KERNEL_ADDR: u64 = 0xffff_8880_0000_0000;
/// A plausible libc restorer address (its bytes do not matter: the kernel never reads them).
const RESTORER: u64 = 0x40_2000;
const HANDLER: u64 = 0x40_3000;

pub(crate) struct T {
    pub(crate) p: Arc<Process>,
    pub(crate) th: Arc<proc::Thread>,
    pub(crate) ctx: UserContext,
    pub(crate) mem: u64,
    _pin: proc::MmPin,
}

impl Drop for T {
    fn drop(&mut self) {
        self.p.kill(9);
        let _ = self.p.wait(Some(10 * SEC));
    }
}

impl T {
    pub(crate) fn new(name: &str) -> Result<T, String> {
        T::with(name, 1000, false)
    }
    /// A held process with uid `uid` (and pid 1 with `pid1`: Err(EBUSY) text when pid 1 is taken).
    pub(crate) fn with(name: &str, uid: u32, pid1: bool) -> Result<T, String> {
        T::spawn(name, uid, pid1, None)
    }
    /// A held child of `parent` (uid 1000).
    pub(crate) fn child_of(name: &str, parent: &Arc<Process>) -> Result<T, String> {
        T::spawn(name, 1000, false, Some(parent))
    }
    fn spawn(name: &str, uid: u32, pid1: bool, parent: Option<&Arc<Process>>) -> Result<T, String> {
        let image = crate::ktests::elf(&EXIT0);
        let argv = [name];
        let mut o = proc::SpawnOptions::new(&argv, &[], policy::ProgramId(1));
        o.held = true;
        o.parent = parent;
        o.uid = uid;
        o.pid1 = pid1;
        let p = proc::spawn_image_with(name, &image, &o).map_err(|e| format!("spawn {name}: {e}"))?;
        let mem = p.mm.sys_mmap(0, SCRATCH, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let th = p.threads().into_iter().next().ok_or("no main thread")?;
        let _pin = proc::pin_mm(&p);
        // A user stack well inside mapped scratch memory so frames land in writable pages.
        Ok(T { p, th, ctx: UserContext::new(0x40_0078, mem + SCRATCH - 4096), mem, _pin })
    }
    pub(crate) fn call(&mut self, n: usize, args: &[u64]) -> i64 {
        let mut a = [0u64; 6];
        a[..args.len()].copy_from_slice(args);
        let r = &mut self.ctx.regs;
        (r.rax, r.rdi, r.rsi, r.rdx, r.r10, r.r8, r.r9) = (n as u64, a[0], a[1], a[2], a[3], a[4], a[5]);
        crate::dispatch(&self.p, &self.th, &mut self.ctx);
        self.ctx.regs.rax as i64
    }
    pub(crate) fn a(&self, at: u64) -> u64 {
        self.mem + at
    }
    pub(crate) fn put(&self, at: u64, b: &[u8]) {
        let _ = self.p.mm.copy_to_user(self.mem + at, b);
    }
    pub(crate) fn get(&self, at: u64, len: usize) -> Vec<u8> {
        let mut v = alloc::vec![0u8; len];
        let _ = self.p.mm.copy_from_user(&mut v, self.mem + at);
        v
    }
    /// rt_sigaction with a handler and SA_RESTORER (so a frame can be built).
    pub(crate) fn install(&mut self, sig: u32, handler: u64, flags: u64, mask: u64) -> TestResult {
        let act = proc::signal::SigAction { handler, flags: flags | uapi::ab::sigabi::SA_RESTORER, restorer: RESTORER, mask };
        self.put(0, &act.to_bytes());
        let r = self.call(nr::RT_SIGACTION, &[sig as u64, self.a(0), 0, 8]);
        kassert!(r == 0, "rt_sigaction({sig}) = {r}");
        Ok(())
    }
    pub(crate) fn alive(&self) -> bool {
        !self.p.has_exited()
    }
}

// ---------------------------------------------------------------------------------------------------------
// delivery and a round trip

ktest!("ab.signal.deliver", Safe, deliver);
fn deliver() -> TestResult {
    let mut t = T::new("ab6-deliver")?;
    t.install(uapi::ab::signal::SIGUSR1, HANDLER, uapi::ab::sigabi::SA_SIGINFO, 1 << 11)?;
    // Mark the registers so the frame's saved state can be checked.
    t.ctx.regs.rbx = 0xB0B;
    t.ctx.regs.r12 = 0x12;
    let saved_rsp = t.ctx.regs.rsp;
    let saved_rip = t.ctx.regs.rip;
    // kill(self, SIGUSR1): pending. Then the way back to user mode builds the frame.
    kassert!(t.p.send_info(SigInfo::kernel(uapi::ab::signal::SIGUSR1), To::Group, false).is_ok(), "send");
    proc::deliver::exit_to_user(&t.th, &mut t.ctx, None);
    kassert!(t.alive(), "delivery ended the process");
    kassert!(t.ctx.regs.rip == HANDLER, "rip is not the handler: {:#x}", t.ctx.regs.rip);
    kassert!(t.ctx.regs.rdi == uapi::ab::signal::SIGUSR1 as u64, "rdi (signo) {:#x}", t.ctx.regs.rdi);
    let frame = t.ctx.regs.rsp;
    kassert!(frame < saved_rsp && frame >= t.mem, "frame {:#x} not below the old rsp {:#x}", frame, saved_rsp);
    kassert!(t.ctx.regs.rsi == frame + sf::INFO_OFFSET, "rsi (siginfo) {:#x}", t.ctx.regs.rsi);
    kassert!(t.ctx.regs.rdx == frame + sf::UC_OFFSET, "rdx (ucontext) {:#x}", t.ctx.regs.rdx);
    // The restorer was written at the frame's start (pretcode).
    let pretcode = u64::from_le_bytes(read8(&t, frame));
    kassert!(pretcode == RESTORER, "pretcode {:#x}", pretcode);
    // The mask in the handler is old | sa_mask | the signal.
    // old(0) | sa_mask(bit index 11) | the signal itself (SIGUSR1 = bit index 9).
    let blocked = t.th.blocked();
    kassert!(blocked == (1u64 << 11) | (1u64 << 9), "blocked in the handler {:#x}", blocked);
    // rt_sigreturn restores everything.
    t.ctx.regs.rsp = frame.wrapping_add(8);
    let r = t.call(nr::RT_SIGRETURN, &[]);
    kassert!(r == 0, "rt_sigreturn answered {r} (the saved rax)");
    kassert!(t.ctx.regs.rip == saved_rip, "rip not restored: {:#x} want {:#x}", t.ctx.regs.rip, saved_rip);
    kassert!(t.ctx.regs.rsp == saved_rsp, "rsp not restored: {:#x}", t.ctx.regs.rsp);
    kassert!(t.ctx.regs.rbx == 0xB0B && t.ctx.regs.r12 == 0x12, "callee-saved registers not restored");
    kassert!(t.th.blocked() == 0, "the mask was not restored by sigreturn: {:#x}", t.th.blocked());
    Ok(())
}

fn read8(t: &T, at_user: u64) -> [u8; 8] {
    let mut b = [0u8; 8];
    let _ = t.p.mm.copy_from_user(&mut b, at_user);
    b
}

ktest!("ab.signal.default_actions", Safe, default_actions);
fn default_actions() -> TestResult {
    // A default-terminate signal ends the process with its number.
    {
        let mut t = T::new("ab6-term")?;
        kassert!(t.call(nr::KILL, &[t.p.pid as u64, 15]) == 0, "kill");
        kassert!(t.p.status() == Some(ExitStatus::Signaled(15)), "status {:?}", t.p.status());
    }
    // A default-ignore signal is dropped; the program runs on.
    {
        let mut t = T::new("ab6-ign")?;
        kassert!(t.call(nr::KILL, &[t.p.pid as u64, 28]) == 0, "SIGWINCH");
        kassert!(t.alive(), "SIGWINCH ended the process");
    }
    // A core-dump signal terminates (no core file; wait status without 0x80).
    {
        let mut t = T::new("ab6-core")?;
        kassert!(t.call(nr::KILL, &[t.p.pid as u64, 6]) == 0, "SIGABRT");
        kassert!(t.p.status() == Some(ExitStatus::Signaled(6)), "status {:?}", t.p.status());
        kassert!(t.p.status().unwrap().wait_status() & 0x80 == 0, "the core bit must be clear");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// the hostile sigreturn test (brief requirement)

ktest!("ab.signal.hostile_sigreturn", Safe, hostile_sigreturn);
fn hostile_sigreturn() -> TestResult {
    // Build one good frame from a real delivery, then hand back many broken copies of it. None may fault the
    // kernel: each either restores safely (the benign edits) or ends the program with SIGSEGV.
    let faults0 = proc::deliver::bad_frame_count();
    for case in Case::ALL {
        let mut t = T::new("ab6-hostile")?;
        t.install(uapi::ab::signal::SIGUSR1, HANDLER, uapi::ab::sigabi::SA_SIGINFO, 0)?;
        kassert!(t.p.send_info(SigInfo::kernel(uapi::ab::signal::SIGUSR1), To::Group, false).is_ok(), "send");
        proc::deliver::exit_to_user(&t.th, &mut t.ctx, None);
        kassert!(t.ctx.regs.rip == HANDLER, "{case:?}: the frame was not built");
        let frame = t.ctx.regs.rsp;
        let fpstate = frame_fpstate(&t, frame);
        case.apply(&mut t, frame, fpstate);
        t.ctx.regs.rsp = case.rsp(&t, frame);
        let alt_before = t.th.alt_stack();
        let r = t.call(nr::RT_SIGRETURN, &[]);
        match case.expect() {
            Expect::Segv => {
                kassert!(t.p.status() == Some(ExitStatus::Signaled(11)), "{case:?}: want SIGSEGV, status {:?} (answer {r})", t.p.status());
            }
            Expect::Ok => {
                kassert!(t.alive(), "{case:?}: a benign frame ended the process");
                accepted_state_is_safe(&t, case)?;
                if matches!(case, Case::UcStackGarbage) {
                    let a = t.th.alt_stack();
                    kassert!((a.sp, a.flags, a.size) == (alt_before.sp, alt_before.flags, alt_before.size), "{case:?}: a bad uc_stack changed the alternate stack");
                }
                if matches!(case, Case::Twice) {
                    // The same frame again (a program may jump back to its restorer): restored again, the same.
                    let (rip, rsp) = (t.ctx.regs.rip, t.ctx.regs.rsp);
                    t.ctx.regs.rsp = frame.wrapping_add(8);
                    let _ = t.call(nr::RT_SIGRETURN, &[]);
                    kassert!(t.alive() && (t.ctx.regs.rip, t.ctx.regs.rsp) == (rip, rsp), "{case:?}: the second sigreturn differed");
                    accepted_state_is_safe(&t, case)?;
                }
            }
            Expect::EntryTrap => {
                // sigreturn accepts the frame (as Linux); the entry path refuses the bad rip in ring 0 and
                // hands the program a #GP instead of jumping there, so there is no escalation.
                kassert!(t.alive(), "{case:?}: sigreturn itself should not end the process");
                match t.ctx.execute_or_stop(true) {
                    Some(hal::entry::ReturnReason::Trap { vector: 13, .. }) => {}
                    other => kassert!(false, "{case:?}: entry gave {other:?}, want Trap #GP"),
                }
            }
        }
    }
    // The hostile cases logged bad frames; the kernel never faulted (it is still running this test).
    kassert!(proc::deliver::bad_frame_count() > faults0, "no bad frame was recorded");
    Ok(())
}

/// What an accepted frame leaves the program with (finding: no escalation through an accepted frame): no
/// IOPL, NT or VM bits in the flags it returns with (IF comes from the entry path, which also drops them
/// again: hal's `USER_RFLAGS`), and an MXCSR without reserved bits in the FPU state now loaded. CS and SS are
/// never taken from a frame: the return to user mode always uses the user selectors (`hal::entry`).
fn accepted_state_is_safe(t: &T, case: Case) -> TestResult {
    let fl = t.ctx.regs.rflags;
    kassert!(fl & (0x3000 | 0x4000 | 0x2_0000) == 0, "{case:?}: rflags {fl:#x} keeps IOPL/NT/VM");
    let img = sf::save_user_fpu().ok_or("no memory for the FPU image")?;
    let mxcsr = u32::from_le_bytes([img[24], img[25], img[26], img[27]]);
    kassert!(mxcsr & !sf::mxcsr_mask() == 0, "{case:?}: MXCSR {mxcsr:#x} has reserved bits");
    Ok(())
}

/// The fpstate pointer stored in a built frame (sigcontext offset 184 from the ucontext at frame + 8).
fn frame_fpstate(t: &T, frame: u64) -> u64 {
    // uc_mcontext sits at frame + 48 (pretcode 8 + uc_flags/uc_link 16 + uc_stack 24); fpstate at +184.
    let mut b = [0u8; 8];
    let _ = t.p.mm.copy_from_user(&mut b, frame + MCTX + 184);
    u64::from_le_bytes(b)
}

/// Offset of uc_mcontext from the frame's start.
const MCTX: u64 = 48;

#[derive(Clone, Copy, Debug)]
enum Case {
    KernelCs,
    NonCanonRip,
    KernelRip,
    SsZeroStrict,
    FpstateUnmapped,
    FpstateKernel,
    XstateBvBad,
    XcompBvSet,
    ReservedHeader,
    MxcsrBad,
    SigmaskAllOnes,
    RflagsIopl,
    FpstateNull,
    /// uc_stack with a kernel ss_sp, unknown ss_flags and a huge size: ignored (`restore_altstack`).
    UcStackGarbage,
    /// rt_sigreturn with no frame at all (rsp moved to unmapped memory).
    NoFrame,
    /// rsp in the kernel half.
    KernelFrame,
    /// A second rt_sigreturn from the same frame.
    Twice,
}

enum Expect {
    Segv,
    Ok,
    EntryTrap,
}

impl Case {
    const ALL: [Case; 17] = [
        Case::KernelCs,
        Case::NonCanonRip,
        Case::KernelRip,
        Case::SsZeroStrict,
        Case::FpstateUnmapped,
        Case::FpstateKernel,
        Case::XstateBvBad,
        Case::XcompBvSet,
        Case::ReservedHeader,
        Case::MxcsrBad,
        Case::SigmaskAllOnes,
        Case::RflagsIopl,
        Case::FpstateNull,
        Case::UcStackGarbage,
        Case::NoFrame,
        Case::KernelFrame,
        Case::Twice,
    ];

    /// The rsp rt_sigreturn is called with (the frame is at rsp - 8).
    fn rsp(self, t: &T, frame: u64) -> u64 {
        match self {
            Case::NoFrame => t.mem + SCRATCH + 0x20_0000,
            Case::KernelFrame => KERNEL_ADDR,
            _ => frame.wrapping_add(8),
        }
    }

    fn expect(self) -> Expect {
        match self {
            // Benign: the kernel keeps itself safe and the program runs on.
            Case::SigmaskAllOnes | Case::RflagsIopl | Case::FpstateNull | Case::UcStackGarbage | Case::Twice => Expect::Ok,
            // A bad rip is accepted by sigreturn and refused when the program tries to run there (AB-1).
            Case::NonCanonRip | Case::KernelRip => Expect::EntryTrap,
            _ => Expect::Segv,
        }
    }

    /// Edits the frame in the process's memory. sigcontext starts at frame + 8 + 48; cs at +144, ss at +150,
    /// rflags at the 18th greg (offset 17*8 = 136), rip at 16*8 = 128.
    fn apply(self, t: &mut T, frame: u64, fpstate: u64) {
        let mc = frame + MCTX;
        let put = |t: &T, at: u64, v: u64| {
            let _ = t.p.mm.copy_to_user(at, &v.to_le_bytes());
        };
        let put16 = |t: &T, at: u64, v: u16| {
            let _ = t.p.mm.copy_to_user(at, &v.to_le_bytes());
        };
        match self {
            Case::KernelCs => put16(t, mc + 144, 0x08),
            Case::NonCanonRip => put(t, mc + 128, 0x8000_0000_0000_0000),
            Case::KernelRip => put(t, mc + 128, 0xffff_ffff_8100_0000),
            Case::SsZeroStrict => put16(t, mc + 150, 0),
            Case::FpstateUnmapped => put(t, mc + 184, t.mem + SCRATCH + 0x10_0000),
            Case::FpstateKernel => put(t, mc + 184, KERNEL_ADDR),
            Case::XstateBvBad => put(t, fpstate + sf::XSTATE_BV_AT as u64, 1u64 << 62),
            Case::XcompBvSet => put(t, fpstate + sf::XSTATE_BV_AT as u64 + 8, 1),
            Case::ReservedHeader => put(t, fpstate + sf::XSTATE_BV_AT as u64 + 16, 1),
            Case::MxcsrBad => {
                let mut b = [0u8; 4];
                let _ = t.p.mm.copy_from_user(&mut b, fpstate + 24);
                b[3] |= 0x80;
                let _ = t.p.mm.copy_to_user(fpstate + 24, &b);
            }
            Case::SigmaskAllOnes => put(t, frame + 304, u64::MAX),
            Case::RflagsIopl => {
                let mut b = [0u8; 8];
                let _ = t.p.mm.copy_from_user(&mut b, mc + 136);
                let v = u64::from_le_bytes(b) | 0x3000 | 0x4000;
                put(t, mc + 136, v);
            }
            Case::FpstateNull => put(t, mc + 184, 0),
            // uc_stack at frame + 24: ss_sp, ss_flags (int), ss_size.
            Case::UcStackGarbage => {
                put(t, frame + 24, KERNEL_ADDR);
                put(t, frame + 32, 0xdead);
                put(t, frame + 40, u64::MAX);
            }
            Case::NoFrame | Case::KernelFrame | Case::Twice => {}
        }
    }
}
