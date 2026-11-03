//! SX-3a: mount(2) (165), umount2(2) (166) and pivot_root(2) (155) for programs. The calls are
//! `vfs::sys_mount`'s; this file answers its questions from the calling process (`MountPerms`): capabilities in
//! the user namespace that owns the caller's mount namespace or the new file system, the caller's ids and id
//! maps, and `chroot_fs_refs` over every process.

use crate::{ProcMem, mount_view};
use alloc::sync::Arc;
use proc::Process;
use proc::cred::{self, Cap, Cred};
use proc::ns::user::{MapKind, UserNs};
use syscall::SysCtx;
use vfs::mount_fs::Mounter;
use vfs::sys::{Args, Sys};
use vfs::sys_mount::{self, MountPerms};
use vfs::{Errno, Path, SuperBlock};

struct Perms<'a> {
    p: &'a Process,
    c: Arc<Cred>,
}

impl Perms<'_> {
    /// The user namespace that owns a new `fstype` mount (`fc->user_ns`).
    fn fs_ns(&self, fstype: &str) -> Arc<UserNs> {
        let set = self.p.nsset();
        match fstype {
            "proc" => self.p.pid_ns.user_ns.clone(),
            "sysfs" => set.net.user_ns.clone(),
            "mqueue" => set.ipc.user_ns.clone(),
            _ => self.c.user_ns.clone(),
        }
    }
}

impl MountPerms for Perms<'_> {
    fn may_mount(&self) -> bool {
        cred::ns_capable(&self.c, &self.p.mnt_ns().user_ns, Cap::SysAdmin)
    }
    fn capable_init(&self) -> bool {
        cred::capable(&self.c, Cap::SysAdmin)
    }
    fn may_mount_fs(&self, fstype: &str) -> bool {
        cred::ns_capable(&self.c, &self.fs_ns(fstype), Cap::SysAdmin)
    }
    fn fs_owner(&self, fstype: &str) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        let ns = self.fs_ns(fstype);
        (!ns.is_init()).then(|| ns as Arc<dyn core::any::Any + Send + Sync>)
    }
    fn admin_over(&self, sb: &SuperBlock) -> bool {
        match sb.owner().and_then(|o| o.downcast::<UserNs>().ok()) {
            Some(ns) => cred::ns_capable(&self.c, &ns, Cap::SysAdmin),
            None => cred::capable(&self.c, Cap::SysAdmin),
        }
    }
    fn ns_owned_by_init(&self) -> bool {
        self.p.mnt_ns().user_ns.is_init()
    }
    fn with_mounter(&self, f: &mut dyn FnMut(&Mounter<'_>) -> Result<(), Errno>) -> Result<(), Errno> {
        let ns = self.c.user_ns.clone();
        let uid_in = |n: u32| ns.to_kernel(MapKind::Uid, n);
        let gid_in = |n: u32| ns.to_kernel(MapKind::Gid, n);
        let root = ns.to_kernel(MapKind::Uid, 0).zip(ns.to_kernel(MapKind::Gid, 0));
        let fs = self.p.fs_cred();
        f(&Mounter { fsuid: fs.uid, fsgid: fs.gid, uid_in: &uid_in, gid_in: &gid_in, init_ns: ns.is_init(), ns_root: root })
    }
    fn ptmx(&self) -> Option<Arc<dyn vfs::File>> {
        let p = vfs::root_ns().lookup_path(b"/dev/ptmx", true).ok()?;
        match &p.dentry.inode.data {
            vfs::inode::Data::Device(f) => Some(f.clone()),
            _ => None,
        }
    }
    fn ram_bytes(&self) -> u64 {
        mm_core::frames().map_or(0, |f| f.stats().total_bytes)
    }
    fn chroot_fs_refs(&self, old: &Path, new: &Path) {
        let n = proc::ns::mnt::chroot_fs_refs(old, new);
        klog::event!(Debug, "vfs", "pivot_root.refs", pid = self.p.pid, moved = n);
    }
    fn pid_ns(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        Some(self.p.pid_ns.clone())
    }
}

fn call(c: &mut SysCtx<'_>, f: fn(&Sys<'_>, Args, &dyn MountPerms) -> i64) -> i64 {
    let p = c.proc;
    let mem = ProcMem(p);
    let fs = c.thread.fs(p);
    let ns = mount_view(p);
    let s = Sys { mem: &mem, fds: &p.fds, fs: &fs, ns: &ns, cred: p.fs_cred(), subject: p.subject() };
    let perms = Perms { p, c: p.cred() };
    f(&s, c.args, &perms)
}

macro_rules! mount_call {
    ($nr:expr, $name:literal, $f:path) => {
        const _: () = {
            fn h(c: &mut SysCtx<'_>) -> i64 {
                call(c, $f)
            }
            syscall::__kreg::register!("syscall", syscall::SyscallEntry, syscall::SyscallEntry { nr: $nr, name: $name, handler: h });
        };
    };
}

mount_call!(uapi::nr::MOUNT, "mount", sys_mount::mount);
mount_call!(uapi::nr::UMOUNT2, "umount2", sys_mount::umount2);
mount_call!(uapi::nr::PIVOT_ROOT, "pivot_root", sys_mount::pivot_root);
