//! What prctl(2) keeps (AB-5; linux-7.3.1/kernel/sys.c:2534 `prctl`, security/commoncap.c:1301 `cap_task_prctl`;
//! L7-SYNC: Linux 7.3.1's PR_FUTEX_HASH state is `FutexHash`, PR_TIMER_CREATE_RESTORE_IDS lives with the timers in
//! `signal::Timers`, PR_RSEQ_SLICE_EXTENSION has nothing to keep without rseq): per task
//! (`TaskPrctl` in every `Thread`) and per address space (`MmFlags` in every `Process` image). The syscall
//! itself is `syscall::prctl`; the rules for fork, clone and execve are here, next to the state.
//!
//! - per task (Linux `task_struct`): `comm` (16 bytes, PR_SET_NAME), `pdeath_signal`, `no_new_privs`,
//!   `timer_slack_ns` and the default it resets to, the machine-check flags (PF_MCE_PROCESS/PF_MCE_EARLY),
//!   PR_IO_FLUSHER, and the credential bits V2 keeps constant otherwise (securebits, the capability bounding
//!   set). A new task copies its creator's (`dup_task_struct`), except the parent-death signal, which starts at
//!   0 (`copy_process`), and the default timer slack, which becomes the creator's current slack. execve keeps
//!   all of them (no set-uid programs in V2) and sets `comm` to the new program's file name. SX-1: the seccomp
//!   mode and filter stack (`seccomp::TaskSeccomp`) live here too and follow the same rules (`copy_seccomp`).
//! - per address space (Linux `mm->flags`): MMF_DUMPABLE (PR_SET_DUMPABLE; 1 at every execve, as
//!   `begin_new_exec` sets SUID_DUMP_USER) and MMF_DISABLE_THP (PR_SET_THP_DISABLE; inherited by fork and kept
//!   across execve, `mmf_init_flags`).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::{EBUSY, EINVAL};

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
    /// GM-2a CR 4: `current->personality` (personality(2), /proc/<pid>/personality).
    personality: AtomicU32,
    seccomp: seccomp::TaskSeccomp,
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
            personality: AtomicU32::new(0),
            seccomp: seccomp::TaskSeccomp::new(),
        }
    }
    /// A new task made by `from` (fork/clone): everything but the parent-death signal; the default slack is
    /// the creator's current slack (linux-7.3.1/kernel/fork.c:2238 `copy_process`).
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
        self.personality.store(from.personality(), Ordering::Relaxed);
        self.seccomp.inherit(&from.seccomp);
    }
    /// SX-1: `Group::add_thread` under `proc.threads`: the creator's no_new_privs and seccomp stack again, in
    /// case a TSYNC synced the creator between `inherit` and the insertion. True when the stack changed.
    pub fn resync_seccomp(&self, from: &TaskPrctl) -> bool {
        if from.no_new_privs() {
            self.set_no_new_privs();
        }
        self.seccomp.resync(&from.seccomp)
    }
    /// SX-1: the seccomp mode and filter stack.
    #[inline(always)]
    pub fn seccomp(&self) -> &seccomp::TaskSeccomp {
        &self.seccomp
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
    pub fn personality(&self) -> u32 {
        self.personality.load(Ordering::Relaxed)
    }
    pub fn set_personality(&self, p: u32) {
        self.personality.store(p, Ordering::Relaxed);
    }
    /// `begin_new_exec` with set-id bits: `me->personality &= ~bprm->per_clear` (`PER_CLEAR_ON_SETID`:
    /// READ_IMPLIES_EXEC, ADDR_NO_RANDOMIZE, ADDR_COMPAT_LAYOUT, MMAP_PAGE_ZERO;
    /// linux-7.3.1/include/uapi/linux/personality.h:31).
    pub fn clear_personality_on_setid(&self) {
        self.personality.fetch_and(!0x0074_0000, Ordering::Relaxed);
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
    /// SX-2: a new user namespace (or setns into one) gives the full bounding set (`set_cred_user_ns`).
    pub fn set_full_bset(&self) {
        self.cap_bset.store(CAP_FULL_SET, Ordering::Relaxed);
    }
    /// PR_CAPBSET_DROP.
    pub fn drop_cap(&self, cap: u64) {
        self.cap_bset.fetch_and(!(1u64 << cap), Ordering::Relaxed);
    }
}

/// MMF_DUMPABLE's two values programs can set (SUID_DUMP_DISABLE, SUID_DUMP_USER).
pub const DUMPABLE_MASK: u32 = 3;
pub const DISABLE_THP: u32 = 4;

/// `mm->flags` bits V2 keeps (per image, shared by the threads that share its memory), and the address space's
/// futex hash as PR_FUTEX_HASH sees it (`FutexHash`).
pub struct MmFlags(AtomicU32, FutexHash);

impl Default for MmFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl MmFlags {
    /// A new address space: dumpable (SUID_DUMP_USER), THP allowed.
    pub const fn new() -> MmFlags {
        MmFlags(AtomicU32::new(1), FutexHash(AtomicU64::new(0)))
    }
    /// The address space's futex hash (not copied by fork or kept by execve: a new mm has none).
    pub fn futex_hash(&self) -> &FutexHash {
        &self.1
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

/// L7-SYNC: the per-process futex hash of Linux 7.3.1 as far as programs see it (kernel/futex/core.c:2071
/// `futex_hash_prctl`, :1848 `futex_hash_allocate`, :1986 `futex_hash_allocate_default`, :2019
/// `futex_hash_get_slots`): none until the first CLONE_VM clone, then `clamp(roundup_pow_of_two(4 *
/// min(threads, CPUs)), 16, global buckets)` slots, grown at later clones and never shrunk; PR_FUTEX_HASH_SET_SLOTS
/// fixes a size (custom), and 0 switches to the global hash for good (then GET says 0 and any custom size is
/// EBUSY). V2 keeps every futex in its one global table (why: plan/spec/core.md §9.3,
/// futexes); this is the bookkeeping the prctl answers from.
pub struct FutexHash(AtomicU64);

/// `FutexHash` word: the slots in the low 32 bits, then these flags.
const FH_CUSTOM: u64 = 1 << 32;
const FH_GLOBAL: u64 = 1 << 33;

impl FutexHash {
    /// PR_FUTEX_HASH_GET_SLOTS: the private hash's size, 0 for none or the global one.
    pub fn slots(&self) -> u32 {
        let v = self.0.load(Ordering::Acquire);
        if v & FH_GLOBAL != 0 { 0 } else { v as u32 }
    }
    /// PR_FUTEX_HASH_SET_SLOTS (`futex_hash_allocate(n, FH_CUSTOM)`): EINVAL for 1 or a size that is not a power
    /// of two, EBUSY once the global hash was chosen.
    pub fn set_custom(&self, n: u32) -> Result<(), i64> {
        if n != 0 && (n == 1 || !n.is_power_of_two()) {
            return Err(EINVAL);
        }
        let new = FH_CUSTOM | if n == 0 { FH_GLOBAL } else { n as u64 };
        self.0.try_update(Ordering::AcqRel, Ordering::Acquire, |v| (v & FH_GLOBAL == 0).then_some(new)).map(|_| ()).map_err(|_| EBUSY)
    }
    /// `futex_hash_allocate_default` at a CLONE_VM clone that found `threads` threads (`get_nr_threads`) with
    /// `cpus` online and a global table of `global` buckets.
    pub fn grow_default(&self, threads: u32, cpus: u32, global: u32) {
        let want = (4 * threads.min(cpus).max(1)).next_power_of_two().clamp(16, global.max(16)) as u64;
        let _ = self.0.try_update(Ordering::AcqRel, Ordering::Acquire, |v| {
            let current = if v & FH_GLOBAL != 0 { 1 } else { v & 0xffff_ffff };
            (v & (FH_CUSTOM | FH_GLOBAL) == 0 && current < want).then_some(want)
        });
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
        assert_eq!(b.seccomp().mode(), 0);
        let g = seccomp::change();
        a.seccomp().set_strict(&g).unwrap();
        let c = TaskPrctl::new();
        c.inherit(&a);
        assert_eq!(c.seccomp().mode(), seccomp::consts::MODE_STRICT, "seccomp follows fork/clone");
    }

    #[test]
    fn futex_hash_as_linux() {
        let h = MmFlags::new();
        let f = h.futex_hash();
        assert_eq!(f.slots(), 0, "none before a thread");
        f.grow_default(1, 24, 8192);
        assert_eq!(f.slots(), 16, "the first thread: 4 rounded up to the minimum");
        f.grow_default(5, 24, 8192);
        assert_eq!(f.slots(), 32);
        f.grow_default(2, 24, 8192);
        assert_eq!(f.slots(), 32, "never shrinks");
        f.grow_default(100, 24, 8192);
        assert_eq!(f.slots(), 128, "threads count up to the CPUs only");
        assert_eq!(f.set_custom(3), Err(EINVAL));
        assert_eq!(f.set_custom(1), Err(EINVAL));
        assert_eq!(f.set_custom(64), Ok(()));
        f.grow_default(100, 24, 8192);
        assert_eq!(f.slots(), 64, "a custom size stays");
        assert_eq!(f.set_custom(0), Ok(()), "to the global hash");
        assert_eq!(f.slots(), 0);
        assert_eq!(f.set_custom(16), Err(EBUSY), "no way back");
        assert_eq!(f.set_custom(0), Err(EBUSY));
        let g = MmFlags::new();
        g.futex_hash().set_custom(0).unwrap();
        g.futex_hash().grow_default(8, 24, 8192);
        assert_eq!(g.futex_hash().slots(), 0, "a clone after the switch keeps the global hash");
        let k = MmFlags::new();
        k.copy_from(&h);
        assert_eq!(k.futex_hash().slots(), 0, "fork: a new mm has no private hash");
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
