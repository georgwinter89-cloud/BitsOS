//! AB-7b: `/proc/sys/kernel` (linux-6.6 kernel/utsname_sysctl.c, kernel/pid.c `pid_max`, drivers/char/random.c
//! `random_table`, kernel/sysctl.c `kern_table`, kernel/capability.c `cap_last_cap`) and `/proc/sys/vm`
//! (mm/util.c, kernel/sysctl.c `vm_table`, security/min_addr.c), a second source beside FS's `memfd_noexec`
//! on the same directory.
//!
//! Knobs V2 cannot carry out (overcommit, ASLR, map and thread ceilings) keep showing what V2 does; a write
//! inside Linux's range is accepted so `sysctl -p` and systemd-sysctl do not fail, and leaves a
//! `sysctl.not_honoured` event. Knobs without an effect in V2 (overflow ids, swappiness, taint) keep what root
//! wrote, as Linux does.

use super::attr::Live;
use crate::uts;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ops::RangeInclusive;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;
use vfs::synth::{DirSource, SynthEntry};
use vfs::sysctl::Knob;
use vfs::{Cred, Errno};

/// `proc_dostring` on a write: the bytes up to the first NUL or newline, at most 64 (`maxlen - 1`); the whole
/// count is answered.
pub(super) fn uts_write(data: &[u8]) -> &[u8] {
    let end = data.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(data.len());
    &data[..end.min(uts::UTS_LEN)]
}

/// The name's bytes as they are (sethostname takes any byte, uname(2) returns them unchanged).
fn uts_file(which: usize) -> Live {
    Live::bytes(0, move || {
        let mut b = uts::name(which);
        b.push(b'\n');
        Ok(b)
    })
    .writable(move |_who: Cred, data: &[u8]| {
        uts::set(which, uts_write(data));
        Ok(data.len())
    })
}

/// A write inside Linux's range that V2 cannot carry out: accepted, the file keeps V2's value.
fn not_honoured(knob: &'static str, v: u64, linux: RangeInclusive<u64>) -> Result<(), Errno> {
    if !linux.contains(&v) {
        klog::event!(Info, "syscall", "sysctl.out_of_range", knob = knob, asked = v);
        return Err(EINVAL);
    }
    klog::event!(Info, "syscall", "sysctl.not_honoured", knob = knob, asked = v);
    Ok(())
}

/// A value without an effect in V2: kept as root wrote it, within Linux's range.
fn store(knob: &'static str, at: &AtomicU64, v: u64, linux: RangeInclusive<u64>) -> Result<(), Errno> {
    if !linux.contains(&v) {
        klog::event!(Info, "syscall", "sysctl.out_of_range", knob = knob, asked = v);
        return Err(EINVAL);
    }
    at.store(v, Ordering::Relaxed);
    klog::event!(Info, "syscall", "sysctl.set", knob = knob, value = v);
    Ok(())
}

const INT_MAX: u64 = i32::MAX as u64;

/// `generate_random_uuid` as `%pU`: version 4, variant 1, lower-case 8-4-4-4-12.
pub(super) fn uuid_text(mut u: [u8; 16]) -> String {
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    let mut s = String::new();
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        let _ = write!(s, "{b:02x}");
    }
    s.push('\n');
    s
}

fn random_uuid() -> String {
    let mut u = [0u8; 16];
    vfs::random::fill(&mut u);
    uuid_text(u)
}

/// `boot_id`: made at its first read, the same until the next boot.
static BOOT_ID: ksync::Once<String> = ksync::Once::new();

fn pid_max() -> u64 {
    proc::pid_max() as u64
}

/// `pid_max_min`..=`pid_max_max` (301..=4194304), checked by proc; later ids stay below the new value.
fn set_pid_max(v: u64) -> Result<(), Errno> {
    proc::set_pid_max(v)
}

/// `max_threads` as `fork_init` sets it (RLIMIT_NPROC's default is half of it); V2 has no global ceiling.
fn threads_max() -> u64 {
    proc::rlimit::max_threads_default(mm_core::frames().map_or(0, |f| f.stats().total_bytes / 4096))
}

/// `sysctl_max_threads`: MIN_THREADS..=MAX_THREADS (FUTEX_TID_MASK).
fn set_threads_max(v: u64) -> Result<(), Errno> {
    not_honoured("threads-max", v, 20..=0x3fff_ffff)
}

/// V2 places programs without randomisation (proc/load.rs, proc/stack.rs): 0.
fn randomize() -> u64 {
    0
}

fn set_randomize(v: u64) -> Result<(), Errno> {
    not_honoured("randomize_va_space", v, 0..=INT_MAX)
}

static OVERFLOW_UID: AtomicU64 = AtomicU64::new(65534);
static OVERFLOW_GID: AtomicU64 = AtomicU64::new(65534);
/// `TAINT_FLAGS_MAX`: 19 flags; a write adds flags (`proc_taint`), higher bits are ignored.
const TAINT_FLAGS_MAX: u64 = (1 << 19) - 1;
static TAINTED: AtomicU64 = AtomicU64::new(0);

fn overflowuid() -> u64 {
    OVERFLOW_UID.load(Ordering::Relaxed)
}
fn set_overflowuid(v: u64) -> Result<(), Errno> {
    store("overflowuid", &OVERFLOW_UID, v, 0..=65535)
}
fn overflowgid() -> u64 {
    OVERFLOW_GID.load(Ordering::Relaxed)
}
fn set_overflowgid(v: u64) -> Result<(), Errno> {
    store("overflowgid", &OVERFLOW_GID, v, 0..=65535)
}
fn tainted() -> u64 {
    TAINTED.load(Ordering::Relaxed)
}
fn set_tainted(v: u64) -> Result<(), Errno> {
    TAINTED.fetch_or(v & TAINT_FLAGS_MAX, Ordering::Relaxed);
    klog::event!(Warn, "syscall", "sysctl.tainted", flags = v & TAINT_FLAGS_MAX);
    Ok(())
}

struct RandomDir;

impl DirSource for RandomDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let boot = || Ok(BOOT_ID.get_or_init(random_uuid).clone());
        // Linux 6.6: the pool is always full (256 bits) once initialised.
        Ok(alloc::vec![
            SynthEntry::file("boot_id", 0o444, Arc::new(Live::new(0, boot))).cookie(1),
            SynthEntry::file("entropy_avail", 0o444, Arc::new(Live::new(0, || Ok(String::from("256\n"))))).cookie(2),
            SynthEntry::file("poolsize", 0o444, Arc::new(Live::new(0, || Ok(String::from("256\n"))))).cookie(3),
            SynthEntry::file("uuid", 0o444, Arc::new(Live::new(0, || Ok(random_uuid())))).cookie(4),
        ])
    }
}

/// `kernel.yama.ptrace_scope` (linux-6.6/security/yama/yama_lsm.c:27,432): default 1 (relational). V2 has no
/// ptrace(2), so no attach exists; the attach-mode checks that do (pidfd_getfd, process_vm_readv/writev:
/// `Process::may_ptrace`) do not apply the scope yet (they act as scope 0 would; C1: an AB item, V2-LANES AB-18).
/// The value answers readers such as Chromium's sandbox check; a write needs CAP_SYS_PTRACE
/// (checked before the number, :437), takes 0..=3, and once 3 is set nothing lower is accepted (:440-443).
static PTRACE_SCOPE: AtomicU64 = AtomicU64::new(1);
const YAMA_SCOPE_NO_ATTACH: u64 = 3;

fn set_ptrace_scope(data: &[u8]) -> Result<usize, Errno> {
    if proc::current().is_some_and(|me| !me.capable(proc::cred::Cap::SysPtrace)) {
        klog::event!(Info, "syscall", "sysctl.denied", knob = "yama.ptrace_scope");
        return Err(EPERM);
    }
    if data.is_empty() {
        return Ok(0);
    }
    if let Some(v) = vfs::sysctl::parse_ulong(data)? {
        let min = if PTRACE_SCOPE.load(Ordering::Relaxed) == YAMA_SCOPE_NO_ATTACH { YAMA_SCOPE_NO_ATTACH } else { 0 };
        store("yama.ptrace_scope", &PTRACE_SCOPE, v, min..=YAMA_SCOPE_NO_ATTACH)?;
    }
    Ok(data.len())
}

struct YamaDir;

impl DirSource for YamaDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let scope = Live::new(0, || Ok(alloc::format!("{}\n", PTRACE_SCOPE.load(Ordering::Relaxed))))
            .writable(|_who: Cred, data: &[u8]| set_ptrace_scope(data));
        Ok(alloc::vec![SynthEntry::file("ptrace_scope", 0o644, Arc::new(scope)).cookie(1)])
    }
}

struct KernelDir;

impl DirSource for KernelDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let fixed = |s: &'static str| Arc::new(Live::new(0, move || Ok(alloc::format!("{s}\n"))));
        let knob = |get: fn() -> u64, set: fn(u64) -> Result<(), Errno>| Arc::new(Knob { get, set, uint: false });
        let mut v = alloc::vec![
            // CAP_LAST_CAP of 6.6: CAP_CHECKPOINT_RESTORE.
            SynthEntry::file("cap_last_cap", 0o444, fixed("40")),
            SynthEntry::file("domainname", 0o644, Arc::new(uts_file(uts::DOMAIN))),
            SynthEntry::file("hostname", 0o644, Arc::new(uts_file(uts::HOST))),
            // NGROUPS_MAX.
            SynthEntry::file("ngroups_max", 0o444, fixed("65536")),
            SynthEntry::file("osrelease", 0o444, fixed(crate::info::RELEASE)),
            SynthEntry::file("ostype", 0o444, fixed("Linux")),
            SynthEntry::file("overflowgid", 0o644, knob(overflowgid, set_overflowgid)),
            SynthEntry::file("overflowuid", 0o644, knob(overflowuid, set_overflowuid)),
            SynthEntry::file("pid_max", 0o644, knob(pid_max, set_pid_max)),
            SynthEntry::dir("random", Arc::new(RandomDir)),
            SynthEntry::file("randomize_va_space", 0o644, knob(randomize, set_randomize)),
            SynthEntry::file("tainted", 0o644, knob(tainted, set_tainted)),
            SynthEntry::file("threads-max", 0o644, knob(threads_max, set_threads_max)),
            SynthEntry::file("version", 0o444, fixed(crate::info::VERSION)),
            SynthEntry::dir("yama", Arc::new(YamaDir)),
        ];
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

fn kernel_dir() -> Arc<dyn DirSource> {
    Arc::new(KernelDir)
}
vfs::procdir!("/proc/sys/kernel", kernel_dir);

/// V2 always commits strictly (MM-5b): mode 2 (OVERCOMMIT_NEVER) whatever root writes (Linux takes 0..=2).
fn overcommit() -> u64 {
    2
}

fn set_overcommit(v: u64) -> Result<(), Errno> {
    if v == 2 {
        return Ok(());
    }
    not_honoured("overcommit_memory", v, 0..=2)
}

/// The share of RAM CommitLimit allows (Linux: `totalram * ratio / 100`); V2's limit is RAM less a reserve
/// (mm::commit::limit_for), shown as the whole percent it comes to.
fn overcommit_ratio() -> u64 {
    let total = mm_core::frames().map_or(0, |f| f.stats().total_bytes / 4096);
    ratio_of(mm::commit::limit(), total)
}

pub(super) fn ratio_of(limit_pages: u64, total_pages: u64) -> u64 {
    if total_pages == 0 { 50 } else { (limit_pages.min(total_pages) as u128 * 100 / total_pages as u128) as u64 }
}

fn set_overcommit_ratio(v: u64) -> Result<(), Errno> {
    not_honoured("overcommit_ratio", v, 0..=INT_MAX)
}

/// 0: the ratio decides (V2's limit is not a fixed size).
fn overcommit_kbytes() -> u64 {
    0
}

fn set_overcommit_kbytes(v: u64) -> Result<(), Errno> {
    not_honoured("overcommit_kbytes", v, 0..=u64::MAX)
}

/// V2 has no limit on mappings (R1): INT_MAX less MAPCOUNT_ELF_CORE_MARGIN, the largest value core dumps allow
/// (what SteamOS and gaming distributions set).
fn max_map_count() -> u64 {
    INT_MAX - 5
}

fn set_max_map_count(v: u64) -> Result<(), Errno> {
    not_honoured("max_map_count", v, 0..=INT_MAX)
}

/// Enforced by mm (`mm::linux::MMAP_MIN_ADDR`, Linux's default).
fn mmap_min_addr() -> u64 {
    mm::linux::MMAP_MIN_ADDR
}

fn set_mmap_min_addr(v: u64) -> Result<(), Errno> {
    not_honoured("mmap_min_addr", v, 0..=u64::MAX)
}

/// No swap: kept as written (Linux's default 60, range 0..=200).
static SWAPPINESS: AtomicU64 = AtomicU64::new(60);

fn swappiness() -> u64 {
    SWAPPINESS.load(Ordering::Relaxed)
}

fn set_swappiness(v: u64) -> Result<(), Errno> {
    store("swappiness", &SWAPPINESS, v, 0..=200)
}

/// BL-0: `/proc/sys/vm/drop_caches` (linux-7/fs/drop_caches.c:78, mode 0200, `proc_dointvec_minmax` 1..=4). Write
/// only, for root alone: Linux's `proc_sys_permission` (`test_perm`) gives root the owner bits and everyone else
/// the other bits, so root may only write and nobody else may open it. The work is `vfs::reclaim::drop_caches`.
struct DropCaches;

impl vfs::synth::FileSource for DropCaches {
    fn permission(&self, who: Cred, want: u32) -> Option<Result<(), Errno>> {
        use vfs::inode::{MAY_EXEC, MAY_READ, MAY_WRITE};
        let allowed = if who.uid == 0 { MAY_WRITE } else { 0 };
        Some(if want & (MAY_READ | MAY_WRITE | MAY_EXEC) & !allowed == 0 { Ok(()) } else { Err(EACCES) })
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(Vec::new())
    }
    fn write(&self, _who: Cred, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0);
        }
        let Some(v) = vfs::sysctl::parse_ulong(data)? else { return Ok(data.len()) };
        if !(1..=4).contains(&v) {
            klog::event!(Info, "syscall", "sysctl.out_of_range", knob = "drop_caches", asked = v);
            return Err(EINVAL);
        }
        let (comm, pid) = match proc::current_thread() {
            Some(t) => (t.prctl().comm_str(), proc::current().map_or(0, |p| p.pid as u64)),
            None => (String::from("kernel"), 0),
        };
        vfs::reclaim::drop_caches(v, &comm, pid);
        Ok(data.len())
    }
}

/// W35 MM-3: `compact_memory` (Linux `sysctl_compaction_handler`, mode 0200): writing 1 compacts every zone in the
/// writer's context (`mm::compact::compact_memory`); any other number is EINVAL. Reads as 0 where the mode lets one.
fn compact_memory() -> u64 {
    0
}

fn set_compact_memory(v: u64) -> Result<(), Errno> {
    if v != 1 {
        return Err(EINVAL);
    }
    mm::compact::compact_memory();
    Ok(())
}

/// W35 MM-3: `compaction_proactiveness` (0..=100, default 20; 0 turns kcompactd's proactive work off).
fn compaction_proactiveness() -> u64 {
    u64::from(mm_core::compact::proactiveness())
}

fn set_compaction_proactiveness(v: u64) -> Result<(), Errno> {
    if mm_core::compact::set_proactiveness(v) { Ok(()) } else { Err(EINVAL) }
}

/// W35 MM-3: `extfrag_threshold` (0..=1000, default 500).
fn extfrag_threshold() -> u64 {
    u64::from(mm_core::compact::extfrag_threshold())
}

fn set_extfrag_threshold(v: u64) -> Result<(), Errno> {
    if mm_core::compact::set_extfrag_threshold(v) { Ok(()) } else { Err(EINVAL) }
}

/// W35 MM-3: `compact_unevictable_allowed` (0..=1, default 1; kept: no migrator moves mlocked pages yet).
fn compact_unevictable_allowed() -> u64 {
    u64::from(mm_core::compact::unevictable_allowed())
}

fn set_compact_unevictable_allowed(v: u64) -> Result<(), Errno> {
    if mm_core::compact::set_unevictable_allowed(v) { Ok(()) } else { Err(EINVAL) }
}

struct VmDir;

impl DirSource for VmDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let knob = |get: fn() -> u64, set: fn(u64) -> Result<(), Errno>| Arc::new(Knob { get, set, uint: false });
        let mut v = alloc::vec![
            SynthEntry::file("compact_memory", 0o200, knob(compact_memory, set_compact_memory)),
            SynthEntry::file("compact_unevictable_allowed", 0o644, knob(compact_unevictable_allowed, set_compact_unevictable_allowed)),
            SynthEntry::file("compaction_proactiveness", 0o644, knob(compaction_proactiveness, set_compaction_proactiveness)),
            SynthEntry::file("drop_caches", 0o200, Arc::new(DropCaches)),
            SynthEntry::file("extfrag_threshold", 0o644, knob(extfrag_threshold, set_extfrag_threshold)),
            SynthEntry::file("max_map_count", 0o644, knob(max_map_count, set_max_map_count)),
            SynthEntry::file("mmap_min_addr", 0o644, knob(mmap_min_addr, set_mmap_min_addr)),
            SynthEntry::file("overcommit_kbytes", 0o644, knob(overcommit_kbytes, set_overcommit_kbytes)),
            SynthEntry::file("overcommit_memory", 0o644, knob(overcommit, set_overcommit)),
            SynthEntry::file("overcommit_ratio", 0o644, knob(overcommit_ratio, set_overcommit_ratio)),
            SynthEntry::file("swappiness", 0o644, knob(swappiness, set_swappiness)),
        ];
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

fn vm_dir() -> Arc<dyn DirSource> {
    Arc::new(VmDir)
}
vfs::procdir!("/proc/sys/vm", vm_dir);

/// SX-3b: `/proc/sys/user` (linux-6.6/kernel/ucount.c `user_table`): the namespace limits of the reader's user
/// namespace, the kinds V2 counts (`proc::ns::count`). stat shows the table's 0644 owned by root, as Linux; access
/// is `set_permissions` through the file's own permission op (`proc_sys_permission`): read and write for a
/// caller with CAP_SYS_RESOURCE over its own user namespace, read only for everyone else, never exec.
const USER_LIMITS: [(&str, proc::ns::count::Counted); 6] = {
    use proc::ns::count::Counted::*;
    [
        ("max_ipc_namespaces", Ipc),
        ("max_mnt_namespaces", Mnt),
        ("max_net_namespaces", Net),
        ("max_pid_namespaces", Pid),
        ("max_user_namespaces", User),
        ("max_uts_namespaces", Uts),
    ]
};

/// The reader's user namespace and whether it may change its limits.
fn reader_user_ns() -> (Arc<proc::ns::user::UserNs>, bool) {
    match proc::current() {
        Some(me) => {
            let c = me.cred();
            let may = proc::cred::ns_capable(&c, &c.user_ns, proc::cred::Cap::SysResource);
            (c.user_ns.clone(), may)
        }
        None => (proc::ns::user::init_user_ns().clone(), true),
    }
}

struct NsLimit(proc::ns::count::Counted);

impl vfs::synth::FileSource for NsLimit {
    /// `test_perm` with `set_permissions`' mode (linux-6.6/fs/proc/proc_sysctl.c:416, kernel/ucount.c:40): the
    /// mode is the same for owner, group and others, so only the capability counts.
    /// access(2) (MAY_ACCESS) asks with the real ids, whose capabilities count only for the namespace's root
    /// uid (linux-6.6/fs/open.c:420 `access_override_creds`; AT_EACCESS keeps them).
    fn permission(&self, _who: Cred, want: u32) -> Option<Result<(), Errno>> {
        use vfs::inode::{MAY_ACCESS, MAY_EXEC, MAY_WRITE};
        let may_write = || {
            let Some(me) = proc::current() else { return true };
            let c = me.cred();
            let caps_kept = want & MAY_ACCESS == 0 || c.user_ns.uid(c.uid) == 0;
            caps_kept && proc::cred::ns_capable(&c, &c.user_ns, proc::cred::Cap::SysResource)
        };
        let denied = want & MAY_EXEC != 0 || (want & MAY_WRITE != 0 && !may_write());
        Some(if denied { Err(EACCES) } else { Ok(()) })
    }
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write && !reader_user_ns().1 {
            return Err(EACCES);
        }
        Ok(())
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok(alloc::format!("{}\n", reader_user_ns().0.limits.get(self.0)).into_bytes())
    }
    fn write(&self, who: Cred, data: &[u8]) -> Result<usize, Errno> {
        self.write_at(who, 0, data)
    }
    /// `proc_dointvec_minmax` with 0..=INT_MAX (linux-6.6/kernel/sysctl.c:471 `__do_proc_dointvec`): a write at a
    /// non-zero position changes nothing and counts as written (SYSCTL_WRITES_STRICT, the default: `goto out`
    /// keeps `*lenp`); one made only of white space is EINVAL.
    fn write_at(&self, _who: Cred, pos: u64, data: &[u8]) -> Result<usize, Errno> {
        let (ns, may) = reader_user_ns();
        if !may {
            return Err(EPERM);
        }
        if data.is_empty() || pos != 0 {
            return Ok(data.len());
        }
        let v = vfs::sysctl::parse_ulong(data)?.ok_or(EINVAL)?;
        ns.limits.set(self.0, v)?;
        klog::event!(Info, "syscall", "sysctl.ns_limit", kind = self.0.name(), value = v, userns = ns.inum);
        Ok(data.len())
    }
}

struct UserDir;

impl DirSource for UserDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let mut v: Vec<SynthEntry> = USER_LIMITS.iter().map(|&(name, kind)| SynthEntry::file(name, 0o644, Arc::new(NsLimit(kind)))).collect();
        for (i, e) in v.iter_mut().enumerate() {
            e.cookie = i as u64 + 1;
        }
        Ok(v)
    }
}

fn user_dir() -> Arc<dyn DirSource> {
    Arc::new(UserDir)
}
vfs::procdir!("/proc/sys/user", user_dir);

#[cfg(test)]
mod compaction_tests {
    use super::*;

    /// W35 MM-3: Linux's ranges and defaults (CachyOS 7.3.1 reads extfrag_threshold 500, compact_unevictable_allowed 1;
    /// its compaction_proactiveness is 0 by CachyOS's choice, Linux's and BitsOS's default is 20).
    #[test]
    fn compaction_knobs_keep_linux_ranges() {
        assert_eq!((extfrag_threshold(), compact_unevictable_allowed(), compact_memory()), (500, 1, 0));
        assert_eq!(compaction_proactiveness(), 20);
        assert_eq!(set_compaction_proactiveness(101), Err(EINVAL));
        assert_eq!(set_extfrag_threshold(1001), Err(EINVAL));
        assert_eq!(set_compact_unevictable_allowed(2), Err(EINVAL));
        assert_eq!(set_compact_memory(0), Err(EINVAL));
        assert_eq!(set_compact_memory(2), Err(EINVAL));
        assert_eq!(set_compact_memory(1), Ok(()), "no boot allocator on the host: nothing to compact, no error");
        assert_eq!(set_extfrag_threshold(1000), Ok(()));
        assert_eq!(extfrag_threshold(), 1000);
        assert_eq!(set_extfrag_threshold(500), Ok(()));
        let entries = VmDir.entries().unwrap();
        let mode = |n: &str| entries.iter().find(|e| e.name == n.as_bytes()).map(|e| e.perm & 0o777);
        assert_eq!(mode("compact_memory"), Some(0o200), "write-only as Linux");
        assert_eq!(mode("compaction_proactiveness"), Some(0o644));
        assert_eq!(mode("extfrag_threshold"), Some(0o644));
        assert_eq!(mode("compact_unevictable_allowed"), Some(0o644));
    }
}
