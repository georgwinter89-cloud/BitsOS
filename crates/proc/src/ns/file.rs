//! SX-2: namespace files (Linux nsfs, linux-6.6/fs/nsfs.c and fs/proc/namespaces.c): what an open of
//! `/proc/<pid>/ns/<kind>` gives. The file holds the namespace (it lives while the fd does, as Linux's
//! `ns_get_path` keeps it), fstat shows the namespace's inode number, setns(2) takes it.
//!
//! A walk that follows the link lands on an empty, read-only file below a private empty view (`place`) whose
//! inode number is the namespace's, so stat(2) and access(2) of the path work as on Linux (Chromium checks
//! that `/proc/self/ns/user` exists); its device is the view's, not nsfs's.

use super::Kind;
use super::misc::{IpcNs, NetNs, UtsNs};
use super::mnt::MntNs;
use super::pid::PidNs;
use super::user::UserNs;
use crate::Process;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use ksync::{Once, SpinLock};
use uapi::errno::*;
use vfs::abi::Attr;
use vfs::{Errno, File, MountNs, OpenFile};

/// One namespace a file can stand for.
#[derive(Clone)]
pub enum NsRef {
    User(Arc<UserNs>),
    Pid(Arc<PidNs>),
    Uts(Arc<UtsNs>),
    Ipc(Arc<IpcNs>),
    Net(Arc<NetNs>),
    Mnt(Arc<MntNs>),
}

impl NsRef {
    pub fn kind(&self) -> Kind {
        match self {
            NsRef::User(_) => Kind::User,
            NsRef::Pid(_) => Kind::Pid,
            NsRef::Uts(_) => Kind::Uts,
            NsRef::Ipc(_) => Kind::Ipc,
            NsRef::Net(_) => Kind::Net,
            NsRef::Mnt(_) => Kind::Mnt,
        }
    }
    /// The inode number (`ns_common.inum`).
    pub fn inum(&self) -> u32 {
        match self {
            NsRef::User(n) => n.inum,
            NsRef::Pid(n) => n.inum,
            NsRef::Uts(n) => n.inum,
            NsRef::Ipc(n) => n.inum,
            NsRef::Net(n) => n.inum,
            NsRef::Mnt(m) => m.inum,
        }
    }
    /// The namespace's user namespace (`ns_get_owner` of the others; a user namespace's own parent).
    pub fn owner(&self) -> Option<Arc<UserNs>> {
        match self {
            NsRef::User(n) => n.parent.clone(),
            NsRef::Pid(n) => Some(n.user_ns.clone()),
            NsRef::Uts(n) => Some(n.user_ns.clone()),
            NsRef::Ipc(n) => Some(n.user_ns.clone()),
            NsRef::Net(n) => Some(n.user_ns.clone()),
            NsRef::Mnt(n) => Some(n.user_ns.clone()),
        }
    }
}

/// Process `p`'s namespace of kind `k` (`ns_get`), as `/proc/<pid>/ns/<k>` shows it.
pub fn ns_of(p: &Process, k: Kind) -> NsRef {
    let s = p.nsset();
    match k {
        Kind::Net => NsRef::Net(s.net.clone()),
        Kind::Uts => NsRef::Uts(s.uts.clone()),
        Kind::Ipc => NsRef::Ipc(s.ipc.clone()),
        Kind::Pid => NsRef::Pid(p.pid_ns.clone()),
        Kind::PidForChildren => NsRef::Pid(s.pid_for_children.clone()),
        Kind::User => NsRef::User(p.cred().user_ns.clone()),
        Kind::Mnt => NsRef::Mnt(p.mnt_ns()),
    }
}

/// The link text readlink shows: `user:[4026531837]`.
pub fn link_text(k: Kind, inum: u32) -> String {
    alloc::format!("{}:[{}]", k.type_name(), inum)
}

struct NsFile {
    ns: NsRef,
}

/// Namespace files by address (as pidfds: which open files are namespace files, and of what).
static OBJECTS: SpinLock<BTreeMap<usize, NsRef>> = SpinLock::named("proc.nsfile.objects", BTreeMap::new());

impl Drop for NsFile {
    fn drop(&mut self) {
        let gone = OBJECTS.lock().remove(&(self as *const NsFile as *const u8 as usize));
        drop(gone);
    }
}

/// nsfs's device (one anonymous device for every namespace file, as Linux's nsfs super block).
fn nsfs_dev() -> u64 {
    static DEV: Once<u64> = Once::new();
    *DEV.get_or_init(vfs::dentry::anon_dev)
}

impl File for NsFile {
    /// `ns_file_operations` has no read or write: EINVAL.
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// `__ns_get_path`: S_IFREG | S_IRUGO.
    fn mode(&self) -> u32 {
        uapi::S_IFREG | 0o444
    }
    fn attr(&self) -> Option<Attr> {
        Some(Attr { dev: nsfs_dev(), ino: self.ns.inum() as u64, mode: uapi::S_IFREG | 0o444, nlink: 1, blksize: 4096, ..Default::default() })
    }
    fn link_name(&self) -> Option<String> {
        Some(link_text(self.ns.kind(), self.ns.inum()))
    }
}

/// An open namespace file (O_RDONLY, as `proc_ns_get_link` → `nsfs` opens give it).
pub fn open_file(ns: NsRef, flags: u64) -> Arc<OpenFile> {
    let obj = Arc::new(NsFile { ns: ns.clone() });
    OBJECTS.lock().insert(Arc::as_ptr(&obj) as *const u8 as usize, ns);
    OpenFile::anon(obj, flags & !(vfs::abi::O_ACCMODE))
}

/// The namespace an open file stands for (`proc_ns_file`), None for every other file.
pub fn ns_of_file(f: &OpenFile) -> Option<NsRef> {
    let addr = Arc::as_ptr(&f.file) as *const u8 as usize;
    OBJECTS.lock().get(&addr).cloned()
}

/// The private, empty view whose root the namespace places hang below (see the module doc).
fn view() -> &'static Arc<MountNs> {
    static VIEW: Once<Arc<MountNs>> = Once::new();
    VIEW.get_or_init(|| Arc::new(MountNs::new_empty()))
}

/// The content of a namespace place: nothing (an open through the link gives `open_file` instead).
struct Empty;

impl vfs::synth::FileSource for Empty {
    fn render(&self, _who: vfs::Cred) -> Result<alloc::vec::Vec<u8>, Errno> {
        Ok(alloc::vec::Vec::new())
    }
}

/// Where a walk that follows `/proc/<pid>/ns/<k>` lands (nsfs's dentry, `ns_get_path`): a read-only file whose
/// inode number is the namespace's, so stat(2) of the path shows what readlink names (Chromium checks that
/// `/proc/self/ns/user` exists; tools compare the numbers).
pub fn place(ns: &NsRef) -> Result<vfs::Path, Errno> {
    let root = view().root_path();
    let inode = vfs::inode::Inode::new(ns.inum() as u64, uapi::S_IFREG | 0o444, vfs::Cred::ROOT, vfs::inode::Data::Synth(Arc::new(Empty)));
    let name = link_text(ns.kind(), ns.inum());
    let dentry = vfs::dentry::Dentry::ephemeral(&root.dentry, name.as_bytes(), inode);
    Ok(vfs::Path { mnt: root.mnt, dentry })
}
