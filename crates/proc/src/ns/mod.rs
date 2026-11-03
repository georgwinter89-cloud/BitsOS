//! SX-2: the namespaces Chromium's sandbox uses (linux-6.6/kernel/nsproxy.c, user_namespace.c,
//! pid_namespace.c, kernel/utsname.c, ipc/namespace.c, net/core/net_namespace.c).
//!
//! - `user`: user namespaces with their id maps (`idmap`); a `Cred` names its namespace (`cred.rs`).
//! - `pid`: pid namespaces; every process and thread keeps its one global id (TABLE/TASKS, procfs) and gets a
//!   number in each pid namespace from its own up to the first level below the initial one, where programs see
//!   it (`getpid`, `fork`'s answer, `kill`'s argument, wait results, si_pid).
//! - `misc`: UTS (its own host and domain name), IPC (empty: V2 has no SysV IPC), network (only `lo`; the
//!   socket rules live in the syscall glue until NW's per-namespace views exist).
//! - `mnt` (SX-3a): mount namespaces around FS's views (`vfs::MountNs`).
//! - `NsSet` (Linux `nsproxy`): what a group's members use and its children get.
//!
//! Approximation (recorded in plan/notes/sx-SX-2.md): Linux keeps the nsproxy per thread; V2 keeps it per thread
//! group, like the credentials (AB-13). Chromium only unshares while single-threaded, and unshare of a user or
//! pid namespace needs a single thread anyway.
//!
//! Locks: `proc.userns` (map writes, a spin leaf), `proc.pidns` (one per pid namespace, a leaf taken under
//! `proc.table`), `proc.nsset` (the group's set, a leaf), `proc.uts` (one per UTS namespace, a leaf),
//! `proc.nscount` (the per-user counts, `count`, a leaf).

pub mod count;
pub mod file;
pub mod idmap;
pub mod misc;
pub mod mnt;
pub mod pid;
pub mod set;
pub mod user;
#[cfg(test)]
mod tests;

use core::sync::atomic::{AtomicU32, Ordering};

/// linux-6.6/include/linux/proc_ns.h: the fixed inode numbers of the initial namespaces.
pub const INIT_IPC_INUM: u32 = 0xEFFF_FFFF;
pub const INIT_UTS_INUM: u32 = 0xEFFF_FFFE;
pub const INIT_USER_INUM: u32 = 0xEFFF_FFFD;
pub const INIT_PID_INUM: u32 = 0xEFFF_FFFC;
/// `PROC_DYNAMIC_FIRST`: the first number `proc_alloc_inum` hands out (the initial network namespace gets it).
const DYNAMIC_FIRST: u32 = 0xF000_0000;

static NEXT_INUM: AtomicU32 = AtomicU32::new(DYNAMIC_FIRST);

/// A fresh namespace inode number (`ns_alloc_inum`). Linux reuses freed numbers (an ida); V2 counts on, which
/// leaves 256 Mi namespaces before a wrap, and a wrap only makes two numbers equal, nothing else.
pub(crate) fn alloc_inum() -> u32 {
    NEXT_INUM.fetch_add(1, Ordering::Relaxed)
}

/// The namespace kinds `/proc/<pid>/ns/` shows and setns(2) takes (`CLONE_NEW*` value, link name).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Net,
    Uts,
    Ipc,
    Pid,
    PidForChildren,
    User,
    Mnt,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Net => "net",
            Kind::Uts => "uts",
            Kind::Ipc => "ipc",
            Kind::Pid => "pid",
            Kind::PidForChildren => "pid_for_children",
            Kind::User => "user",
            Kind::Mnt => "mnt",
        }
    }
    /// The type word readlink shows (`pid_for_children` is a pid namespace).
    pub fn type_name(self) -> &'static str {
        if self == Kind::PidForChildren { "pid" } else { self.name() }
    }
    /// The CLONE_NEW* flag setns(2) compares `nstype` with.
    pub fn flag(self) -> u64 {
        use uapi::ab::clone::*;
        match self {
            Kind::Net => CLONE_NEWNET,
            Kind::Uts => CLONE_NEWUTS,
            Kind::Ipc => CLONE_NEWIPC,
            Kind::Pid | Kind::PidForChildren => CLONE_NEWPID,
            Kind::User => CLONE_NEWUSER,
            Kind::Mnt => CLONE_NEWNS,
        }
    }
    /// Linux's order in `/proc/<pid>/ns` (fs/proc/namespaces.c `ns_entries`, without cgroup and time).
    pub const ALL: [Kind; 7] = [Kind::Net, Kind::Uts, Kind::Ipc, Kind::Pid, Kind::PidForChildren, Kind::User, Kind::Mnt];
}
