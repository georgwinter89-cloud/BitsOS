//! What prctl(2) keeps (AB-5; linux-6.6/kernel/sys.c `prctl`, security/commoncap.c `cap_task_prctl`): per task
//! (`TaskPrctl` in every `Thread`) and per address space (`MmFlags` in every `Process` image). The syscall
//! itself is `syscall::prctl`; the rules for fork, clone and execve are here, next to the state.
//!
//! - per task (Linux `task_struct`): `comm` (16 bytes, PR_SET_NAME), `pdeath_signal`, `no_new_privs`,
//!   `timer_slack_ns` and the default it resets to, the machine-check flags (PF_MCE_PROCESS/PF_MCE_EARLY),
//!   PR_IO_FLUSHER, and the credential bits V2 keeps constant otherwise (securebits, the capability bounding
//!   set). A new task copies its creator's (`dup_task_struct`), except the parent-death signal, which starts at
//!   0 (`copy_process`), and the default timer slack, which becomes the creator's current slack. execve keeps
//!   all of them (no set-uid programs in V2) and sets `comm` to the new program's file name.
//! - per address space (Linux `mm->flags`): MMF_DUMPABLE (PR_SET_DUMPABLE; 1 at every execve, as
//!   `begin_new_exec` sets SUID_DUMP_USER) and MMF_DISABLE_THP (PR_SET_THP_DISABLE; inherited by fork and kept
//!   across execve, `mmf_init_flags`).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;

/// `TASK_COMM_LEN`.
pub const COMM_LEN: usize = 16;
/// Linux's default timer slack (`INIT_TASK`'s 50 us).
pub const DEFAULT_TIMER_SLACK_NS: u64 = 50_000;
/// `CAP_LAST_CAP` of 6.6 (CAP_CHECKPOINT_RESTORE).
pub const CAP_LAST_CAP: u64 = 40;
/// The full bounding set every task starts with (`CAP_FULL_SET`).
pub const CAP_FULL_SET: u64 = (1u64 << (CAP_LAST_CAP + 1)) - 1;

/// PF_MCE_PROCESS / PF_MCE_EARLY.
pub const MCE_PROCESS: u32 = 1;
pub const MCE_EARLY: u32 = 2;

/// The `comm` of a program: the first 15 bytes of its file name, NUL-padded (`__set_task_comm` with
/// `strscpy_pad`).
pub fn comm_of(name: &[u8]) -> [u8; COMM_LEN] {
    let mut c = [0u8; COMM_LEN];
    let n = name.iter().position(|&b| b == 0).unwrap_or(name.len()).min(COMM_LEN - 1);
    c[..n].copy_from_slice(&name[..n]);
    c
}

/// The prctl state of one task.
pub struct TaskPrctl {
    comm: SpinLock<[u8; COMM_LEN]>,
    pdeath_signal: AtomicU32,
    no_new_privs: AtomicBool,
    timer_slack: AtomicU64,
    default_slack: AtomicU64,
    mce: AtomicU32,
    io_flusher: AtomicBool,
    securebits: AtomicU32,
    cap_bset: AtomicU64,
}

impl Default for TaskPrctl {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskPrctl {
    pub fn new() -> TaskPrctl {
        TaskPrctl {
            comm: SpinLock::named("proc.comm", [0; COMM_LEN]),
            pdeath_signal: AtomicU32::new(0),
            no_new_privs: AtomicBool::new(false),
            timer_slack: AtomicU64::new(DEFAULT_TIMER_SLACK_NS),
            default_slack: AtomicU64::new(DEFAULT_TIMER_SLACK_NS),
            mce: AtomicU32::new(0),
            io_flusher: AtomicBool::new(false),
            securebits: AtomicU32::new(0),
            cap_bset: AtomicU64::new(CAP_FULL_SET),
        }
    }
    /// A new task made by `from` (fork/clone): everything but the parent-death signal; the default slack is
    /// the creator's current slack (linux-6.6/kernel/fork.c `copy_process`).
    pub fn inherit(&self, from: &TaskPrctl) {
        *self.comm.lock() = from.comm();
        self.pdeath_signal.store(0, Ordering::Relaxed);
        self.no_new_privs.store(from.no_new_privs(), Ordering::Relaxed);
        let slack = from.timer_slack();
        self.timer_slack.store(slack, Ordering::Relaxed);
        self.default_slack.store(slack, Ordering::Relaxed);
        self.mce.store(from.mce(), Ordering::Relaxed);
        self.io_flusher.store(from.io_flusher(), Ordering::Relaxed);
        self.securebits.store(from.securebits(), Ordering::Relaxed);
        self.cap_bset.store(from.cap_bset(), Ordering::Relaxed);
    }
    pub fn comm(&self) -> [u8; COMM_LEN] {
        *self.comm.lock()
    }
    pub fn set_comm(&self, c: [u8; COMM_LEN]) {
        *self.comm.lock() = c;
    }
    /// The comm as text (up to its NUL).
    pub fn comm_str(&self) -> alloc::string::String {
        let c = self.comm();
        let n = c.iter().position(|&b| b == 0).unwrap_or(COMM_LEN);
        alloc::string::String::from_utf8_lossy(&c[..n]).into_owned()
    }
    pub fn pdeath_signal(&self) -> u32 {
        self.pdeath_signal.load(Ordering::Acquire)
    }
    pub fn set_pdeath_signal(&self, s: u32) {
        self.pdeath_signal.store(s, Ordering::Release);
    }
    pub fn no_new_privs(&self) -> bool {
        self.no_new_privs.load(Ordering::Acquire)
    }
    /// One way only (`task_set_no_new_privs`).
    pub fn set_no_new_privs(&self) {
        self.no_new_privs.store(true, Ordering::Release);
    }
    pub fn timer_slack(&self) -> u64 {
        self.timer_slack.load(Ordering::Relaxed)
    }
    /// PR_SET_TIMERSLACK: 0 restores the default (the creator's slack at fork).
    pub fn set_timer_slack(&self, ns: u64) {
        let v = if ns == 0 { self.default_slack.load(Ordering::Relaxed) } else { ns };
        self.timer_slack.store(v, Ordering::Relaxed);
    }
    pub fn mce(&self) -> u32 {
        self.mce.load(Ordering::Relaxed)
    }
    pub fn set_mce(&self, f: u32) {
        self.mce.store(f, Ordering::Relaxed);
    }
    pub fn io_flusher(&self) -> bool {
        self.io_flusher.load(Ordering::Relaxed)
    }
    pub fn set_io_flusher(&self, on: bool) {
        self.io_flusher.store(on, Ordering::Relaxed);
    }
    pub fn securebits(&self) -> u32 {
        self.securebits.load(Ordering::Relaxed)
    }
    pub fn set_securebits(&self, b: u32) {
        self.securebits.store(b, Ordering::Relaxed);
    }
    pub fn cap_bset(&self) -> u64 {
        self.cap_bset.load(Ordering::Relaxed)
    }
    /// PR_CAPBSET_DROP.
    pub fn drop_cap(&self, cap: u64) {
        self.cap_bset.fetch_and(!(1u64 << cap), Ordering::Relaxed);
    }
}

/// MMF_DUMPABLE's two values programs can set (SUID_DUMP_DISABLE, SUID_DUMP_USER).
pub const DUMPABLE_MASK: u32 = 3;
pub const DISABLE_THP: u32 = 4;

/// `mm->flags` bits V2 keeps (per image, shared by the threads that share its memory).
pub struct MmFlags(AtomicU32);

impl Default for MmFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl MmFlags {
    /// A new address space: dumpable (SUID_DUMP_USER), THP allowed.
    pub const fn new() -> MmFlags {
        MmFlags(AtomicU32::new(1))
    }
    pub fn get(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }
    pub fn dumpable(&self) -> u32 {
        self.get() & DUMPABLE_MASK
    }
    pub fn set_dumpable(&self, v: u32) {
        let mut cur = self.get();
        while let Err(now) = self.0.compare_exchange_weak(cur, (cur & !DUMPABLE_MASK) | (v & DUMPABLE_MASK), Ordering::AcqRel, Ordering::Acquire) {
            cur = now;
        }
    }
    pub fn thp_disabled(&self) -> bool {
        self.get() & DISABLE_THP != 0
    }
    pub fn set_thp_disabled(&self, on: bool) {
        if on {
            self.0.fetch_or(DISABLE_THP, Ordering::AcqRel);
        } else {
            self.0.fetch_and(!DISABLE_THP, Ordering::AcqRel);
        }
    }
    /// fork copies the parent's flags (`dup_mmap` → `mmf_init_flags`).
    pub fn copy_from(&self, o: &MmFlags) {
        self.0.store(o.get(), Ordering::Release);
    }
    /// execve: the new memory keeps THP's flag and is dumpable again.
    pub fn for_exec(&self, old: &MmFlags) {
        self.0.store((old.get() & DISABLE_THP) | 1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_cut_and_padded() {
        assert_eq!(&comm_of(b"abc")[..4], b"abc\0");
        let c = comm_of(b"a-very-long-program-name");
        assert_eq!(&c[..15], b"a-very-long-pro");
        assert_eq!(c[15], 0);
        assert_eq!(&comm_of(b"ab\0cd")[..3], b"ab\0");
    }

    #[test]
    fn fork_rules() {
        let a = TaskPrctl::new();
        a.set_pdeath_signal(9);
        a.set_no_new_privs();
        a.set_timer_slack(1234);
        a.set_comm(comm_of(b"parent"));
        let b = TaskPrctl::new();
        b.inherit(&a);
        assert_eq!(b.pdeath_signal(), 0, "pdeath is not inherited");
        assert!(b.no_new_privs());
        assert_eq!(b.timer_slack(), 1234);
        b.set_timer_slack(99);
        b.set_timer_slack(0);
        assert_eq!(b.timer_slack(), 1234, "the default is the creator's slack");
        assert_eq!(b.comm_str(), "parent");
        assert_eq!(a.cap_bset(), CAP_FULL_SET);
    }

    #[test]
    fn mm_flags() {
        let m = MmFlags::new();
        assert_eq!(m.dumpable(), 1);
        m.set_dumpable(0);
        m.set_thp_disabled(true);
        let n = MmFlags::new();
        n.for_exec(&m);
        assert_eq!(n.dumpable(), 1);
        assert!(n.thp_disabled());
        let f = MmFlags::new();
        f.copy_from(&m);
        assert_eq!(f.dumpable(), 0);
    }
}
