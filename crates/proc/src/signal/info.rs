//! siginfo: the 48-byte kernel record, its union layouts and signalfd's record (linux-6.6/kernel/signal.c,
//! fs/signalfd.c).

use super::{SYNCHRONOUS_MASK, bit_checked};
use uapi::ab::siginfo::*;
use uapi::ab::signal::*;
use uapi::errno::*;
use vfs::Errno;

/// A kernel siginfo (`struct kernel_siginfo`: the first 48 bytes of `siginfo_t`; the 80 bytes after are always
/// zero towards programs). Field offsets as x86-64's `siginfo_t`: si_signo 0, si_errno 4, si_code 8, the union
/// from 16 (si_pid 16, si_uid 20, si_status/sigval 24, si_utime 32, si_stime 40; si_addr 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigInfo(pub [u8; 48]);

impl SigInfo {
    pub const KERNEL_SIZE: usize = 48;
    pub const USER_SIZE: usize = 128;

    pub fn new(signo: u32, code: i32) -> SigInfo {
        let mut s = SigInfo([0; 48]);
        s.put32(0, signo);
        s.put32(8, code as u32);
        s
    }
    fn put32(&mut self, off: usize, v: u32) {
        self.0[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(&mut self, off: usize, v: u64) {
        self.0[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    pub fn get32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.0[off..off + 4].try_into().unwrap_or([0; 4]))
    }
    pub fn get64(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.0[off..off + 8].try_into().unwrap_or([0; 8]))
    }
    pub fn signo(&self) -> u32 {
        self.get32(0)
    }
    pub fn set_signo(&mut self, sig: u32) {
        self.put32(0, sig);
    }
    pub fn errno(&self) -> i32 {
        self.get32(4) as i32
    }
    pub fn code(&self) -> i32 {
        self.get32(8) as i32
    }
    pub fn pid(&self) -> u32 {
        self.get32(16)
    }
    pub fn uid(&self) -> u32 {
        self.get32(20)
    }
    /// `SEND_SIG_NOINFO` / kill(2): SI_USER (or SI_TKILL / SI_QUEUE ...) with the sender's pid and uid.
    pub fn user(sig: u32, code: i32, pid: u32, uid: u32) -> SigInfo {
        let mut s = SigInfo::new(sig, code);
        s.put32(16, pid);
        s.put32(20, uid);
        s
    }
    /// `SEND_SIG_PRIV`: from the kernel (SI_KERNEL, pid and uid 0).
    pub fn kernel(sig: u32) -> SigInfo {
        SigInfo::new(sig, SI_KERNEL)
    }
    /// A fault (`force_sig_fault`): si_addr.
    pub fn fault(sig: u32, code: i32, addr: u64) -> SigInfo {
        let mut s = SigInfo::new(sig, code);
        s.put64(16, addr);
        s
    }
    /// SIGCHLD (`do_notify_parent`, `do_notify_parent_cldstop`): pid, uid, status, user and system time in
    /// clock ticks.
    pub fn chld(sig: u32, code: i32, pid: u32, uid: u32, status: i32, utime: u64, stime: u64) -> SigInfo {
        let mut s = SigInfo::user(sig, code, pid, uid);
        s.put32(24, status as u32);
        s.put64(32, utime);
        s.put64(40, stime);
        s
    }
    /// The 128 bytes a program sees (`copy_siginfo_to_user`: the kernel part, zeros after).
    pub fn to_user(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[..48].copy_from_slice(&self.0);
        b
    }
    /// From a program's 128 bytes (`__copy_siginfo_from_user`): signo replaced by `sig`; E2BIG when the layout
    /// is unknown and the 80 bytes after the kernel part are not zero (`post_copy_siginfo_from_user`).
    pub fn from_user(sig: u32, b: &[u8; 128]) -> Result<SigInfo, Errno> {
        let mut s = SigInfo([0; 48]);
        s.0.copy_from_slice(&b[..48]);
        s.set_signo(sig);
        if !known_layout(sig, s.code()) && b[48..].iter().any(|&x| x != 0) {
            return Err(E2BIG);
        }
        Ok(s)
    }
    /// SIGSEGV/SIGBUS/SIGILL/... raised by the CPU or the kernel for this thread (si_code > SI_USER).
    pub fn is_kernel_fault(&self) -> bool {
        self.code() > SI_USER && bit_checked(self.signo()) & SYNCHRONOUS_MASK != 0
    }
}

/// Which union member a siginfo uses (`siginfo_layout`, kernel/signal.c:3326).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Kill,
    Timer,
    Poll,
    Fault,
    Chld,
    Rt,
    Sys,
}

fn specific_limit(sig: u32) -> Option<(i32, Layout)> {
    Some(match sig {
        SIGILL => (NSIGILL, Layout::Fault),
        SIGFPE => (NSIGFPE, Layout::Fault),
        SIGSEGV => (NSIGSEGV, Layout::Fault),
        SIGBUS => (NSIGBUS, Layout::Fault),
        SIGTRAP => (NSIGTRAP, Layout::Fault),
        SIGCHLD => (NSIGCHLD, Layout::Chld),
        SIGIO => (NSIGPOLL, Layout::Poll),
        SIGSYS => (NSIGSYS, Layout::Sys),
        _ => return None,
    })
}

pub fn layout(sig: u32, code: i32) -> Layout {
    if code > SI_USER && code < SI_KERNEL {
        match specific_limit(sig) {
            Some((limit, l)) if code <= limit => l,
            Some(_) => Layout::Kill,
            None if code <= NSIGPOLL => Layout::Poll,
            None => Layout::Kill,
        }
    } else if code == SI_TIMER {
        Layout::Timer
    } else if code == SI_SIGIO {
        Layout::Poll
    } else if code < 0 {
        Layout::Rt
    } else {
        Layout::Kill
    }
}

/// `known_siginfo_layout` (kernel/signal.c:3279).
fn known_layout(sig: u32, code: i32) -> bool {
    if code == SI_KERNEL {
        true
    } else if code > SI_USER {
        match specific_limit(sig) {
            Some((limit, _)) => code <= limit,
            None => code <= NSIGPOLL,
        }
    } else {
        code >= SI_DETHREAD || code == SI_ASYNCNL
    }
}

/// `struct signalfd_siginfo` for a dequeued signal (`signalfd_copyinfo`, fs/signalfd.c:84).
pub fn signalfd_record(i: &SigInfo) -> [u8; 128] {
    let mut r = [0u8; 128];
    let mut put = |off: usize, v: u64, w: usize| r[off..off + w].copy_from_slice(&v.to_le_bytes()[..w]);
    put(0, i.signo() as u64, 4);
    put(4, i.errno() as u32 as u64, 4);
    put(8, i.code() as u32 as u64, 4);
    match layout(i.signo(), i.code()) {
        Layout::Kill => {
            put(12, i.get32(16) as u64, 4);
            put(16, i.get32(20) as u64, 4);
        }
        Layout::Timer => {
            put(24, i.get32(16) as u64, 4); // ssi_tid
            put(32, i.get32(20) as u64, 4); // ssi_overrun
            put(48, i.get64(24), 8); // ssi_ptr
            put(44, i.get32(24) as u64, 4); // ssi_int
        }
        Layout::Poll => {
            put(28, i.get64(16), 4); // ssi_band (the long, cut to u32)
            put(20, i.get32(24) as u64, 4); // ssi_fd
        }
        Layout::Fault => put(72, i.get64(16), 8),
        Layout::Chld => {
            put(12, i.get32(16) as u64, 4);
            put(16, i.get32(20) as u64, 4);
            put(40, i.get32(24) as u64, 4);
            put(56, i.get64(32), 8);
            put(64, i.get64(40), 8);
        }
        Layout::Rt => {
            put(12, i.get32(16) as u64, 4);
            put(16, i.get32(20) as u64, 4);
            put(48, i.get64(24), 8);
            put(44, i.get32(24) as u64, 4);
        }
        Layout::Sys => {
            put(88, i.get64(16), 8); // ssi_call_addr
            put(84, i.get32(24) as u64, 4); // ssi_syscall
            put(96, i.get32(28) as u64, 4); // ssi_arch
        }
    }
    r
}
