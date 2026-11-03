//! process_vm_readv(2) and process_vm_writev(2) (AB-17; linux-6.6/mm/process_vm_access.c `process_vm_rw` :253,
//! `process_vm_rw_core` :150, `process_vm_rw_single_vec` :60). Wine's server reads and writes its clients'
//! memory this way and falls back to ptrace + /proc/pid/mem only on ENOSYS/EPERM, so the errors are Linux's
//! in Linux's order:
//! 1. flags != 0: EINVAL;
//! 2. the local iovecs (`import_iovec`: count as an unsigned int, EINVAL above UIO_MAXIOV or for a negative
//!    length, EFAULT for an unreadable array or a segment that fails `access_ok`, the total clipped to
//!    MAX_RW_COUNT); a local total of 0 answers 0 before anything else is looked at;
//! 3. the remote iovecs (`iovec_from_user`: no `access_ok`, no clip); no remote byte asked for: 0;
//! 4. the pid in the caller's pid namespace (a thread id works too): ESRCH, also for a process whose memory
//!    is gone (`get_task_mm` NULL: it ended);
//! 5. `mm_access(PTRACE_MODE_ATTACH_REALCREDS)`: the caller's own memory needs no check, else
//!    `Process::may_ptrace(target, false)`, EPERM (Linux maps EACCES to EPERM).
//!
//! The copy goes through a page-sized kernel buffer in pieces that never cross a page edge on either side, so
//! no two address-space locks are held at once (the same address space on both sides is fine) and a fault
//! stops exactly where Linux's page pinning stops. Bytes moved before a fault are the answer; with none, the
//! fault is (EFAULT). The remote write is a plain `copy_to_user` (FOLL_WRITE, never FOLL_FORCE: a read-only
//! remote page is EFAULT and a private file mapping gets its own copy-on-write page, never the page cache).

use crate::SysCtx;
use alloc::vec::Vec;
use mm::AddressSpace;
use uapi::errno::*;

const PAGE: u64 = 4096;
/// `UIO_MAXIOV` (linux-6.6/include/uapi/linux/uio.h:47).
const UIO_MAXIOV: u64 = 1024;
/// `MAX_RW_COUNT` (INT_MAX & PAGE_MASK).
const MAX_RW_COUNT: u64 = 0x7fff_f000;
/// Pieces between two looks at a kill (Linux's `pin_user_pages_remote` notices a fatal signal per batch).
const KILL_CHECK_EVERY: u32 = 16;

fn sys_process_vm_readv(c: &mut SysCtx<'_>) -> i64 {
    process_vm_rw(c, false)
}
crate::syscall_nr!(uapi::ab::nr_more::PROCESS_VM_READV, "process_vm_readv", sys_process_vm_readv);

fn sys_process_vm_writev(c: &mut SysCtx<'_>) -> i64 {
    process_vm_rw(c, true)
}
crate::syscall_nr!(uapi::ab::nr_more::PROCESS_VM_WRITEV, "process_vm_writev", sys_process_vm_writev);

fn process_vm_rw(c: &mut SysCtx<'_>, write: bool) -> i64 {
    let [pid, lvec, liovcnt, rvec, riovcnt, flags] = c.args;
    if flags != 0 {
        return -EINVAL;
    }
    // `import_iovec` takes the count as an unsigned int.
    let local = match import_iovec(&c.proc.mm, lvec, liovcnt as u32 as u64) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if local.iter().all(|&(_, l)| l == 0) {
        return 0;
    }
    let remote = match iovec_from_user(&c.proc.mm, rvec, riovcnt) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if remote.iter().all(|&(_, l)| l == 0) {
        return 0;
    }
    let pid = pid as i32;
    let Some(t) = (pid > 0).then(|| crate::ns_sys::find_thread(c, pid as u32)).flatten() else { return -ESRCH };
    let Some(target) = t.group().image() else { return -ESRCH };
    // `mmget` before the look at the end: memory pinned now stays until the copy is done.
    let _pin = proc::pin_mm(&target);
    if target.is_finished() {
        return -ESRCH;
    }
    let same_mm = alloc::sync::Arc::ptr_eq(&target.mm, &c.proc.mm);
    if !same_mm && !crate::pidfd::may_attach(c.proc, &target) {
        klog::event!(Info, "syscall", "process_vm.refused", pid = c.proc.pid, uid = c.proc.cred().uid, target = target.pid, target_uid = target.cred().uid, write = write);
        return -EPERM;
    }
    let mut buf = Vec::new();
    if buf.try_reserve_exact(PAGE as usize).is_err() {
        return -ENOMEM;
    }
    buf.resize(PAGE as usize, 0u8);
    let r = copy(c, &target.mm, &local, &remote, &mut buf, write);
    // Written bytes must not linger in the kernel heap.
    buf.fill(0);
    r
}

/// Moves bytes between the caller's `local` and the target's `remote` segments, in order, until either side
/// is used up or a page fails. Answers the bytes moved, or -EFAULT when the first piece failed.
fn copy(c: &SysCtx<'_>, rmm: &AddressSpace, local: &[(u64, u64)], remote: &[(u64, u64)], buf: &mut [u8], write: bool) -> i64 {
    let lmm = &c.proc.mm;
    let (mut li, mut loff) = (0usize, 0u64);
    let mut done: u64 = 0;
    let mut pieces = 0u32;
    for &(rbase, rlen) in remote {
        let mut roff = 0u64;
        while roff < rlen {
            while li < local.len() && loff == local[li].1 {
                li += 1;
                loff = 0;
            }
            if li == local.len() {
                return done as i64;
            }
            let (ra, la) = (rbase.wrapping_add(roff), local[li].0.wrapping_add(loff));
            let n = (rlen - roff).min(local[li].1 - loff).min(PAGE - (ra & (PAGE - 1))).min(PAGE - (la & (PAGE - 1)));
            let b = &mut buf[..n as usize];
            let ok = if write {
                lmm.copy_from_user(b, la).is_ok() && rmm.copy_to_user(ra, b).is_ok()
            } else {
                rmm.copy_from_user(b, ra).is_ok() && lmm.copy_to_user(la, b).is_ok()
            };
            if !ok {
                klog::event!(Debug, "syscall", "process_vm.fault", pid = c.proc.pid, remote = ra, local = la, done = done, write = write);
                return if done > 0 { done as i64 } else { -EFAULT };
            }
            done += n;
            roff += n;
            loff += n;
            pieces += 1;
            if pieces % KILL_CHECK_EVERY == 0 {
                if c.proc.has_exited() {
                    return done as i64;
                }
                sched::cond_resched();
            }
        }
    }
    done as i64
}

/// `import_iovec` (linux-6.6/lib/iov_iter.c:1456): one segment is clipped before its `access_ok`, several
/// are checked whole and then clipped so the total stays within MAX_RW_COUNT.
fn import_iovec(mm: &AddressSpace, uvec: u64, n: u64) -> Result<Vec<(u64, u64)>, i64> {
    if n == 1 {
        let mut v = copy_iovecs(mm, uvec, 1)?;
        v[0].1 = v[0].1.min(MAX_RW_COUNT);
        if !crate::info::access_ok(v[0].0, v[0].1) {
            return Err(-EFAULT);
        }
        return Ok(v);
    }
    let mut v = iovec_from_user(mm, uvec, n)?;
    let mut total = 0u64;
    for (base, len) in v.iter_mut() {
        if !crate::info::access_ok(*base, *len) {
            return Err(-EFAULT);
        }
        *len = (*len).min(MAX_RW_COUNT - total);
        total += *len;
    }
    Ok(v)
}

/// `iovec_from_user` (linux-6.6/lib/iov_iter.c:1397): 0 segments are none, more than UIO_MAXIOV EINVAL.
pub(crate) fn iovec_from_user(mm: &AddressSpace, uvec: u64, n: u64) -> Result<Vec<(u64, u64)>, i64> {
    if n == 0 {
        return Ok(Vec::new());
    }
    if n > UIO_MAXIOV {
        return Err(-EINVAL);
    }
    copy_iovecs(mm, uvec, n)
}

/// `copy_iovec_from_user` (:1365): `access_ok` on the whole array, then element by element, so a negative
/// length before an unreadable element is EINVAL and an unreadable element before it EFAULT.
fn copy_iovecs(mm: &AddressSpace, uvec: u64, n: u64) -> Result<Vec<(u64, u64)>, i64> {
    let bytes = n * 16;
    if !crate::info::access_ok(uvec, bytes) {
        return Err(-EFAULT);
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(bytes as usize).map_err(|_| -ENOMEM)?;
    raw.resize(bytes as usize, 0u8);
    // Usually the whole array reads at once; after a fault, element by element to find which comes first.
    let whole = mm.copy_from_user(&mut raw, uvec).is_ok();
    let mut out = Vec::new();
    out.try_reserve_exact(n as usize).map_err(|_| -ENOMEM)?;
    for (i, e) in raw.chunks_exact_mut(16).enumerate() {
        if !whole && mm.copy_from_user(e, uvec + i as u64 * 16).is_err() {
            return Err(-EFAULT);
        }
        let base = u64::from_le_bytes(e[0..8].try_into().unwrap_or([0; 8]));
        let len = u64::from_le_bytes(e[8..16].try_into().unwrap_or([0; 8]));
        if (len as i64) < 0 {
            return Err(-EINVAL);
        }
        out.push((base, len));
    }
    Ok(out)
}
