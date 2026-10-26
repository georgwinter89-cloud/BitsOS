//! Files (owner: FS; contracts `C: Pollable`, `B2 MountNs`, `B1 FsFreeze`, registries `devnode!`, `procfile!`).
//!
//! FS-1: the VFS core. Inodes (`inode`), dentries with the name cache and super blocks (`dentry`), mounts and
//! per-process mount views (`mount_ns`, B2), the path walk with Linux's rules (`walk`), name operations with
//! Linux's error order and the B3 policy hook (`ops`), open files and dynamic fd tables (`fd`), tmpfs as the
//! root (`tmpfs`), the initramfs unpacked into it at boot (`cpio`), and the file syscalls on an abstract
//! user memory (`sys`, registered by the `vfs-sys` glue crate). Freezing (B1) is per super block.
//! FS-2: devfs (`/dev` with null, zero, full, random, urandom, console, tty and every `devnode!`), the
//! synthetic file systems `/proc` and `/sys` (`synth`: `procfile!` texts and `procdir!` computed directories
//! whose content providers belong to their owners), the ChaCha20 generator (`random`) and the console hook for
//! IN (`tty`). FS-3: pipes and FIFOs (`pipe`), eventfd (`eventfd`), mknod's socket names and device numbers
//! (`special`). FS-4 epoll on `Pollable`, FS-5 the page cache, FS-10 memfd and file seals (`memfd`), FS-11 per-program views.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod abi;
pub mod cpio;
pub mod dentry;
pub mod devfs;
pub mod epoll;
pub mod eventfd;
pub mod fd;
pub mod frame;
pub mod fsops;
pub mod index;
pub mod inotify;
pub mod inode;
mod kapi;
pub mod memfd;
mod ktests;
mod pm_cost_ktest;
mod fs5_ktests;
pub mod mmap;
pub mod mount_ns;
pub mod node;
pub mod ops;
pub mod pagecache;
pub mod pipe;
pub mod poll;
pub mod pollsys;
pub mod pool;
pub mod preload;
pub mod random;
pub mod readahead;
pub mod reclaim;
pub mod setattr;
pub mod special;
pub mod statfs;
pub mod synth;
pub mod sys;
pub mod sys_attr;
pub mod sys_copy;
pub mod sysctl;
pub mod tmpfs;
pub mod tty;
pub mod walk;
pub mod writeback;

#[cfg(test)]
mod cases_test;
#[cfg(test)]
mod fs1b_tests;
#[cfg(test)]
mod fixer_tests;
#[cfg(test)]
mod more_tests;
#[cfg(test)]
mod fs2_tests;
#[cfg(test)]
mod fs5_tests;
#[cfg(test)]
mod fs12_tests;
#[cfg(test)]
mod fs14b_tests;
#[cfg(test)]
mod fs8c_tests;
#[cfg(test)]
mod fs3_tests;
#[cfg(test)]
mod fs4_tests;
#[cfg(test)]
mod fs10_tests;
#[cfg(test)]
mod fs13_tests;
#[cfg(test)]
mod fs13_preload_tests;
#[cfg(test)]
mod mm9_reclaim_tests;

pub use dentry::{Dentry, SuperBlock};
pub use fsops::FsOps;
pub use fd::{FdTable, FsState, OpenFile, OpenFlags};
pub use inode::{Cred, Inode};
pub use mount_ns::{MNT_NOATIME, MNT_NODEV, MNT_NODIRATIME, MNT_NOEXEC, MNT_NOSUID, MNT_NOSYMFOLLOW, MNT_RELATIME, Mount, Path};
pub use node::{File, MountNs, NodeFile};
pub use ops::{Ctx, OwnFolders, set_own_folders};
pub use poll::{Events, Pollable};

/// The G0 name of a tree node: a dentry.
pub type Node = Dentry;

use alloc::sync::Arc;

/// Errors are Linux errno values (positive); syscalls return them negated.
pub type Errno = i64;

/// A device node registered by its owner crate: `vfs::devnode!("/dev/null", make_null);`
pub struct DevNodeEntry {
    pub path: &'static str,
    pub make: fn() -> Arc<dyn File>,
}
kreg::declare!(pub DEVNODES: DevNodeEntry = "devnode");

/// A synthetic /proc or /sys file: `vfs::procfile!("/proc/meminfo", render_meminfo);` (text made per open; the
/// path may be anywhere, `/proc/...` and `/sys/...` land on those file systems).
pub struct ProcFileEntry {
    pub path: &'static str,
    pub render: fn() -> alloc::string::String,
}
kreg::declare!(pub PROCFILES: ProcFileEntry = "procfile");

/// A computed directory of `/proc` or `/sys`, attached at `path` (made when missing):
/// `vfs::procdir!("/proc", pid_dirs);` with `fn pid_dirs() -> Arc<dyn vfs::synth::DirSource>`. Several sources
/// on one directory are merged (see `synth`).
pub struct ProcDirEntry {
    pub path: &'static str,
    pub make: fn() -> Arc<dyn synth::DirSource>,
}
kreg::declare!(pub PROCDIRS: ProcDirEntry = "procdir");

#[doc(hidden)]
pub use kreg as __kreg;

#[macro_export]
macro_rules! devnode {
    ($path:literal, $make:path) => {
        $crate::__kreg::register!("devnode", $crate::DevNodeEntry, $crate::DevNodeEntry { path: $path, make: $make });
    };
}
#[macro_export]
macro_rules! procdir {
    ($path:literal, $make:path) => {
        $crate::__kreg::register!("procdir", $crate::ProcDirEntry, $crate::ProcDirEntry { path: $path, make: $make });
    };
}
#[macro_export]
macro_rules! procfile {
    ($path:literal, $render:path) => {
        $crate::__kreg::register!("procfile", $crate::ProcFileEntry, $crate::ProcFileEntry { path: $path, render: $render });
    };
}

/// B1: a file system that can be frozen for a consistent snapshot (all dirty data written, new writes held
/// back until `thaw`). Every file system implements it; tmpfs (`SuperBlock`) trivially.
pub trait FsFreeze: Send + Sync {
    fn freeze(&self) -> Result<(), Errno>;
    fn thaw(&self);
}

static ROOT_NS: ksync::Once<Arc<MountNs>> = ksync::Once::new();

/// The boot mount view: tmpfs at `/`. Processes hold an `Arc<MountNs>` (B2) and inherit it; FS-11 gives
/// programs their own views (`MountNs::copy_view`).
pub fn root_ns() -> Arc<MountNs> {
    ROOT_NS.get_or_init(|| Arc::new(MountNs::new_empty())).clone()
}

/// Kernel init: unpack the initramfs into the root view (everything is copied: the caller may release the
/// module afterwards), make `/tmp` (1777), mount devtmpfs, proc and sysfs and make the registered nodes.
pub fn init(initramfs: Option<&[u8]>) -> Result<usize, &'static str> {
    let ns = root_ns();
    let mut st = cpio::Unpacked::default();
    let mut result = Ok(());
    if let Some(data) = initramfs {
        match cpio::unpack(data, &ns) {
            Ok(s) => st = s,
            Err(e) => {
                klog::event!(Error, "vfs", "initramfs.bad", error = alloc::format!("{e:?}"), bytes = data.len(); "entries before the error were kept");
                result = Err("initramfs: bad cpio archive");
            }
        }
    }
    // FS-5: /tmp is a tmpfs of its own (1777, bounded by the pool); without it a directory of the root.
    // AB-7c: nosuid,nodev as systemd's tmp.mount (strictatime there: no relatime).
    if let Err(e) = tmpfs::mount_new(&ns, "/tmp", 0o1777, None).map(|m| m.set_flags(MNT_NOSUID | MNT_NODEV)) {
        klog::event!(Warn, "vfs", "tmp.failed", errno = e; "/tmp stays a directory of the root file system");
        if let Ok(d) = ns.mkdir_p("/tmp") {
            d.inode.meta.set_perm(0o1777);
        }
    }
    let made = devfs::populate(&ns);
    // /dev/shm (POSIX shared memory, Chromium's shm files): a tmpfs on the devtmpfs, 1777 as on Linux.
    if let Err(e) = tmpfs::mount_new(&ns, "/dev/shm", 0o1777, None).map(|m| m.set_flags(MNT_NOSUID | MNT_NODEV)) {
        klog::event!(Warn, "vfs", "shm.failed", errno = e; "no /dev/shm: shm_open fails in programs");
    }
    klog::event!(
        Info,
        "vfs",
        "root.ready",
        fs = "tmpfs",
        initramfs_bytes = initramfs.map(|d| d.len()).unwrap_or(0),
        files = st.files,
        dirs = st.dirs,
        symlinks = st.symlinks,
        hardlinks = st.hardlinks,
        nodes = st.nodes,
        skipped = st.skipped,
        data_bytes = st.bytes,
        devnodes = made.devnodes,
        procfiles = made.procfiles,
        procdirs = made.procdirs,
        ttys_adopted = made.adopted,
        node_failures = made.failed
    );
    result.map(|_| st.files)
}
