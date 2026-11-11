//! SX-IPC: `/proc/sys/kernel/{sem,shmmax,shmall,shmmni,shm_rmid_forced,msgmax,msgmnb,msgmni,auto_msgmni,
//! *_next_id}` per IPC namespace (linux-7.3.1/ipc/ipc_sysctl.c) and `/proc/sysvipc/{sem,shm,msg}`. A second source
//! beside `sysctl.rs` on the same directory. Every file belongs to the reader's namespace, as Linux's `set_lookup`.

use super::attr::Live;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};
use proc::cred::{self, Cap};
use proc::ipc::ids::IPC_MNI;
use proc::ipc::{Tables, shm, text};
use proc::ns::misc::{self, IpcNs};
use proc::ns::user::MapKind;
use uapi::ab::sysv::SC_SEMMNI;
use uapi::errno::*;
use vfs::inode::{MAY_ACCESS, MAY_WRITE};
use vfs::synth::{DirSource, FileSource, SynthEntry};
use vfs::{Cred, Errno};

const INT_MAX: u64 = i32::MAX as u64;

fn reader_ns() -> Arc<IpcNs> {
    proc::current().map_or_else(|| misc::init_ipc().clone(), |p| p.nsset().ipc.clone())
}

/// `ipc_set_ownership`: the namespace's root, or the global one when it has none.
fn ns_root(ns: &IpcNs) -> (u32, u32) {
    (ns.user_ns.to_kernel(MapKind::Uid, 0).unwrap_or(0), ns.user_ns.to_kernel(MapKind::Gid, 0).unwrap_or(0))
}

/// One number or row of numbers.
struct Knob {
    read: fn(&Tables) -> String,
    write: fn(&Arc<IpcNs>, &[u8]) -> Result<(), Errno>,
    /// `*_next_id`: 0444, but open to CAP_CHECKPOINT_RESTORE or CAP_SYS_ADMIN over the namespace (`ipc_permissions`).
    next_id: bool,
}

impl Knob {
    /// `ipc_permissions` with `test_perm` (ipc_sysctl.c:244): the mode the namespace's root gets, or the world's.
    /// `access`: access(2), which asks with the real uid.
    fn may_write(&self, ns: &IpcNs, access: bool) -> bool {
        let Some(me) = proc::current() else { return true };
        let c = me.cred();
        if self.next_id {
            return cred::ns_capable(&c, &ns.user_ns, Cap::CheckpointRestore) || cred::ns_capable(&c, &ns.user_ns, Cap::SysAdmin);
        }
        (if access { c.uid } else { c.euid }) == ns_root(ns).0
    }
}

impl FileSource for Knob {
    fn permission(&self, _who: Cred, want: u32) -> Option<Result<(), Errno>> {
        use vfs::inode::MAY_EXEC;
        let denied = want & MAY_EXEC != 0 || (want & MAY_WRITE != 0 && !self.may_write(&reader_ns(), want & MAY_ACCESS != 0));
        Some(if denied { Err(EACCES) } else { Ok(()) })
    }
    fn may_open(&self, _who: Cred, write: bool) -> Result<(), Errno> {
        if write && !self.may_write(&reader_ns(), false) {
            return Err(EACCES);
        }
        Ok(())
    }
    fn render(&self, _who: Cred) -> Result<Vec<u8>, Errno> {
        Ok((self.read)(&reader_ns().tables).into_bytes())
    }
    fn write(&self, who: Cred, data: &[u8]) -> Result<usize, Errno> {
        self.write_at(who, 0, data)
    }
    /// A write past the start changes nothing and counts as written (SYSCTL_WRITES_STRICT).
    fn write_at(&self, _who: Cred, pos: u64, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() || pos != 0 {
            return Ok(data.len());
        }
        (self.write)(&reader_ns(), data)?;
        Ok(data.len())
    }
}

/// `proc_dointvec_minmax` on one int: only white space or a value outside the range is EINVAL.
fn int_in(data: &[u8], max: u64) -> Result<i32, Errno> {
    let v = vfs::sysctl::parse_ulong(data)?.ok_or(EINVAL)?;
    if v > max {
        return Err(EINVAL);
    }
    Ok(v as i32)
}

fn int_text(a: &AtomicI32) -> String {
    alloc::format!("{}\n", a.load(Ordering::Relaxed))
}

/// `proc_dointvec`'s numbers: each may have a sign, separated by white space, at most `sem_ctls`' four.
fn sem_values(data: &[u8]) -> Result<Vec<i32>, Errno> {
    let mut out = Vec::new();
    for tok in data.split(|c| matches!(c, b' ' | b'\t' | b'\n')).filter(|t| !t.is_empty()).take(4) {
        let (neg, digits) = match tok.first() {
            Some(b'-') => (true, &tok[1..]),
            _ => (false, tok),
        };
        let v = vfs::sysctl::parse_ulong(digits)?.ok_or(EINVAL)?;
        if v > if neg { INT_MAX + 1 } else { INT_MAX } {
            return Err(EINVAL);
        }
        out.try_reserve(1).map_err(|_| ENOMEM)?;
        out.push(if neg { (v as i64).wrapping_neg() as i32 } else { v as i32 });
    }
    if out.is_empty() {
        return Err(EINVAL);
    }
    Ok(out)
}

/// `proc_ipc_sem_dointvec`: what was given is stored; semmni out of 0..=IPCMNI is ERANGE and goes back alone.
fn write_sem(ns: &Arc<IpcNs>, data: &[u8]) -> Result<(), Errno> {
    let l = &ns.tables.limits;
    let old = l.sem[SC_SEMMNI].load(Ordering::Relaxed);
    for (i, v) in sem_values(data)?.iter().enumerate() {
        l.sem[i].store(*v, Ordering::Relaxed);
    }
    let mni = l.sem[SC_SEMMNI].load(Ordering::Relaxed);
    if !(0..=IPC_MNI).contains(&mni) {
        l.sem[SC_SEMMNI].store(old, Ordering::Relaxed);
        return Err(ERANGE);
    }
    Ok(())
}

fn write_shm_rmid_forced(ns: &Arc<IpcNs>, data: &[u8]) -> Result<(), Errno> {
    let v = int_in(data, 1)?;
    ns.tables.limits.shm_rmid_forced.store(v, Ordering::Relaxed);
    if v != 0 {
        shm::destroy_orphaned(ns);
    }
    Ok(())
}

/// `proc_doulongvec_minmax`: only white space changes nothing.
fn write_ulong(data: &[u8], store: impl FnOnce(u64)) -> Result<(), Errno> {
    if let Some(v) = vfs::sysctl::parse_ulong(data)? {
        store(v);
    }
    Ok(())
}

fn knobs() -> [(&'static str, u32, Knob); 12] {
    let k = |read, write| Knob { read, write, next_id: false };
    let id = |read, write| Knob { read, write, next_id: true };
    [
        ("auto_msgmni", 0o644, k(|_| String::from("0\n"), |_, d| int_in(d, 1).map(|_| ()))),
        ("msg_next_id", 0o444, id(|t| alloc::format!("{}\n", t.msg.ids.read().next_id), |n, d| int_in(d, INT_MAX).map(|v| n.tables.msg.ids.write().next_id = v))),
        ("msgmax", 0o644, k(|t| int_text(&t.limits.msgmax), |n, d| int_in(d, INT_MAX).map(|v| n.tables.limits.msgmax.store(v, Ordering::Relaxed)))),
        ("msgmnb", 0o644, k(|t| int_text(&t.limits.msgmnb), |n, d| int_in(d, INT_MAX).map(|v| n.tables.limits.msgmnb.store(v, Ordering::Relaxed)))),
        ("msgmni", 0o644, k(|t| int_text(&t.limits.msgmni), |n, d| int_in(d, IPC_MNI as u64).map(|v| n.tables.limits.msgmni.store(v, Ordering::Relaxed)))),
        (
            "sem",
            0o644,
            k(
                |t| {
                    let s = |i: usize| t.limits.sem[i].load(Ordering::Relaxed);
                    alloc::format!("{}\t{}\t{}\t{}\n", s(0), s(1), s(2), s(3))
                },
                write_sem,
            ),
        ),
        ("sem_next_id", 0o444, id(|t| alloc::format!("{}\n", t.sem.ids.read().next_id), |n, d| int_in(d, INT_MAX).map(|v| n.tables.sem.ids.write().next_id = v))),
        ("shm_next_id", 0o444, id(|t| alloc::format!("{}\n", t.shm.ids.read().next_id), |n, d| int_in(d, INT_MAX).map(|v| n.tables.shm.ids.write().next_id = v))),
        ("shm_rmid_forced", 0o644, k(|t| int_text(&t.limits.shm_rmid_forced), write_shm_rmid_forced)),
        ("shmall", 0o644, k(|t| alloc::format!("{}\n", t.limits.shmall.load(Ordering::Relaxed)), |n, d| write_ulong(d, |v| n.tables.limits.shmall.store(v, Ordering::Relaxed)))),
        ("shmmax", 0o644, k(|t| alloc::format!("{}\n", t.limits.shmmax.load(Ordering::Relaxed)), |n, d| write_ulong(d, |v| n.tables.limits.shmmax.store(v, Ordering::Relaxed)))),
        ("shmmni", 0o644, k(|t| int_text(&t.limits.shmmni), |n, d| int_in(d, IPC_MNI as u64).map(|v| n.tables.limits.shmmni.store(v, Ordering::Relaxed)))),
    ]
}

struct KernelIpcDir;

impl DirSource for KernelIpcDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        let (uid, gid) = ns_root(&reader_ns());
        let mut v = Vec::new();
        for (i, (name, perm, knob)) in knobs().into_iter().enumerate() {
            v.push(SynthEntry::file(name, perm, Arc::new(knob)).owner(uid, gid).cookie(i as u64 + 100));
        }
        Ok(v)
    }
}

fn kernel_ipc_dir() -> Arc<dyn DirSource> {
    Arc::new(KernelIpcDir)
}
vfs::procdir!("/proc/sys/kernel", kernel_ipc_dir);

/// What the reader sees of the namespace; a kernel thread reads as the initial namespace's root.
fn view<R>(f: impl FnOnce(&text::View<'_>) -> R) -> R {
    match proc::current() {
        Some(p) => {
            let (ns, cred) = (p.nsset().ipc.clone(), p.cred());
            f(&text::View { ns: &ns, user_ns: &cred.user_ns, vnr: &|id| p.vnr(id) })
        }
        None => f(&text::View { ns: misc::init_ipc(), user_ns: proc::ns::user::init_user_ns(), vnr: &|id| id }),
    }
}

struct SysvipcDir;

impl DirSource for SysvipcDir {
    fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
        Ok(alloc::vec![
            SynthEntry::file("msg", 0o444, Arc::new(Live::new(0, || Ok(view(text::msg))))).cookie(1),
            SynthEntry::file("sem", 0o444, Arc::new(Live::new(0, || Ok(view(text::sem))))).cookie(2),
            SynthEntry::file("shm", 0o444, Arc::new(Live::new(0, || Ok(view(text::shm))))).cookie(3),
        ])
    }
}

fn sysvipc_dir() -> Arc<dyn DirSource> {
    Arc::new(SysvipcDir)
}
vfs::procdir!("/proc/sysvipc", sysvipc_dir);
