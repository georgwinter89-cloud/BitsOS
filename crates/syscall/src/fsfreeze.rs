//! FIFREEZE / FITHAW on any open file (linux-6.6/fs/ioctl.c:387 `ioctl_fsfreeze`, :404 `ioctl_fsthaw`; VS-2 CR5):
//! the file's file system is frozen for a consistent copy (the snapshot tools' `fsfreeze`). EPERM without
//! CAP_SYS_ADMIN, EOPNOTSUPP for a file system without a freeze of its own (tmpfs, FAT, a pipe), EBUSY when it
//! is frozen already (by a program or by a VS-2 snapshot), and FITHAW of a file system no program froze is
//! EINVAL (fs/super.c:2053), so a program can never thaw a snapshot's freeze. The argument is not read.

use crate::SysCtx;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::FsFreeze;
use vfs::dentry::SuperBlock;
use vfs::fd::OpenFile;

/// `_IOWR('X', 119, int)` and `_IOWR('X', 120, int)` (include/uapi/linux/fs.h).
pub const FIFREEZE: u64 = 0xC004_5877;
pub const FITHAW: u64 = 0xC004_5878;

/// File systems frozen by FIFREEZE (Linux's FREEZE_HOLDER_USERSPACE); held until FITHAW.
static BY_PROGRAMS: ksync::Mutex<Vec<Arc<SuperBlock>>> = ksync::Mutex::named("syscall.fsfreeze", Vec::new());

fn sb_of(f: &OpenFile) -> Option<Arc<SuperBlock>> {
    f.path.as_ref().map(|p| p.dentry.sb.clone())
}

pub fn freeze(c: &SysCtx<'_>, f: &OpenFile) -> i64 {
    if !crate::uts::sys_admin(c) {
        return -EPERM;
    }
    let Some(sb) = sb_of(f).filter(|sb| sb.ops.as_ref().is_some_and(|o| o.can_freeze())) else {
        return -EOPNOTSUPP;
    };
    // The freeze itself runs without the list's lock (it may wait for writers); a second FIFREEZE meanwhile
    // gets EBUSY from the freeze, a FITHAW meanwhile EINVAL (not frozen yet), as on Linux.
    if let Err(e) = sb.freeze() {
        return -e;
    }
    klog::event!(Info, "syscall", "fifreeze", pid = c.proc.pid, fs = sb.fstype, dev = sb.dev);
    BY_PROGRAMS.lock().push(sb);
    0
}

pub fn thaw(c: &SysCtx<'_>, f: &OpenFile) -> i64 {
    if !crate::uts::sys_admin(c) {
        return -EPERM;
    }
    let Some(sb) = sb_of(f) else {
        return -EINVAL;
    };
    let mine = {
        let mut held = BY_PROGRAMS.lock();
        held.iter().position(|h| Arc::ptr_eq(h, &sb)).map(|i| held.swap_remove(i))
    };
    if mine.is_none() {
        return -EINVAL;
    }
    sb.thaw();
    klog::event!(Info, "syscall", "fithaw", pid = c.proc.pid, fs = sb.fstype, dev = sb.dev);
    0
}
