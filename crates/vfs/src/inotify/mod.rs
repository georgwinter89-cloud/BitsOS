//! FS-14: inotify (linux-6.6/fs/notify/inotify/inotify_user.c, inotify_fsnotify.c, fs/notify/fsnotify.c) on every
//! file system: the VFS calls `hooks` where Linux calls its fsnotify hooks (namei.c, open.c, read_write.c,
//! attr.c, dcache.c, file_table.c, readdir.c), so tmpfs, devtmpfs (`/dev/input` hot plug), proc, sysfs and FAT
//! all report.
//!
//! - A **watch** (Linux's inotify mark) sits on an inode's `Marks` and in its group's table by wd; the table
//!   pins the inode (as a mark does), the inode holds the watch, the watch knows its group only weakly.
//! - A **group** (`group::Group`) is the inotify fd: its event queue (merge of a repeat of the last event,
//!   IN_Q_OVERFLOW at `max_queued_events`), its wait queue for read/poll/epoll, its watches.
//! - **Delivery** (`send`): a watch takes an event its mask asks for; IN_IGNORED, IN_Q_OVERFLOW and IN_UNMOUNT
//!   need no bit. A directory's watch also takes its children's ACCESS/MODIFY/ATTRIB/OPEN/CLOSE with the child's
//!   name (Linux FS_EVENT_ON_CHILD, `fsnotify_parent`). IN_ONESHOT ends a watch after its first event.
//!
//! Locks (all spin, short, never held across a wait): a group's `vfs.inotify.watches` > an inode's
//! `vfs.inotify.marks`; a group's `vfs.inotify.queue` is taken alone; wakes and the drop of an ended watch's
//! inode pin come after every lock is released, so hooks may run under any FS lock. `ACTIVE` keeps them free
//! while nobody watches anything.

pub mod group;
pub mod hooks;
pub mod sys;
#[cfg(test)]
mod tests;

use crate::inode::Inode;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use ksync::SpinLock;

pub use group::Group;

pub const IN_ACCESS: u32 = 0x0000_0001;
pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
pub const IN_OPEN: u32 = 0x0000_0020;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_UNMOUNT: u32 = 0x0000_2000;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
pub const IN_EXCL_UNLINK: u32 = 0x0400_0000;
pub const IN_MASK_CREATE: u32 = 0x1000_0000;
pub const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
pub const IN_ONESHOT: u32 = 0x8000_0000;
pub const IN_ALL_EVENTS: u32 = 0xfff;
/// `ALL_INOTIFY_BITS` (linux-6.6/include/linux/inotify.h): any other bit of a mask is EINVAL.
pub const ALL_INOTIFY_BITS: u32 = IN_ALL_EVENTS
    | IN_UNMOUNT
    | IN_Q_OVERFLOW
    | IN_IGNORED
    | IN_ONLYDIR
    | IN_DONT_FOLLOW
    | IN_EXCL_UNLINK
    | IN_MASK_ADD
    | IN_MASK_CREATE
    | IN_ISDIR
    | IN_ONESHOT;
/// Events a directory's watch takes for its children (`FS_EVENTS_POSS_ON_CHILD`).
pub const ON_CHILD: u32 = IN_ACCESS | IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_OPEN;
/// `struct inotify_event` without its name.
pub const EVENT_SIZE: usize = 16;

/// Watches alive in the whole system (the hooks' fast path).
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

klog::counter!(EVENTS, "vfs.inotify.events");
klog::counter!(OVERFLOWS, "vfs.inotify.overflows");

pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed) != 0
}

/// One watch: a group's interest in one inode.
pub struct Watch {
    pub wd: i32,
    /// Event bits (IN_ALL_EVENTS) plus the flags IN_ONESHOT and IN_EXCL_UNLINK.
    mask: AtomicU32,
    group: Weak<Group>,
    /// Ended (removed, oneshot fired, inode gone, group closed): delivers nothing more.
    dead: AtomicBool,
}

impl Watch {
    pub fn mask(&self) -> u32 {
        self.mask.load(Ordering::Acquire)
    }
}

/// The watches on one inode (Linux `i_fsnotify_marks`).
pub struct Marks {
    list: SpinLock<Vec<Arc<Watch>>>,
    n: AtomicUsize,
}

impl Default for Marks {
    fn default() -> Self {
        Marks { list: SpinLock::named("vfs.inotify.marks", Vec::new()), n: AtomicUsize::new(0) }
    }
}

impl Marks {
    fn is_empty(&self) -> bool {
        self.n.load(Ordering::Acquire) == 0
    }
    fn snapshot(&self) -> Vec<Arc<Watch>> {
        if self.is_empty() {
            return Vec::new();
        }
        self.list.lock().clone()
    }
    /// The watch of `group` here, if any.
    fn of_group(&self, group: &Group) -> Option<Arc<Watch>> {
        self.list.lock().iter().find(|w| core::ptr::eq(w.group.as_ptr(), group)).cloned()
    }
    fn add(&self, w: Arc<Watch>) -> Result<(), crate::Errno> {
        let mut l = self.list.lock();
        l.try_reserve(1).map_err(|_| uapi::errno::ENOMEM)?;
        l.push(w);
        self.n.store(l.len(), Ordering::Release);
        Ok(())
    }
    fn remove(&self, w: &Arc<Watch>) {
        let mut l = self.list.lock();
        l.retain(|x| !Arc::ptr_eq(x, w));
        self.n.store(l.len(), Ordering::Release);
    }
}

/// One event before it is queued: the mask (with IN_ISDIR), the cookie of a rename pair, a child's name.
#[derive(Clone, Copy)]
pub(crate) struct Ev<'a> {
    pub mask: u32,
    pub cookie: u32,
    pub name: Option<&'a [u8]>,
}

/// Delivers `ev` to the watches of `inode` that ask for it; `on_child`: the event is a child's, for a
/// directory's watches only. `unlinked`: the subject's name is gone (IN_EXCL_UNLINK watches skip it).
fn send(inode: &Inode, ev: Ev<'_>, on_child: bool, unlinked: bool) {
    let watches = inode.marks.snapshot();
    for w in watches {
        let m = w.mask();
        if w.dead.load(Ordering::Acquire) || m & ev.mask & IN_ALL_EVENTS == 0 {
            continue;
        }
        if on_child && (!inode.is_dir() || ev.mask & ON_CHILD == 0) {
            continue;
        }
        if unlinked && m & IN_EXCL_UNLINK != 0 {
            continue;
        }
        let Some(g) = w.group.upgrade() else { continue };
        // `inotify_handle_inode_event`: inotify never reported IN_ISDIR with the SELF events.
        let mask = if ev.mask & (IN_MOVE_SELF | IN_DELETE_SELF) != 0 { ev.mask & !IN_ISDIR } else { ev.mask };
        g.queue(w.wd, mask, ev.cookie, ev.name);
        EVENTS.inc();
        if m & IN_ONESHOT != 0 {
            end(&w, inode);
        }
    }
}

/// Ends a watch once: off the inode, out of its group's table (the inode's pin goes after the lock), IN_IGNORED
/// queued (`inotify_ignored_and_remove_idr`), the user's count given back.
fn end(w: &Arc<Watch>, inode: &Inode) {
    if w.dead.swap(true, Ordering::AcqRel) {
        return;
    }
    inode.marks.remove(w);
    ACTIVE.fetch_sub(1, Ordering::Relaxed);
    if let Some(g) = w.group.upgrade() {
        let pinned = g.forget(w.wd);
        g.queue(w.wd, IN_IGNORED, 0, None);
        drop(pinned);
    }
}

/// The inode's last name and last user went (Linux `fsnotify_inoderemove`): IN_DELETE_SELF, then every watch
/// on it ends (IN_IGNORED).
pub(crate) fn inode_removed(inode: &Inode) {
    if inode.marks.is_empty() {
        return;
    }
    send(inode, Ev { mask: IN_DELETE_SELF, cookie: 0, name: None }, false, false);
    for w in inode.marks.snapshot() {
        end(&w, inode);
    }
}
