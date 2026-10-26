//! memfd (FS-10): `memfd_create` and file seals (`F_ADD_SEALS`/`F_GET_SEALS`), as linux-6.6/mm/memfd.c and the
//! seal checks of linux-6.6/mm/shmem.c.
//!
//! A memfd is a nameless tmpfs file on an internal tmpfs instance (Linux's `shm_mnt`): its pages are an FS-5
//! page cache charged to that instance's pool at size change (ENOMEM from the system pool, R2), it maps like
//! any tmpfs file (`mmap`), and its dentry is named `memfd:<name>` and already unlinked, so `/proc/<pid>/fd`
//! and `/proc/<pid>/maps` show `/memfd:<name> (deleted)`. Every tmpfs file has seals (they start as
//! F_SEAL_SEAL, so only a memfd made with MFD_ALLOW_SEALING or MFD_NOEXEC_SEAL can get more); the seals live
//! in the file's `PageCache` next to its count of shared mappings that may write, under `vfs.inode.data`:
//!
//! - F_SEAL_SHRINK / F_SEAL_GROW: a truncate that shrinks / grows is EPERM; a write that would grow the file
//!   stops at the page-sized step that would (EPERM when it is the first);
//! - F_SEAL_WRITE: write(2) is EPERM, a new writable MAP_SHARED mapping is EPERM, a read-only one cannot get
//!   PROT_WRITE by mprotect (EACCES); adding it is EBUSY while a shared mapping that may write exists;
//! - F_SEAL_FUTURE_WRITE: the same for new writes and mappings, existing writable mappings keep writing;
//! - F_SEAL_EXEC: the exec bits may not change (chmod; V2 has no chmod yet, `check_mode_change` is its hook);
//!   added to a file with an exec bit it brings the four seals above along (W^X);
//! - F_SEAL_SEAL: no more seals.
//!
//! fallocate (`PageCache::fallocate`) obeys them as shmem does: a hole punch is EPERM under F_SEAL_WRITE or
//! F_SEAL_FUTURE_WRITE, growth past the end EPERM under F_SEAL_GROW. Every refusal by a seal is counted and logged
//! (`seal_refused`), the only way to see on the Mini-PC why a client's or compositor's call failed.
//!
//! Where CachyOS's kernel (7.x) differs from 6.6 the oracle wins (the ABI diff runs there): a read-only
//! MAP_SHARED mapping of an F_SEAL_WRITE file is allowed (6.6 refused it with EPERM in `mapping_map_writable`;
//! fixed upstream for exactly the compositor case, so a Wayland compositor can map a client's sealed buffer).
//!
//! The seal commands are fcntl's (`memfd_fcntl`): `seal_fcntl` is the call fcntl dispatches F_ADD_SEALS and
//! F_GET_SEALS to (today `sys::fcntl` here; AB-5 takes fcntl over and keeps calling it).

use crate::fd::OpenFile;
use crate::inode::{Cred, Inode};
use crate::mount_ns::{Mount, Path};
use crate::sys::{Args, Sys};
use crate::Errno;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use uapi::errno::*;

// linux-6.6/include/uapi/linux/memfd.h:8-24
pub const MFD_CLOEXEC: u32 = 0x1;
pub const MFD_ALLOW_SEALING: u32 = 0x2;
pub const MFD_HUGETLB: u32 = 0x4;
pub const MFD_NOEXEC_SEAL: u32 = 0x8;
pub const MFD_EXEC: u32 = 0x10;
const MFD_ALL_FLAGS: u32 = MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB | MFD_NOEXEC_SEAL | MFD_EXEC;
/// `HUGETLB_FLAG_ENCODE_SHIFT`/`_MASK`: a huge page size may be encoded in bits 26..31 with MFD_HUGETLB.
const MFD_HUGE_SHIFT: u32 = 26;
const MFD_HUGE_MASK: u32 = 0x3f;

// linux-6.6/include/uapi/linux/fcntl.h:35-46
pub const F_ADD_SEALS: u64 = 1033;
pub const F_GET_SEALS: u64 = 1034;
pub const F_SEAL_SEAL: u32 = 0x1;
pub const F_SEAL_SHRINK: u32 = 0x2;
pub const F_SEAL_GROW: u32 = 0x4;
pub const F_SEAL_WRITE: u32 = 0x8;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x10;
pub const F_SEAL_EXEC: u32 = 0x20;
pub const F_ALL_SEALS: u32 = F_SEAL_SEAL | F_SEAL_EXEC | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

const NAME_PREFIX: &[u8] = b"memfd:";
/// `MFD_NAME_MAX_LEN` = NAME_MAX - strlen("memfd:"): the longest name a program may give (bytes, no NUL).
pub const MFD_NAME_MAX_LEN: usize = crate::abi::NAME_MAX - 6;

/// `vm.memfd_noexec` (linux-6.6/kernel/pid_sysctl.h): 0 = MFD_EXEC unless asked otherwise, 1 = MFD_NOEXEC_SEAL
/// unless asked otherwise, 2 = MFD_NOEXEC_SEAL required. One pid namespace in V2: one value.
const SCOPE_NOEXEC_SEAL: u64 = 1;
const SCOPE_NOEXEC_ENFORCED: u64 = 2;
static NOEXEC_SCOPE: AtomicU64 = AtomicU64::new(0);
static WARNED_NO_EXEC_FLAG: AtomicBool = AtomicBool::new(false);
static WARNED_HUGETLB: AtomicBool = AtomicBool::new(false);

klog::counter!(CREATED, "vfs.memfd.created");
klog::counter!(SEALED, "vfs.memfd.seals_added");

klog::counter!(pub(crate) REFUSED_WRITE, "vfs.memfd.refused_write");
klog::counter!(pub(crate) REFUSED_TRUNCATE, "vfs.memfd.refused_truncate");
klog::counter!(pub(crate) REFUSED_MAP, "vfs.memfd.refused_map");
klog::counter!(pub(crate) REFUSED_ADD, "vfs.memfd.refused_add_seals");
klog::counter!(pub(crate) REFUSED_FALLOCATE, "vfs.memfd.refused_fallocate");

/// What a seal refused (B6: without a serial port the log is the only way to see why a Wayland client's or
/// compositor's call failed on the Mini-PC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// write/pwrite/writev (F_SEAL_WRITE, F_SEAL_FUTURE_WRITE, F_SEAL_GROW).
    Write,
    /// ftruncate/truncate/O_TRUNC (F_SEAL_SHRINK, F_SEAL_GROW).
    Truncate,
    /// A new MAP_SHARED mapping (EPERM under a write seal).
    Map,
    /// F_ADD_SEALS itself (EPERM after F_SEAL_SEAL, EBUSY while a shared mapping may write).
    AddSeals,
    /// fallocate (a hole punch under a write seal, growth under F_SEAL_GROW).
    Fallocate,
}

/// Counts a refusal by the seals and logs it: every one counts (`vfs.memfd.refused_*`), the event comes for
/// the first 8 of each kind and then at each power of two (a program retrying in a loop cannot flood the
/// ring). Called with no lock of the file held.
pub fn seal_refused(what: Refused, ino: u64, seals: u32, errno: Errno) {
    let (counter, op) = match what {
        Refused::Write => (&REFUSED_WRITE, "write"),
        Refused::Truncate => (&REFUSED_TRUNCATE, "truncate"),
        Refused::Map => (&REFUSED_MAP, "map"),
        Refused::AddSeals => (&REFUSED_ADD, "add_seals"),
        Refused::Fallocate => (&REFUSED_FALLOCATE, "fallocate"),
    };
    let n = counter.get() + 1;
    counter.inc();
    if n <= 8 || n.is_power_of_two() {
        klog::event!(Info, "vfs", "memfd.seal_refused", op = op, ino = ino, seals = seals, errno = errno, count = n);
    }
}

/// `/proc/sys/vm/memfd_noexec` now.
pub fn noexec_scope() -> u64 {
    NOEXEC_SCOPE.load(Ordering::Relaxed)
}

/// A write of `/proc/sys/vm/memfd_noexec` (root only by the file's mode; `proc_dointvec_minmax` with 0..2: the
/// lower bound is the parent namespace's value, 0 for the only one V2 has).
pub fn set_noexec_scope(v: u64) -> Result<(), Errno> {
    if v > SCOPE_NOEXEC_ENFORCED {
        return Err(EINVAL);
    }
    let old = NOEXEC_SCOPE.swap(v, Ordering::Relaxed);
    if old != v {
        klog::event!(Info, "vfs", "memfd.noexec_scope", from = old, to = v);
    }
    Ok(())
}

struct ProcSysVm;

impl crate::synth::DirSource for ProcSysVm {
    fn entries(&self) -> Result<Vec<crate::synth::SynthEntry>, Errno> {
        use crate::synth::SynthEntry;
        Ok(alloc::vec![SynthEntry::file(
            "memfd_noexec",
            0o644,
            Arc::new(crate::sysctl::Knob { get: noexec_scope, set: set_noexec_scope, uint: true })
        )
        .cookie(1)])
    }
}

fn proc_sys_vm() -> Arc<dyn crate::synth::DirSource> {
    Arc::new(ProcSysVm)
}
crate::procdir!("/proc/sys/vm", proc_sys_vm);

static SHM: ksync::Once<Arc<Mount>> = ksync::Once::new();

/// The internal tmpfs every memfd lives on (Linux `shm_mnt`): mounted nowhere, kept for good. Its pool has no
/// size of its own, so memfds are bounded by the system pool only (as /dev/shm, R2).
pub fn shm_mount() -> Arc<Mount> {
    SHM.get_or_init(|| {
        let (sb, root) = crate::tmpfs::new_fs(0o1777);
        Mount::new(sb, root, None, false)
    })
    .clone()
}

/// The memfd itself (`shmem_file_setup(name, 0, VM_NORESERVE)` and the MFD_* seal set-up of `memfd_create`),
/// after memfd_create's checks: `name` is the program's (without the prefix), `flags` include what the sysctl
/// added. Mode 0777 (0666 with MFD_NOEXEC_SEAL; no umask, as shmem), owned by `cred`, nlink 0, O_RDWR.
pub fn create(name: &[u8], flags: u32, cred: Cred) -> Result<Arc<OpenFile>, Errno> {
    let mnt = shm_mount();
    let perm = if flags & MFD_NOEXEC_SEAL != 0 { 0o666 } else { 0o777 };
    let mut full = Vec::new();
    full.try_reserve_exact(NAME_PREFIX.len() + name.len()).map_err(|_| ENOMEM)?;
    full.extend_from_slice(NAME_PREFIX);
    full.extend_from_slice(name);
    let data = crate::kapi::reg_data(&mnt.sb);
    let d = mnt.root.nameless_child(full, uapi::S_IFREG | perm, cred, data)?;
    if let Some(c) = d.inode.page_cache() {
        if flags & MFD_NOEXEC_SEAL != 0 {
            c.set_initial_seals(F_SEAL_EXEC);
        } else if flags & MFD_ALLOW_SEALING != 0 {
            c.set_initial_seals(0);
        }
    }
    CREATED.inc();
    let path = Path { mnt, dentry: d };
    let file = crate::node::file_of(&path.dentry);
    Ok(OpenFile::with_path(file, crate::abi::O_RDWR, path))
}

/// `check_sysctl_memfd_noexec`: without MFD_EXEC or MFD_NOEXEC_SEAL the sysctl picks one; at scope 2 anything
/// but MFD_NOEXEC_SEAL is EACCES.
fn apply_noexec_scope(flags: &mut u32) -> Result<(), Errno> {
    let scope = noexec_scope();
    if *flags & (MFD_EXEC | MFD_NOEXEC_SEAL) == 0 {
        *flags |= if scope >= SCOPE_NOEXEC_SEAL { MFD_NOEXEC_SEAL } else { MFD_EXEC };
    }
    if *flags & MFD_NOEXEC_SEAL == 0 && scope >= SCOPE_NOEXEC_ENFORCED {
        klog::event!(Info, "vfs", "memfd.noexec_required", scope = scope; "memfd_create without MFD_NOEXEC_SEAL refused (vm.memfd_noexec=2)");
        return Err(EACCES);
    }
    Ok(())
}

/// memfd_create(name, flags) (linux-6.6/mm/memfd.c:285-375) in Linux's order: unknown flags EINVAL (huge page
/// size bits only with MFD_HUGETLB), MFD_EXEC with MFD_NOEXEC_SEAL EINVAL, the sysctl (EACCES), the name (a
/// fault EFAULT, more than MFD_NAME_MAX_LEN bytes EINVAL), the fd (EMFILE), then the file (ENOMEM). V2 has no
/// hugetlbfs: MFD_HUGETLB is ENOSYS there, as a Linux kernel without CONFIG_HUGETLBFS answers.
pub fn memfd_create(s: &Sys<'_>, a: Args) -> i64 {
    match do_create(s, a[0], a[1] as u32) {
        Ok(fd) => fd as i64,
        Err(e) => -e,
    }
}

fn do_create(s: &Sys<'_>, uname: u64, flags: u32) -> Result<usize, Errno> {
    let allowed = if flags & MFD_HUGETLB == 0 { MFD_ALL_FLAGS } else { MFD_ALL_FLAGS | (MFD_HUGE_MASK << MFD_HUGE_SHIFT) };
    if flags & !allowed != 0 {
        return Err(EINVAL);
    }
    if flags & MFD_EXEC != 0 && flags & MFD_NOEXEC_SEAL != 0 {
        return Err(EINVAL);
    }
    if flags & (MFD_EXEC | MFD_NOEXEC_SEAL) == 0 && !WARNED_NO_EXEC_FLAG.swap(true, Ordering::Relaxed) {
        // Linux's pr_warn_once: the default (executable) may change; programs should say which they want.
        klog::event!(Info, "vfs", "memfd.no_exec_flag"; "memfd_create called without MFD_EXEC or MFD_NOEXEC_SEAL (once)");
    }
    let mut flags = flags;
    apply_noexec_scope(&mut flags)?;
    // strnlen_user(uname, MFD_NAME_MAX_LEN + 1): no NUL within it is EINVAL, a fault EFAULT.
    let name = match s.mem.read_cstr(uname, MFD_NAME_MAX_LEN + 1) {
        Ok(n) => n,
        Err(ENAMETOOLONG) => return Err(EINVAL),
        Err(_) => return Err(EFAULT),
    };
    let fd = s.fds.reserve(0, flags & MFD_CLOEXEC != 0)?;
    let made = if flags & MFD_HUGETLB != 0 {
        if !WARNED_HUGETLB.swap(true, Ordering::Relaxed) {
            klog::event!(Warn, "vfs", "memfd.hugetlb"; "memfd_create(MFD_HUGETLB) answers ENOSYS: V2 has no hugetlbfs (once)");
        }
        Err(ENOSYS)
    } else {
        create(&name, flags, s.cred)
    };
    match made {
        Ok(f) => {
            s.fds.commit(fd, f);
            Ok(fd)
        }
        Err(e) => {
            s.fds.unreserve(fd);
            Err(e)
        }
    }
}

/// The seal commands of fcntl (`memfd_fcntl`, `memfd_add_seals`, `memfd_get_seals`; the int argument as
/// unsigned, as `do_fcntl` passes it). F_ADD_SEALS: EPERM on an fd not open for writing, EINVAL for unknown
/// seal bits or a file without seals, then the file's rules (`PageCache::add_seals`). F_GET_SEALS: the seals,
/// EINVAL for a file without them. The caller already refused O_PATH fds (EBADF).
pub fn seal_fcntl(f: &OpenFile, cmd: u64, arg: u64) -> Result<i64, Errno> {
    let seals = arg as u32;
    let cache = f.inode().and_then(|i| i.page_cache());
    match cmd {
        F_ADD_SEALS => {
            if !f.can_write() {
                return Err(EPERM);
            }
            if seals & !F_ALL_SEALS != 0 {
                return Err(EINVAL);
            }
            let (Some(c), Some(i)) = (cache, f.inode()) else { return Err(EINVAL) };
            let now = c.add_seals(seals, i.meta.mode() & 0o111 != 0).inspect_err(|&e| {
                if e != EINVAL {
                    seal_refused(Refused::AddSeals, i.meta.ino, c.seals().unwrap_or(0), e);
                }
            })?;
            SEALED.inc();
            klog::event!(Debug, "vfs", "memfd.sealed", ino = i.meta.ino, added = seals, seals = now);
            Ok(0)
        }
        F_GET_SEALS => cache.and_then(|c| c.seals()).map(|s| s as i64).ok_or(EINVAL),
        _ => Err(EINVAL),
    }
}

/// F_SEAL_EXEC's rule for a mode change (`shmem_setattr`, linux-6.6/mm/shmem.c:1166): changing an exec bit of
/// a file sealed so is EPERM. For the chmod family when it arrives (V2 has none yet; FS-10 open point).
pub fn check_mode_change(inode: &Inode, new_mode: u32) -> Result<(), Errno> {
    let sealed = inode.page_cache().and_then(|c| c.seals()).is_some_and(|s| s & F_SEAL_EXEC != 0);
    if sealed && (inode.meta.mode() ^ new_mode) & 0o111 != 0 {
        return Err(EPERM);
    }
    Ok(())
}
