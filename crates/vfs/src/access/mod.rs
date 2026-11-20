//! DB2 (Durchblick, Georg's idea 12): what each program touched. The hooks in the open/unlink/rename paths, in
//! exec, in the socket calls and in the rights calls put one fixed-size record each into this CPU's ring
//! (`ring`), with no allocation and no lock but the ring's own; the reader of `/dev/bitsos/access` (`node`, the
//! service bitsaccess, root only) drains every ring, merges the records per program (count, first and last time)
//! and hands out JSON lines (`drain`). Lost records are counted, never silently dropped.
//!
//! What a hook sees is what Linux's fanotify sees at `fsnotify_open`/`fsnotify_create`/`fsnotify_delete`
//! (linux-7.3.1/fs/notify/fanotify) and audit at its syscall exits (kernel/auditsc.c): the opened path and its
//! mode, the directory a name left or entered, the peer of a socket. This is a BitsOS feature, not a Linux ABI:
//! neither interface is emulated. Reads are kept per folder (the page shows folders); a device is kept by its node.
//!
//! The hot paths pay one relaxed atomic load while recording is off (settings key `access.enabled`, written by the
//! service through the node).
//!
//! A record holds a path only weakly (no strong reference to its mount or dentry): a program that touched a mount
//! must not keep it busy for umount, nor keep a removed file's space, until the reader comes. A file's record also
//! keeps its folder (weakly) and its name, so a file removed before the reader comes is still counted there.

pub mod drain;
mod ktests;
mod node;
pub mod ring;

use crate::dentry::Dentry;
use crate::mount_ns::{Mount, Path};
use crate::ops::Ctx;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub use ring::{dropped, prepare};

const OFF: u8 = 0;
const ON_: u8 = 1;
/// A ktest on a live system switched recording off for a moment: what programs do meanwhile counts as lost.
const PAUSED: u8 = 2;

/// On from boot, so the services started before bitsaccess are seen; the service switches it off when Georg did.
static ON: AtomicU8 = AtomicU8::new(ON_);

pub fn on() -> bool {
    ON.load(Ordering::Relaxed) == ON_
}

/// The hooks' test: on; while a ktest paused the log, the record is counted as lost.
#[inline]
fn live() -> bool {
    match ON.load(Ordering::Relaxed) {
        ON_ => true,
        PAUSED => {
            ring::lost_one();
            false
        }
        _ => false,
    }
}

/// Off: nothing more is recorded and what waits (in the rings and kept back by a ktest) is dropped, not counted
/// as lost. On again: recording goes on.
pub fn set_on(v: bool) {
    ON.store(if v { ON_ } else { OFF }, Ordering::Relaxed);
    if !v {
        drop(ring::take_all());
        drain::forget();
    }
}

/// For ktests on a live system: no recording, but nothing waiting is dropped and the reader keeps what it knows
/// (`set_on` ends the pause).
pub fn pause() {
    ON.store(PAUSED, Ordering::Relaxed);
}

/// The cost ktest's baseline: off as Georg's switch makes it, but nothing waiting is dropped (what programs do
/// meanwhile is not counted: the baseline must not pay for counting).
fn hush() {
    ON.store(OFF, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Kind {
    Read,
    Write,
    Create,
    Unlink,
    Rename,
    Exec,
    Device,
    Connect,
    Listen,
    Accept,
    NetClose,
    /// A program with a set-uid or set-gid bit was started.
    SetId,
    /// capset raised the effective set.
    CapRaise,
    /// PTRACE_ATTACH / PTRACE_SEIZE of another program.
    Ptrace,
    /// fork/clone made a process (`Obj::Num` = the child): the child runs its parent's program until its exec.
    /// Only for the reader's program table, never handed out.
    Fork,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Read => "read",
            Kind::Write => "write",
            Kind::Create => "create",
            Kind::Unlink => "unlink",
            Kind::Rename => "rename",
            Kind::Exec => "exec",
            Kind::Device => "device",
            Kind::Connect => "connect",
            Kind::Listen => "listen",
            Kind::Accept => "accept",
            Kind::NetClose => "netclose",
            Kind::SetId => "setid",
            Kind::CapRaise => "caps",
            Kind::Ptrace => "ptrace",
            Kind::Fork => "fork",
        }
    }
}

/// A peer or local address of an inet socket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct NetAddr {
    /// 6 TCP, 17 UDP.
    pub proto: u8,
    pub v6: bool,
    pub addr: [u8; 16],
    pub port: u16,
}

impl NetAddr {
    /// From a sockaddr as programs pass it (AF_INET 2, AF_INET6 10); None for other families.
    pub fn from_sockaddr(proto: u8, raw: &[u8]) -> Option<NetAddr> {
        let fam = u16::from_le_bytes([*raw.first()?, *raw.get(1)?]);
        let port = u16::from_be_bytes([*raw.get(2)?, *raw.get(3)?]);
        let mut addr = [0u8; 16];
        match fam {
            2 => addr[..4].copy_from_slice(raw.get(4..8)?),
            10 => addr.copy_from_slice(raw.get(8..24)?),
            _ => return None,
        }
        Some(NetAddr { proto, v6: fam == 10, addr, port })
    }
}

/// A path as a record keeps it: weakly, so it never keeps a mount busy or a removed file alive. `up` and `name`
/// (files only) say where the file was when it was touched: its folder and name.
#[derive(Clone)]
pub struct Seen {
    pub mnt: Weak<Mount>,
    pub dentry: Weak<Dentry>,
    pub up: Weak<Dentry>,
    pub name: Option<Arc<[u8]>>,
}

impl Seen {
    /// The path while its mount and dentry still live.
    pub fn path(&self) -> Option<Path> {
        Some(Path { mnt: self.mnt.upgrade()?, dentry: self.dentry.upgrade()? })
    }
    /// The folder it was in (files only), while that folder and its mount live.
    pub fn folder(&self) -> Option<Path> {
        Some(Path { mnt: self.mnt.upgrade()?, dentry: self.up.upgrade()? })
    }
}

#[derive(Clone)]
pub enum Obj {
    Path(Seen),
    Net(NetAddr),
    Num(u64),
}

/// What a hook hands the ring: a path is kept (weakly) only when it gets a slot of its own (a repeat folds without
/// touching any reference count). `file`: a file's record, which keeps its folder and name too.
#[derive(Clone, Copy)]
pub enum ObjRef<'a> {
    Path(&'a Path, bool),
    Net(NetAddr),
    Num(u64),
}

impl ObjRef<'_> {
    /// Same thing as a kept record's (the weak pointers keep their storage, so an address is never reused while
    /// a record names it).
    fn same(&self, o: &Obj) -> bool {
        match (self, o) {
            (ObjRef::Path(a, _), Obj::Path(b)) => core::ptr::eq(Arc::as_ptr(&a.dentry), b.dentry.as_ptr()) && core::ptr::eq(Arc::as_ptr(&a.mnt), b.mnt.as_ptr()),
            (ObjRef::Net(a), Obj::Net(b)) => a == b,
            (ObjRef::Num(a), Obj::Num(b)) => a == b,
            _ => false,
        }
    }
    fn owned(&self) -> Obj {
        match self {
            ObjRef::Path(p, file) => {
                // Rarely busy (a rename of it right now): then only the live path names it at the read.
                let (up, name) = match file.then(|| p.dentry.try_place()).flatten() {
                    Some((up, name)) => (up, Some(name)),
                    None => (Weak::new(), None),
                };
                Obj::Path(Seen { mnt: Arc::downgrade(&p.mnt), dentry: Arc::downgrade(&p.dentry), up, name })
            }
            ObjRef::Net(a) => Obj::Net(*a),
            ObjRef::Num(n) => Obj::Num(*n),
        }
    }
}

/// A record's fixed part as a hook makes it.
#[derive(Clone, Copy)]
pub struct Head {
    pub kind: Kind,
    pub flags: u8,
    pub pid: u32,
    pub uid: u32,
    pub t: u64,
    pub tx: u64,
    pub rx: u64,
}

/// Flag bits of a record.
pub const F_DIR: u8 = 1;
pub const F_WRITE: u8 = 2;

#[derive(Clone)]
pub struct Rec {
    pub kind: Kind,
    pub flags: u8,
    pub pid: u32,
    pub uid: u32,
    pub n: u32,
    pub t0: u64,
    pub t1: u64,
    pub tx: u64,
    pub rx: u64,
    pub obj: Obj,
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Puts one record into this CPU's ring (the hooks below all end here).
pub fn record(kind: Kind, flags: u8, pid: u32, uid: u32, obj: ObjRef<'_>) {
    ring::push(&Head { kind, flags, pid, uid, t: now(), tx: 0, rx: 0 }, obj);
}

/// Majors of nodes every program opens all the time and that say nothing (mem, tty, console, ptmx, pts).
fn boring_device(rdev: u64) -> bool {
    matches!(crate::abi::dev_major(rdev), 1 | 4 | 5 | 136..=143)
}

/// `Ctx::open` after a successful open of a regular file, a folder or a device node.
#[inline]
pub fn opened(c: &Ctx<'_>, p: &Path, ftype: u32, write: bool, created: bool) {
    if c.subject == policy::Subject::KERNEL || !live() {
        return;
    }
    let (kind, flags) = match ftype {
        uapi::S_IFREG if created => (Kind::Create, 0),
        uapi::S_IFREG if write => (Kind::Write, 0),
        uapi::S_IFREG => (Kind::Read, 0),
        uapi::S_IFDIR => (Kind::Read, F_DIR),
        uapi::S_IFCHR | crate::abi::S_IFBLK => {
            let rdev = match &p.dentry.inode.data {
                crate::inode::Data::Device(f) => f.rdev(),
                _ => 0,
            };
            if boring_device(rdev) {
                return;
            }
            (Kind::Device, if write { F_WRITE } else { 0 })
        }
        _ => return,
    };
    record(kind, flags, c.subject.pid, c.cred.uid, ObjRef::Path(p, kind != Kind::Device && flags & F_DIR == 0));
}

/// A name left (`unlink`, `rmdir`) or entered (`rename`, `mkdir`) the folder `dir`.
#[inline]
pub fn named(c: &Ctx<'_>, kind: Kind, dir: &Path) {
    if c.subject == policy::Subject::KERNEL || !live() {
        return;
    }
    record(kind, F_DIR, c.subject.pid, c.cred.uid, ObjRef::Path(dir, false));
}

/// exec of `exe` succeeded; `setid`: the file carries a set-uid or set-gid bit.
#[inline]
pub fn exec(pid: u32, uid: u32, exe: &Path, setid: bool) {
    if !live() {
        return;
    }
    record(Kind::Exec, 0, pid, uid, ObjRef::Path(exe, false));
    if setid {
        record(Kind::SetId, 0, pid, uid, ObjRef::Path(exe, false));
    }
}

/// connect / listen / accept of an inet socket.
#[inline]
pub fn net(kind: Kind, pid: u32, uid: u32, a: NetAddr) {
    if live() {
        record(kind, 0, pid, uid, ObjRef::Net(a));
    }
}

/// capset raised the effective set (`caps`: the new bits), ptrace attached to `target`.
#[inline]
pub fn right(kind: Kind, pid: u32, uid: u32, arg: u64) {
    if live() {
        record(kind, 0, pid, uid, ObjRef::Num(arg));
    }
}

/// fork/clone: `child` runs `parent`'s program until it execs.
#[inline]
pub fn forked(parent: u32, uid: u32, child: u32) {
    if live() {
        record(Kind::Fork, 0, parent, uid, ObjRef::Num(child as u64));
    }
}

/// One inet socket's account: its owner, its peer once known and the bytes it moved. Kept inside the socket;
/// `closed` (the last fd went) records it once.
pub struct NetTally {
    pub tx: AtomicU64,
    pub rx: AtomicU64,
    peer: ksync::SpinLock<Option<(u32, u32, NetAddr)>>,
}

impl Default for NetTally {
    fn default() -> Self {
        NetTally { tx: AtomicU64::new(0), rx: AtomicU64::new(0), peer: ksync::SpinLock::named("vfs.access.peer", None) }
    }
}

impl NetTally {
    /// The connection's peer and owner (connect, accept): from then on its close is recorded.
    pub fn set_peer(&self, pid: u32, uid: u32, a: NetAddr) {
        *self.peer.lock() = Some((pid, uid, a));
    }
    #[inline]
    pub fn sent(&self, n: usize) {
        self.tx.fetch_add(n as u64, Ordering::Relaxed);
    }
    #[inline]
    pub fn received(&self, n: usize) {
        self.rx.fetch_add(n as u64, Ordering::Relaxed);
    }
    pub fn closed(&self) {
        let Some((pid, uid, a)) = self.peer.lock().take() else { return };
        if !live() {
            return;
        }
        let (tx, rx) = (self.tx.load(Ordering::Relaxed), self.rx.load(Ordering::Relaxed));
        ring::push(&Head { kind: Kind::NetClose, flags: 0, pid, uid, t: now(), tx, rx }, ObjRef::Net(a));
    }
}

/// vfs-sys installs the process layer's view of a pid: (program path, start time in ns since boot); None when
/// no such process lives.
pub type ProcInfo = fn(u32) -> Option<(alloc::vec::Vec<u8>, u64)>;
static PROC_INFO: ksync::Once<ProcInfo> = ksync::Once::new();

pub fn set_proc_info(f: ProcInfo) {
    PROC_INFO.get_or_init(|| f);
}

pub(crate) fn proc_info(pid: u32) -> Option<(alloc::vec::Vec<u8>, u64)> {
    PROC_INFO.get().and_then(|f| f(pid))
}

/// EINS-C: the assistant tree a live pid is in (0: none), installed by vfs-sys.
pub type TreeInfo = fn(u32) -> u32;
static TREE_INFO: ksync::Once<TreeInfo> = ksync::Once::new();

pub fn set_tree_info(f: TreeInfo) {
    TREE_INFO.get_or_init(|| f);
}

pub(crate) fn tree_of(pid: u32) -> u32 {
    TREE_INFO.get().map_or(0, |f| f(pid))
}
