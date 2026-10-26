//! AF_UNIX sockets (linux-6.6/net/unix/af_unix.c): stream, datagram and seqpacket; path names (a socket inode
//! made through the VFS, found again by its inode), abstract names (one name space per socket type, as Linux's
//! hash includes the type) and autobind; listen/connect/accept with embryo sockets in the listener's queue;
//! socketpair; SCM_RIGHTS and SCM_CREDENTIALS; SO_PEERCRED.
//!
//! Locks (R5): `st` (one socket's state and receive queue) is a short spin section and never nested with
//! another socket's `st`; the name tables are taken before a socket's `st` (bind) and never inside it. Nothing
//! is dropped and no queue is woken while an `st` is held: packets taken out of a queue (their files may be the
//! last reference to another socket, whose release takes locks) are dropped afterwards. `io` serialises the
//! readers of one socket (Linux `iolock`) and is never held while waiting.
//!
//! Flow control as Linux: a sender's bytes in other sockets' queues are charged to it (`wmem`, with a per-packet
//! overhead like an skb's truesize) and it waits while they reach SO_SNDBUF; a datagram receiver that is not
//! connected back to the sender takes at most `backlog` (net.unix.max_dgram_qlen) queued datagrams.

use super::abi::*;
use super::addr::{self, UnixName, UnixTarget};
use super::unix_gc::InFlight;
use super::{Caller, Deadline, Opts, Received, Source, Ucred, flag, wait};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::{Errno, OpenFile};

/// What one queued packet costs its sender beyond its bytes (Linux charges an skb's truesize).
const OVERHEAD: usize = 768;
/// Largest piece of a stream send (`(sndbuf >> 1) - 64`, at most one head page + UNIX_SKB_FRAGS_SZ).
const STREAM_CHUNK: usize = 32768 + 3776;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UType {
    Stream,
    Dgram,
    SeqPacket,
}

impl UType {
    /// `unix_create`: SOCK_RAW becomes a datagram socket; other types are ESOCKTNOSUPPORT.
    pub fn from_linux(t: u32) -> Option<UType> {
        match t {
            SOCK_STREAM => Some(UType::Stream),
            SOCK_DGRAM | SOCK_RAW => Some(UType::Dgram),
            SOCK_SEQPACKET => Some(UType::SeqPacket),
            _ => None,
        }
    }
    pub fn linux(self) -> u32 {
        match self {
            UType::Stream => SOCK_STREAM,
            UType::Dgram => SOCK_DGRAM,
            UType::SeqPacket => SOCK_SEQPACKET,
        }
    }
    fn connection(self) -> bool {
        self != UType::Dgram
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Close,
    Listen,
    Established,
}

/// Returns the sender's charge when the packet is gone (received or purged) and wakes its writers.
struct Charge {
    to: Weak<UnixSock>,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        if let Some(s) = self.to.upgrade() {
            s.wmem.fetch_sub(self.bytes, Ordering::AcqRel);
            s.wake();
        }
    }
}

pub(crate) struct Packet {
    data: Vec<u8>,
    consumed: usize,
    /// The sender's name when it sent (recvfrom's address).
    from: Option<UnixName>,
    /// Attached credentials (explicit SCM_CREDENTIALS, or the sender's when a side has SO_PASSCRED).
    creds: Option<Ucred>,
    files: Option<InFlight>,
    /// A listener's queue: a connection waiting for accept (its embryo socket).
    conn: Option<Arc<UnixSock>>,
    _charge: Option<Charge>,
}

impl Packet {
    fn left(&self) -> usize {
        self.data.len() - self.consumed
    }
}

struct St {
    state: State,
    shutdown: u8,
    /// Pending error (`sk_err`), reported once.
    err: i64,
    peer: Option<Arc<UnixSock>>,
    name: Option<UnixName>,
    /// A path name's socket inode (kept alive while bound, as Linux's `u->path`).
    path: Option<vfs::Path>,
    /// Key in the path table.
    inode: Option<usize>,
    /// `sk_max_ack_backlog`: the listen backlog, or the datagram queue limit.
    backlog: usize,
    peercred: Option<Ucred>,
    dead: bool,
    /// Made by a connect and not yet accepted (`!sk->sk_socket`).
    embryo: bool,
    connecting: bool,
    q: VecDeque<Packet>,
}

pub struct UnixSock {
    pub ty: UType,
    st: SpinLock<St>,
    /// Readers, pollers and accept wait here (and the socket's own writers, woken when its charge drops). A
    /// `PollQueue` (INT-W4, WY-1a CR1): epoll items of stream and seqpacket sockets hang on it, so an epoll
    /// inside another epoll wakes its outer one (a legacy object was only looked at by a sleeper of its own epoll).
    pub(crate) wq: vfs::poll::PollQueue,
    /// Senders waiting for room in this socket's queue (datagram backlog, listen backlog).
    peer_wq: WaitQueue,
    io: Mutex<()>,
    wmem: AtomicUsize,
    pub(crate) opts: Opts,
    /// How often this socket's open file sits in a queue (the collector's count).
    pub(crate) inflight: AtomicUsize,
}

/// Abstract names, one name space per socket type: (type, name bytes) -> socket.
static ABSTRACT: SpinLock<BTreeMap<(UType, Vec<u8>), Weak<UnixSock>>> = SpinLock::named("net.unix.abstract", BTreeMap::new());
/// Path names: the socket inode's address -> socket (Linux `unix_find_socket_byinode`).
static BSD: SpinLock<BTreeMap<usize, Weak<UnixSock>>> = SpinLock::named("net.unix.bsd", BTreeMap::new());
static AUTO_NEXT: AtomicU32 = AtomicU32::new(0);

klog::counter!(UNIX_MADE, "net.unix.made");

/// Everything a send needs besides the bytes.
pub struct SendArgs<'a> {
    pub flags: u32,
    pub nonblock: bool,
    /// The raw destination address (sendto/sendmsg msg_name).
    pub to: Option<&'a [u8]>,
    pub files: Vec<Arc<OpenFile>>,
    pub creds: Option<Ucred>,
    pub caller: Caller,
    /// RLIMIT_NOFILE of the caller (the in-flight limit).
    pub nofile: usize,
}

impl SendArgs<'_> {
    /// write(2).
    pub fn plain(nonblock: bool) -> SendArgs<'static> {
        SendArgs { flags: 0, nonblock, to: None, files: Vec::new(), creds: None, caller: super::current(), nofile: usize::MAX }
    }
}

fn inode_key(p: &vfs::Path) -> usize {
    Arc::as_ptr(&p.dentry.inode) as usize
}

/// The file behind a path name's inode: opening it is not possible on Linux (ENXIO); here open succeeds and
/// every read or write answers ENXIO (change request to FS: open of an S_IFSOCK inode -> ENXIO).
struct SockNode;

impl vfs::File for SockNode {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(ENXIO)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(ENXIO)
    }
    fn mode(&self) -> u32 {
        vfs::abi::S_IFSOCK | 0o777
    }
}

/// `vfs_mknod(S_IFSOCK)` through the public VFS pieces, with Linux's error order (`filename_create`,
/// `may_create`): EEXIST for an existing name (the caller maps it to EADDRINUSE).
fn mknod_sock(ctx: &vfs::Ctx<'_>, path: &[u8]) -> Result<vfs::Path, Errno> {
    use vfs::walk::LastKind;
    let (dir, last) = ctx.walker().parent(ctx.cwd.clone(), path)?;
    if last.kind != LastKind::Normal {
        return Err(EEXIST);
    }
    if last.name.len() > vfs::abi::NAME_MAX {
        return Err(ENAMETOOLONG);
    }
    let check = || -> Result<(), Errno> {
        if ctx.walker().child(&dir, &last.name)?.is_some() {
            return Err(EEXIST);
        }
        if last.trailing {
            return Err(ENOENT);
        }
        match dir.dentry.sb.fixed {
            Some(vfs::dentry::Fixed::Proc) => return Err(ENOENT),
            Some(vfs::dentry::Fixed::Sys) => {
                dir.dentry.inode.meta.permission(ctx.cred, vfs::inode::MAY_WRITE | vfs::inode::MAY_EXEC)?;
                return Err(EPERM);
            }
            None => {}
        }
        if dir.mnt.readonly {
            return Err(EROFS);
        }
        if dir.dentry.is_dead_dir() {
            return Err(ENOENT);
        }
        if dir.dentry.inode.immutable.load(Ordering::Relaxed) {
            return Err(EPERM);
        }
        dir.dentry.inode.meta.permission(ctx.cred, vfs::inode::MAY_WRITE | vfs::inode::MAY_EXEC)
    };
    check()?;
    if !policy::allows_any(&ctx.subject, policy::AccessClass::Path) {
        let mut p = ctx.ns.path_of(&ctx.root, &dir).unwrap_or_default();
        if p.last() != Some(&b'/') {
            p.push(b'/');
        }
        p.extend_from_slice(&last.name);
        let text = alloc::string::String::from_utf8_lossy(&p);
        if let Err(d) = policy::check(&ctx.subject, &policy::Access::Path { path: &text, write: true, create: true }) {
            klog::event!(Info, "net", "unix.policy_refused", pid = ctx.subject.pid, path = &*text, reason = d.reason.as_str());
            return Err(EACCES);
        }
    }
    let sb = dir.dentry.sb.clone();
    let _g = sb.lock_names();
    check()?;
    let mode = vfs::abi::S_IFSOCK | (0o777 & !ctx.umask);
    let d = dir.dentry.create_child(&last.name, mode, ctx.cred, vfs::inode::Data::Device(Arc::new(SockNode)))?;
    Ok(vfs::Path { mnt: dir.mnt.clone(), dentry: d })
}

/// `unix_find_other`: the socket a name stands for, of type `ty`.
fn find(name: &UnixName, ty: UType, ctx: Option<&vfs::Ctx<'_>>, caller: &Caller) -> Result<Arc<UnixSock>, Errno> {
    if let Some(a) = name.abstract_name() {
        if caller.subject != policy::Subject::KERNEL
            && policy::check(&caller.subject, &policy::Access::NetConnect { proto: policy::NetProto::Unix, addr: policy::IpAddr::V4([0; 4]), port: 0 }).is_err()
        {
            return Err(EACCES);
        }
        let _ = a;
        return ABSTRACT.lock().get(&(ty, name.bytes().to_vec())).and_then(|w| w.upgrade()).ok_or(ECONNREFUSED);
    }
    let ctx = ctx.ok_or(ECONNREFUSED)?;
    let path = name.path().unwrap_or_default();
    let p = ctx.lookup(None, path, true)?;
    p.dentry.inode.meta.permission(ctx.cred, vfs::inode::MAY_WRITE)?;
    if !policy::allows_any(&ctx.subject, policy::AccessClass::Path) {
        // PM-1 CR 5: the canonical path of what the lookup found, not the caller's text (relative, "..").
        let name = ctx.ns.path_of(&ctx.root, &p).unwrap_or_default();
        let text = alloc::string::String::from_utf8_lossy(&name);
        if policy::check(&ctx.subject, &policy::Access::Path { path: &text, write: true, create: false }).is_err() {
            return Err(EACCES);
        }
    }
    if p.dentry.inode.meta.mode() & uapi::S_IFMT != vfs::abi::S_IFSOCK {
        return Err(ECONNREFUSED);
    }
    let s = BSD.lock().get(&inode_key(&p)).and_then(|w| w.upgrade()).ok_or(ECONNREFUSED)?;
    if s.ty != ty {
        return Err(EPROTOTYPE);
    }
    Ok(s)
}

/// The Linux view of a not-attached credential: pid 0, ids shown as overflowuid.
fn shown(c: Option<Ucred>) -> Ucred {
    c.unwrap_or(Ucred { pid: 0, uid: OVERFLOW_ID, gid: OVERFLOW_ID })
}

impl UnixSock {
    pub fn new(ty: UType) -> Arc<UnixSock> {
        UNIX_MADE.inc();
        Arc::new(UnixSock {
            ty,
            st: SpinLock::named(
                "net.unix.state",
                St {
                    state: State::Close,
                    shutdown: 0,
                    err: 0,
                    peer: None,
                    name: None,
                    path: None,
                    inode: None,
                    backlog: MAX_DGRAM_QLEN,
                    peercred: None,
                    dead: false,
                    embryo: false,
                    connecting: false,
                    q: VecDeque::new(),
                },
            ),
            wq: vfs::poll::PollQueue::new(),
            peer_wq: WaitQueue::new(),
            io: Mutex::named("net.unix.io", ()),
            wmem: AtomicUsize::new(0),
            opts: Opts::new(WMEM_DEFAULT, RMEM_DEFAULT),
            inflight: AtomicUsize::new(0),
        })
    }

    /// `socketpair`: two connected sockets, both with the creator's credentials as peer credentials.
    pub fn pair(ty: UType, caller: &Caller) -> (Arc<UnixSock>, Arc<UnixSock>) {
        let (a, b) = (UnixSock::new(ty), UnixSock::new(ty));
        for (x, y) in [(&a, &b), (&b, &a)] {
            let mut s = x.st.lock();
            s.peer = Some(y.clone());
            s.state = State::Established;
            s.peercred = Some(caller.ucred());
        }
        (a, b)
    }

    pub(crate) fn wake(&self) {
        if self.wq.waiter_count() > 0 {
            self.wq.wake_all();
        }
    }

    /// An embryo that never reached a queue: its link to the connecting socket goes.
    fn forget(&self) {
        let mut s = self.st.lock();
        s.dead = true;
        let p = s.peer.take();
        drop(s);
        drop(p);
    }

    fn wake_senders(&self) {
        if self.peer_wq.waiter_count() > 0 {
            self.peer_wq.wake_all();
        }
    }

    fn passcred(&self) -> bool {
        self.opts.has(flag::PASSCRED)
    }

    /// SO_ERROR (cleared by reading).
    pub fn take_error(&self) -> i64 {
        self.take_err()
    }

    pub fn set_error(&self, e: i64) {
        self.st.lock().err = e;
        self.wake();
    }

    fn take_err(&self) -> i64 {
        core::mem::take(&mut self.st.lock().err)
    }

    /// `sock_error` on a held state.
    fn take_err_locked(s: &mut St) -> Option<i64> {
        (s.err != 0).then(|| core::mem::take(&mut s.err))
    }

    // -----------------------------------------------------------------------------------------------------
    // names

    pub fn bind(self: &Arc<Self>, raw: &[u8], ctx: &vfs::Ctx<'_>) -> Result<(), Errno> {
        match addr::parse_unix(raw, true)? {
            UnixTarget::Auto => self.autobind(),
            UnixTarget::Name(n) if n.is_abstract() => {
                if ctx.subject != policy::Subject::KERNEL
                    && policy::check(&ctx.subject, &policy::Access::NetBind { proto: policy::NetProto::Unix, port: 0 }).is_err()
                {
                    return Err(EACCES);
                }
                let mut t = ABSTRACT.lock();
                let mut s = self.st.lock();
                if s.name.is_some() {
                    return Err(EINVAL);
                }
                let key = (self.ty, n.bytes().to_vec());
                if t.get(&key).is_some_and(|w| w.strong_count() > 0) {
                    return Err(EADDRINUSE);
                }
                t.insert(key, Arc::downgrade(self));
                s.name = Some(n);
                Ok(())
            }
            UnixTarget::Name(n) => {
                let path = n.path().unwrap_or_default().to_vec();
                let p = mknod_sock(ctx, &path).map_err(|e| if e == EEXIST { EADDRINUSE } else { e })?;
                let key = inode_key(&p);
                let bound = {
                    let mut t = BSD.lock();
                    let mut s = self.st.lock();
                    if s.name.is_some() {
                        false
                    } else {
                        t.insert(key, Arc::downgrade(self));
                        s.name = Some(n);
                        s.path = Some(p);
                        s.inode = Some(key);
                        true
                    }
                };
                if !bound {
                    // Linux unlinks what it made and answers EINVAL (already bound).
                    let _ = vfs::Ctx::kernel(ctx.ns).unlink(Some(&ctx.cwd), &path);
                    return Err(EINVAL);
                }
                Ok(())
            }
        }
    }

    /// `unix_autobind`: a free abstract name `\0xxxxx` (five hex digits), tried from a moving start; ENOSPC
    /// when all 2^20 are taken (the bound of Linux's search).
    pub fn autobind(self: &Arc<Self>) -> Result<(), Errno> {
        if self.st.lock().name.is_some() {
            return Ok(());
        }
        if AUTO_NEXT.load(Ordering::Relaxed) == 0 {
            AUTO_NEXT.store((crate::stack::mix64(super::now()) as u32) | 1, Ordering::Relaxed);
        }
        let start = AUTO_NEXT.fetch_add(1, Ordering::Relaxed);
        for i in 0..=0xFFFFFu32 {
            let n = UnixName::auto(start.wrapping_add(i) & 0xFFFFF);
            let mut t = ABSTRACT.lock();
            let key = (self.ty, n.bytes().to_vec());
            if t.get(&key).is_some_and(|w| w.strong_count() > 0) {
                continue;
            }
            let mut s = self.st.lock();
            if s.name.is_some() {
                return Ok(());
            }
            t.insert(key, Arc::downgrade(self));
            s.name = Some(n);
            return Ok(());
        }
        Err(ENOSPC)
    }

    /// getsockname / getpeername.
    pub fn getname(&self, peer: bool) -> Result<Vec<u8>, Errno> {
        let s = self.st.lock();
        let name = if peer {
            let p = s.peer.clone().ok_or(ENOTCONN)?;
            drop(s);
            let n = p.st.lock().name.clone();
            n
        } else {
            s.name.clone()
        };
        Ok(name.map(|n| n.bytes().to_vec()).unwrap_or_else(UnixName::unbound))
    }

    pub fn peercred(&self) -> Option<Ucred> {
        self.st.lock().peercred
    }

    pub fn is_listening(&self) -> bool {
        self.st.lock().state == State::Listen
    }

    // -----------------------------------------------------------------------------------------------------
    // connections

    /// `unix_listen`; `backlog` is already clamped to somaxconn by the syscall.
    pub fn listen(&self, backlog: usize, caller: &Caller) -> Result<(), Errno> {
        if self.ty == UType::Dgram {
            return Err(EOPNOTSUPP);
        }
        let grew = {
            let mut s = self.st.lock();
            if s.name.is_none() {
                return Err(EINVAL);
            }
            if s.state != State::Close && s.state != State::Listen {
                return Err(EINVAL);
            }
            let grew = backlog > s.backlog;
            s.backlog = backlog;
            s.state = State::Listen;
            s.peercred = Some(caller.ucred());
            grew
        };
        if grew {
            self.wake_senders();
        }
        Ok(())
    }

    /// `unix_stream_connect` (stream and seqpacket).
    pub fn connect(self: &Arc<Self>, raw: &[u8], ctx: Option<&vfs::Ctx<'_>>, caller: &Caller, nonblock: bool) -> Result<(), Errno> {
        if self.ty == UType::Dgram {
            return self.dgram_connect(raw, ctx, caller);
        }
        let UnixTarget::Name(name) = addr::parse_unix(raw, false)? else { return Err(EINVAL) };
        if self.passcred() {
            self.autobind()?;
        }
        let dl = Deadline::new(nonblock, self.opts.sndtimeo.load(Ordering::Relaxed));
        // Only the call that set `connecting` may clear it: another thread's connect on the same socket that
        // fails early (lookup, refused, full backlog) must not open the socket to a third connect while this
        // one still waits.
        let mut claimed = false;
        let r = self.connect_to(&name, ctx, caller, dl, &mut claimed);
        if claimed && r.is_err() {
            self.st.lock().connecting = false;
        }
        r
    }

    /// Linux's order (`unix_stream_connect`): the target first (lookup errors, not listening, a full backlog),
    /// then our own state (EISCONN, EINVAL for a listener). `claimed` tells the caller whether this call set
    /// `connecting`.
    fn connect_to(self: &Arc<Self>, name: &UnixName, ctx: Option<&vfs::Ctx<'_>>, caller: &Caller, dl: Deadline, claimed: &mut bool) -> Result<(), Errno> {
        enum Look {
            Dead,
            Full,
            Room,
        }
        loop {
            let other = find(name, self.ty, ctx, caller)?;
            let look = {
                let o = other.st.lock();
                if o.dead {
                    Look::Dead
                } else if o.state != State::Listen || o.shutdown & RCV_SHUTDOWN != 0 {
                    return Err(ECONNREFUSED);
                } else if o.q.len() > o.backlog {
                    Look::Full
                } else {
                    Look::Room
                }
            };
            match look {
                Look::Dead => continue,
                Look::Full => {
                    let room = || {
                        let o = other.st.lock();
                        o.dead || o.shutdown & RCV_SHUTDOWN != 0 || o.q.len() <= o.backlog
                    };
                    if !wait(&other.peer_wq, dl, room)? {
                        return Err(EAGAIN);
                    }
                    continue;
                }
                Look::Room => {}
            }
            if !*claimed {
                let mut s = self.st.lock();
                match s.state {
                    State::Established => return Err(EISCONN),
                    State::Listen => return Err(EINVAL),
                    State::Close if s.connecting => return Err(EISCONN),
                    State::Close => s.connecting = true,
                }
                *claimed = true;
            }
            // The embryo gets the listener's name now (it never changes once bound), so no two sockets' states
            // are ever locked together.
            let (lname, lpath) = {
                let o = other.st.lock();
                (o.name.clone(), o.path.clone())
            };
            let embryo = UnixSock::new(self.ty);
            {
                let mut e = embryo.st.lock();
                e.peer = Some(self.clone());
                e.state = State::Established;
                e.peercred = Some(caller.ucred());
                e.name = lname;
                e.path = lpath;
                e.embryo = true;
            }
            let pushed = {
                let mut o = other.st.lock();
                if o.dead || o.state != State::Listen || o.shutdown & RCV_SHUTDOWN != 0 || o.q.len() > o.backlog {
                    None
                } else if o.q.try_reserve(1).is_err() {
                    drop(o);
                    embryo.forget();
                    return Err(ENOMEM);
                } else {
                    o.q.push_back(Packet { data: Vec::new(), consumed: 0, from: None, creds: None, files: None, conn: Some(embryo.clone()), _charge: None });
                    Some(o.peercred)
                }
            };
            match pushed {
                Some(cred) => {
                    {
                        let mut s = self.st.lock();
                        s.peer = Some(embryo.clone());
                        s.state = State::Established;
                        s.connecting = false;
                        s.peercred = cred;
                    }
                    other.wake();
                    return Ok(());
                }
                // Changed in between: look again.
                None => embryo.forget(),
            }
        }
    }

    /// `unix_dgram_connect`: sets (or with AF_UNSPEC clears) the default peer.
    fn dgram_connect(self: &Arc<Self>, raw: &[u8], ctx: Option<&vfs::Ctx<'_>>, caller: &Caller) -> Result<(), Errno> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        let other = if addr::family(raw) != Some(AF_UNSPEC) {
            let UnixTarget::Name(name) = addr::parse_unix(raw, false)? else { return Err(EINVAL) };
            if self.passcred() {
                self.autobind()?;
            }
            loop {
                let o = find(&name, self.ty, ctx, caller)?;
                let mut g = o.st.lock();
                if g.dead {
                    continue;
                }
                if g.peer.as_ref().is_some_and(|p| !Arc::ptr_eq(p, self)) {
                    return Err(EPERM);
                }
                g.state = State::Established;
                drop(g);
                break Some(o);
            }
        } else {
            None
        };
        let (old, purged) = {
            let mut s = self.st.lock();
            let old = core::mem::replace(&mut s.peer, other.clone());
            s.state = if other.is_some() { State::Established } else { State::Close };
            let changed = match (&old, &other) {
                (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                (Some(_), None) => true,
                _ => false,
            };
            let purged = if changed { core::mem::take(&mut s.q) } else { VecDeque::new() };
            (old.filter(|_| changed), purged)
        };
        if let Some(old) = old {
            // `unix_dgram_disconnected`: our queue (from the old peer) is gone; an old peer connected back to
            // us hears ECONNRESET.
            if !purged.is_empty() {
                let mut o = old.st.lock();
                if !o.dead && o.peer.as_ref().is_some_and(|p| Arc::ptr_eq(p, self)) {
                    o.err = ECONNRESET;
                }
                drop(o);
                old.wake();
            }
            old.wake_senders();
        }
        drop(purged);
        self.wake_senders();
        Ok(())
    }

    /// `unix_accept`: the oldest embryo becomes a socket of its own (SO_PASSCRED is inherited).
    pub fn accept(&self, nonblock: bool) -> Result<Arc<UnixSock>, Errno> {
        if self.ty == UType::Dgram {
            return Err(EOPNOTSUPP);
        }
        let dl = Deadline::new(nonblock, self.opts.rcvtimeo.load(Ordering::Relaxed));
        loop {
            let got = {
                let mut s = self.st.lock();
                if s.state != State::Listen {
                    return Err(EINVAL);
                }
                if let Some(e) = Self::take_err_locked(&mut s) {
                    return Err(e);
                }
                match s.q.pop_front() {
                    Some(p) => Some(p),
                    None if s.shutdown & RCV_SHUTDOWN != 0 => return Err(EINVAL),
                    None => None,
                }
            };
            if let Some(mut p) = got {
                self.wake_senders();
                let Some(e) = p.conn.take() else { continue };
                e.st.lock().embryo = false;
                e.opts.set(flag::PASSCRED, self.passcred());
                return Ok(e);
            }
            let ready = || {
                let s = self.st.lock();
                !s.q.is_empty() || s.shutdown & RCV_SHUTDOWN != 0 || s.err != 0 || s.state != State::Listen
            };
            if !wait(&self.wq, dl, ready)? {
                return Err(EAGAIN);
            }
        }
    }

    /// `unix_shutdown`: our side and, for connected types, the peer's opposite side.
    pub fn shutdown(&self, how: u64) -> Result<(), Errno> {
        if how > SHUT_RDWR {
            return Err(EINVAL);
        }
        let mode = (how + 1) as u8;
        let peer = {
            let mut s = self.st.lock();
            s.shutdown |= mode;
            s.peer.clone()
        };
        self.wake();
        if let Some(p) = peer
            && self.ty.connection()
        {
            let mut pm = 0;
            if mode & RCV_SHUTDOWN != 0 {
                pm |= SEND_SHUTDOWN;
            }
            if mode & SEND_SHUTDOWN != 0 {
                pm |= RCV_SHUTDOWN;
            }
            p.st.lock().shutdown |= pm;
            p.wake();
            p.wake_senders();
        }
        Ok(())
    }

    /// `unix_release_sock`: the last fd is gone (or an embryo nobody accepted: `embryo`).
    pub fn release(&self, embryo: bool) {
        let (peer, q, name, inode, path, state) = {
            let mut s = self.st.lock();
            if s.dead {
                return;
            }
            s.dead = true;
            s.shutdown = SHUTDOWN_MASK;
            let state = s.state;
            s.state = State::Close;
            (s.peer.take(), core::mem::take(&mut s.q), s.name.take(), s.inode.take(), s.path.take(), state)
        };
        if let Some(n) = &name
            && n.is_abstract()
        {
            let mut t = ABSTRACT.lock();
            let key = (self.ty, n.bytes().to_vec());
            if t.get(&key).is_some_and(|w| core::ptr::eq(w.as_ptr(), self)) {
                t.remove(&key);
            }
        }
        if let Some(k) = inode {
            let mut t = BSD.lock();
            if t.get(&k).is_some_and(|w| core::ptr::eq(w.as_ptr(), self)) {
                t.remove(&k);
            }
        }
        self.wake();
        self.wake_senders();
        if let Some(p) = &peer {
            if self.ty.connection() {
                let mut ps = p.st.lock();
                ps.shutdown = SHUTDOWN_MASK;
                if !q.is_empty() || embryo {
                    ps.err = ECONNRESET;
                }
            }
            p.wake();
            p.wake_senders();
        }
        drop(peer);
        drop(path);
        // Queued connections of a listener are reset; files in flight here are closed ("closing in flight").
        for mut pk in q {
            if state == State::Listen
                && let Some(e) = pk.conn.take()
            {
                e.release(true);
            }
            drop(pk);
        }
        super::unix_gc::maybe_collect();
    }

    /// The collector's purge: the queue's packets (and its embryos' queues) go, closing the files in flight.
    pub(crate) fn purge(&self) {
        let q = core::mem::take(&mut self.st.lock().q);
        for mut pk in q {
            if let Some(e) = pk.conn.take() {
                e.purge();
                e.release(true);
            }
            drop(pk);
        }
        self.wake();
    }

    /// Unix sockets whose open files sit in this socket's queue (and in its embryos' queues).
    pub(crate) fn inflight_children(&self) -> Vec<Arc<UnixSock>> {
        let mut out = Vec::new();
        let mut embryos = Vec::new();
        {
            let s = self.st.lock();
            for pk in &s.q {
                if let Some(f) = &pk.files {
                    out.extend(f.files_unix());
                }
                if let Some(e) = &pk.conn {
                    embryos.push(e.clone());
                }
            }
        }
        for e in embryos {
            out.extend(e.inflight_children());
        }
        out
    }

    // -----------------------------------------------------------------------------------------------------
    // data

    fn charge(self: &Arc<Self>, bytes: usize) -> Charge {
        let bytes = bytes + OVERHEAD;
        self.wmem.fetch_add(bytes, Ordering::AcqRel);
        Charge { to: Arc::downgrade(self), bytes }
    }

    /// Waits until this socket may queue another packet (`sock_alloc_send_pskb`: charge below SO_SNDBUF).
    /// Errors: a pending error, EPIPE after shutdown(SHUT_WR), EAGAIN at the deadline, EINTR when killed.
    fn wait_room(&self, dl: Deadline) -> Result<(), Errno> {
        loop {
            {
                let mut s = self.st.lock();
                if let Some(e) = Self::take_err_locked(&mut s) {
                    return Err(e);
                }
                if s.shutdown & SEND_SHUTDOWN != 0 {
                    return Err(EPIPE);
                }
            }
            if self.wmem.load(Ordering::Acquire) < self.opts.sndbuf() {
                return Ok(());
            }
            let ready = || {
                let s = self.st.lock();
                s.err != 0 || s.shutdown & SEND_SHUTDOWN != 0 || self.wmem.load(Ordering::Acquire) < self.opts.sndbuf()
            };
            if !wait(&self.wq, dl, ready)? {
                return Err(EAGAIN);
            }
        }
    }

    /// sendmsg / write. `len` bytes come from `src`; `ctx` resolves a destination path name.
    pub fn send(self: &Arc<Self>, src: &mut dyn Source, len: usize, a: SendArgs<'_>, ctx: Option<&vfs::Ctx<'_>>) -> Result<usize, Errno> {
        match self.ty {
            UType::Stream => self.stream_send(src, len, a),
            UType::SeqPacket => {
                let e = self.take_err();
                if e != 0 {
                    return Err(e);
                }
                if self.st.lock().state != State::Established {
                    return Err(ENOTCONN);
                }
                self.dgram_send(src, len, SendArgs { to: None, ..a }, ctx)
            }
            UType::Dgram => self.dgram_send(src, len, a, ctx),
        }
    }

    fn files_for(&self, a: &mut SendArgs<'_>) -> Result<Option<InFlight>, Errno> {
        if a.files.is_empty() {
            return Ok(None);
        }
        InFlight::attach(core::mem::take(&mut a.files), a.caller.uid, a.nofile).map(Some)
    }

    /// `maybe_add_creds`: the sender's credentials when either side has SO_PASSCRED or the receiver is an
    /// embryo nobody accepted yet.
    fn creds_for(&self, explicit: Option<Ucred>, other: &St, other_sock: &UnixSock, caller: &Caller) -> Option<Ucred> {
        explicit.or_else(|| (self.passcred() || other_sock.passcred() || other.embryo).then(|| caller.ucred()))
    }

    fn stream_send(self: &Arc<Self>, src: &mut dyn Source, len: usize, mut a: SendArgs<'_>) -> Result<usize, Errno> {
        if a.flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let peer = {
            let s = self.st.lock();
            if a.to.is_some_and(|t| !t.is_empty()) {
                return Err(if s.state == State::Established { EISCONN } else { EOPNOTSUPP });
            }
            s.peer.clone()
        };
        let Some(peer) = peer else { return Err(ENOTCONN) };
        let pipe = |sent: usize, flags: u32| -> Result<usize, Errno> {
            if sent == 0 && flags & MSG_NOSIGNAL == 0 {
                super::sigpipe();
            }
            if sent > 0 { Ok(sent) } else { Err(EPIPE) }
        };
        if self.st.lock().shutdown & SEND_SHUTDOWN != 0 {
            return pipe(0, a.flags);
        }
        let mut files = self.files_for(&mut a)?;
        let dl = Deadline::new(a.nonblock || a.flags & MSG_DONTWAIT != 0, self.opts.sndtimeo.load(Ordering::Relaxed));
        let mut sent = 0usize;
        while sent < len {
            match self.wait_room(dl) {
                Ok(()) => {}
                Err(EPIPE) => return pipe(sent, a.flags),
                Err(e) => return if sent > 0 { Ok(sent) } else { Err(e) },
            }
            let size = (len - sent).min(STREAM_CHUNK).min((self.opts.sndbuf() / 2).saturating_sub(64).max(1));
            let mut data = match super::buffer(size) {
                Ok(d) => d,
                Err(e) => return if sent > 0 { Ok(sent) } else { Err(e) },
            };
            if let Err(e) = src.fill(&mut data) {
                return if sent > 0 { Ok(sent) } else { Err(e) };
            }
            let from = self.st.lock().name.clone();
            let charge = self.charge(size);
            let mut pk = Packet { data, consumed: 0, from, creds: None, files: files.take(), conn: None, _charge: Some(charge) };
            let refused = {
                let mut o = peer.st.lock();
                if o.dead || o.shutdown & RCV_SHUTDOWN != 0 {
                    Some(Ok(pk))
                } else if o.q.try_reserve(1).is_err() {
                    Some(Err(pk))
                } else {
                    pk.creds = self.creds_for(a.creds, &o, &peer, &a.caller);
                    o.q.push_back(pk);
                    None
                }
            };
            match refused {
                Some(Ok(pk)) => {
                    drop(pk);
                    return pipe(sent, a.flags);
                }
                Some(Err(pk)) => {
                    // R2: no room for one more queue entry: what was sent stands, else ENOMEM.
                    drop(pk);
                    return if sent > 0 { Ok(sent) } else { Err(ENOMEM) };
                }
                None => {}
            }
            peer.wake();
            sent += size;
        }
        Ok(sent)
    }

    fn dgram_send(self: &Arc<Self>, src: &mut dyn Source, len: usize, mut a: SendArgs<'_>, ctx: Option<&vfs::Ctx<'_>>) -> Result<usize, Errno> {
        if a.flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let target = match a.to.filter(|t| !t.is_empty()) {
            Some(raw) => match addr::parse_unix(raw, false)? {
                UnixTarget::Name(n) => Some(n),
                UnixTarget::Auto => return Err(EINVAL),
            },
            None => None,
        };
        let mut other = match &target {
            Some(_) => None,
            None => Some(self.st.lock().peer.clone().ok_or(ENOTCONN)?),
        };
        if self.passcred() {
            self.autobind()?;
        }
        if len > self.opts.sndbuf().saturating_sub(32) {
            return Err(EMSGSIZE);
        }
        let dl = Deadline::new(a.nonblock || a.flags & MSG_DONTWAIT != 0, self.opts.sndtimeo.load(Ordering::Relaxed));
        self.wait_room(dl)?;
        let mut data = super::buffer(len)?;
        src.fill(&mut data)?;
        let files = self.files_for(&mut a)?;
        let from = self.st.lock().name.clone();
        let charge = self.charge(len);
        let mut pk = Some(Packet { data, consumed: 0, from, creds: None, files, conn: None, _charge: Some(charge) });
        loop {
            let o = match &other {
                Some(o) => o.clone(),
                None => match &target {
                    Some(n) => find(n, self.ty, ctx, &a.caller)?,
                    None => return Err(ECONNRESET),
                },
            };
            enum Next {
                Done,
                Dead,
                Full,
            }
            let next = {
                let mut g = o.st.lock();
                if g.peer.as_ref().is_some_and(|p| !core::ptr::eq(Arc::as_ptr(p), Arc::as_ptr(self))) {
                    return Err(EPERM);
                }
                if g.dead {
                    Next::Dead
                } else if g.shutdown & RCV_SHUTDOWN != 0 {
                    return Err(EPIPE);
                } else if !Arc::ptr_eq(&o, self)
                    && !g.peer.as_ref().is_some_and(|p| Arc::ptr_eq(p, self))
                    && g.q.len() > g.backlog
                {
                    Next::Full
                } else {
                    g.q.try_reserve(1).map_err(|_| ENOMEM)?;
                    let mut p = pk.take().ok_or(EINVAL)?;
                    p.creds = self.creds_for(a.creds, &g, &o, &a.caller);
                    g.q.push_back(p);
                    Next::Done
                }
            };
            match next {
                Next::Done => {
                    o.wake();
                    return Ok(len);
                }
                Next::Dead => {
                    if self.ty == UType::SeqPacket {
                        return Err(EPIPE);
                    }
                    let mut s = self.st.lock();
                    if s.peer.as_ref().is_some_and(|p| Arc::ptr_eq(p, &o)) {
                        s.peer = None;
                        s.state = State::Close;
                        let q = core::mem::take(&mut s.q);
                        drop(s);
                        drop(q);
                        return Err(ECONNREFUSED);
                    }
                    drop(s);
                    if target.is_none() {
                        return Err(ECONNREFUSED);
                    }
                    other = None;
                }
                Next::Full => {
                    let room = || {
                        let g = o.st.lock();
                        g.dead || g.shutdown & RCV_SHUTDOWN != 0 || g.q.len() <= g.backlog
                    };
                    if !wait(&o.peer_wq, dl, room)? {
                        return Err(EAGAIN);
                    }
                    other = Some(o);
                }
            }
        }
    }

    /// recvmsg / read. Bytes go to `sink` in order; `want_from` asks for the sender's address.
    pub fn recv(&self, size: usize, flags: u32, nonblock: bool, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>, want_from: bool) -> Result<Received, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.rcvtimeo.load(Ordering::Relaxed));
        match self.ty {
            UType::Stream => self.stream_recv(size, flags, dl, sink, want_from),
            _ => {
                if self.ty == UType::SeqPacket && self.st.lock().state != State::Established {
                    return Err(ENOTCONN);
                }
                self.dgram_recv(size, flags, dl, sink, want_from)
            }
        }
    }

    fn dgram_recv(&self, size: usize, flags: u32, dl: Deadline, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>, want_from: bool) -> Result<Received, Errno> {
        let peek = flags & MSG_PEEK != 0;
        let _io = self.io.lock();
        let got = loop {
            enum Look {
                Got(Packet),
                Peeked(Vec<u8>, usize, Option<UnixName>, Option<Ucred>, Vec<Arc<OpenFile>>),
                Empty,
            }
            let look = {
                let mut s = self.st.lock();
                if let Some(e) = Self::take_err_locked(&mut s) {
                    return Err(e);
                }
                if peek {
                    match s.q.front() {
                        Some(p) => {
                            let n = size.min(p.data.len());
                            let mut d = Vec::new();
                            d.try_reserve_exact(n).map_err(|_| ENOMEM)?;
                            d.extend_from_slice(&p.data[..n]);
                            Look::Peeked(d, p.data.len(), p.from.clone(), p.creds, p.files.as_ref().map(|f| f.peek()).unwrap_or_default())
                        }
                        None => Look::Empty,
                    }
                } else {
                    match s.q.pop_front() {
                        Some(p) => Look::Got(p),
                        None => Look::Empty,
                    }
                }
            };
            match look {
                Look::Got(p) => {
                    self.wake_senders();
                    let len = p.data.len();
                    let n = size.min(len);
                    let r = sink(&p.data[..n]);
                    // A failed copy loses the datagram (Linux frees the skb) and closes its files.
                    r?;
                    let Packet { from, creds, files, .. } = p;
                    break (n, len, from, creds, files.map(|f| f.take()).unwrap_or_default());
                }
                Look::Peeked(d, len, from, creds, files) => {
                    sink(&d)?;
                    break (d.len(), len, from, creds, files);
                }
                Look::Empty => {}
            }
            {
                let s = self.st.lock();
                if s.shutdown & RCV_SHUTDOWN != 0 && (!dl.nonblocking() || self.ty == UType::SeqPacket) {
                    return Ok(Received::default());
                }
                if self.ty == UType::SeqPacket && s.state != State::Established && s.state != State::Listen {
                    return Err(ENOTCONN);
                }
            }
            if dl.nonblocking() {
                return Err(EAGAIN);
            }
            let ready = || {
                let s = self.st.lock();
                !s.q.is_empty() || s.err != 0 || s.shutdown & RCV_SHUTDOWN != 0
            };
            if !wait(&self.wq, dl, ready)? {
                return Err(EAGAIN);
            }
        };
        let (n, len, from, creds, fds) = got;
        let mut r = Received { n: if flags & MSG_TRUNC != 0 { len } else { n }, ..Default::default() };
        if n < len {
            r.flags |= MSG_TRUNC;
        }
        if want_from {
            r.from = from.map(|f| f.bytes().to_vec());
        }
        if self.passcred() {
            r.creds = Some(shown(creds));
        }
        r.fds = fds;
        Ok(r)
    }

    fn stream_recv(&self, size: usize, flags: u32, dl: Deadline, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>, want_from: bool) -> Result<Received, Errno> {
        if self.st.lock().state != State::Established {
            return Err(EINVAL);
        }
        let peek = flags & MSG_PEEK != 0;
        let target = if flags & MSG_WAITALL != 0 { size } else { (self.opts.rcvlowat.load(Ordering::Relaxed) as usize).min(size) }.max(1);
        let passcred = self.passcred();
        let mut io = Some(self.io.lock());
        let mut copied = 0usize;
        let mut creds: Option<Option<Ucred>> = None;
        let mut from: Option<Vec<u8>> = None;
        let mut fds: Vec<Arc<OpenFile>> = Vec::new();
        // Peeking walks the queue: packet index and offset inside it.
        let (mut pi, mut poff) = (0usize, 0usize);
        let mut err: Option<i64> = None;
        loop {
            if copied >= size && size > 0 {
                break;
            }
            enum Step {
                Chunk(Vec<u8>, Option<Ucred>, Option<UnixName>, bool),
                Stop,
                Wait,
            }
            let step = {
                let mut s = self.st.lock();
                if s.dead {
                    err = Some(ECONNRESET);
                    Step::Stop
                } else {
                    let idx = if peek { pi } else { 0 };
                    match s.q.get(idx) {
                        None => {
                            if copied >= target {
                                Step::Stop
                            } else if let Some(e) = Self::take_err_locked(&mut s) {
                                err = Some(e);
                                Step::Stop
                            } else if s.shutdown & RCV_SHUTDOWN != 0 {
                                Step::Stop
                            } else {
                                Step::Wait
                            }
                        }
                        Some(p) => {
                            if let Some(c) = creds {
                                if c != p.creds {
                                    // Never glue messages from different writers.
                                    Step::Stop
                                } else {
                                    let off = if peek { p.consumed + poff } else { p.consumed };
                                    let n = (p.data.len() - off).min(size - copied);
                                    Step::Chunk(p.data[off..off + n].to_vec(), p.creds, p.from.clone(), p.files.is_some())
                                }
                            } else {
                                let off = if peek { p.consumed + poff } else { p.consumed };
                                let n = (p.data.len() - off).min(size - copied);
                                Step::Chunk(p.data[off..off + n].to_vec(), p.creds, p.from.clone(), p.files.is_some())
                            }
                        }
                    }
                }
            };
            match step {
                Step::Stop => break,
                Step::Wait => {
                    if dl.nonblocking() {
                        err = Some(EAGAIN);
                        break;
                    }
                    io = None;
                    let ready = || {
                        let s = self.st.lock();
                        let idx = if peek { pi } else { 0 };
                        s.q.len() > idx || s.err != 0 || s.shutdown & RCV_SHUTDOWN != 0 || s.dead
                    };
                    match wait(&self.wq, dl, ready) {
                        Ok(true) => {}
                        Ok(false) => {
                            err = Some(EAGAIN);
                            break;
                        }
                        Err(e) => {
                            err = Some(e);
                            break;
                        }
                    }
                    io = Some(self.io.lock());
                    continue;
                }
                Step::Chunk(bytes, pc, pfrom, has_files) => {
                    if creds.is_none() && passcred {
                        creds = Some(pc);
                    }
                    if want_from && from.is_none() {
                        from = pfrom.map(|f| f.bytes().to_vec());
                    }
                    if let Err(e) = sink(&bytes) {
                        if copied == 0 {
                            err = Some(e);
                        }
                        break;
                    }
                    let n = bytes.len();
                    copied += n;
                    if peek {
                        let s = self.st.lock();
                        let Some(p) = s.q.get(pi) else { break };
                        if has_files {
                            fds = p.files.as_ref().map(|f| f.peek()).unwrap_or_default();
                            break;
                        }
                        poff += n;
                        if p.consumed + poff >= p.data.len() {
                            pi += 1;
                            poff = 0;
                        } else {
                            break;
                        }
                    } else {
                        let (done, taken) = {
                            let mut s = self.st.lock();
                            let Some(p) = s.q.front_mut() else { break };
                            p.consumed += n;
                            let f = p.files.take();
                            let left = p.left();
                            let taken = if left == 0 { s.q.pop_front() } else { None };
                            (left > 0 || f.is_some(), (taken, f))
                        };
                        let (pk, f) = taken;
                        drop(pk);
                        if let Some(f) = f {
                            fds = f.take();
                        }
                        if done {
                            break;
                        }
                    }
                }
            }
        }
        drop(io);
        if copied == 0
            && let Some(e) = err
        {
            return Err(e);
        }
        let mut r = Received { n: copied, ..Default::default() };
        r.from = from;
        if passcred {
            r.creds = Some(shown(creds.flatten()));
        }
        r.fds = fds;
        Ok(r)
    }

    // -----------------------------------------------------------------------------------------------------
    // readiness

    /// `unix_poll` (stream) / `unix_dgram_poll` (datagram, seqpacket).
    pub fn poll(&self) -> u32 {
        use vfs::Events as E;
        let s = self.st.lock();
        let mut m = 0;
        if s.err != 0 {
            m |= E::ERR;
        }
        if s.shutdown == SHUTDOWN_MASK {
            m |= E::HUP;
        }
        if s.shutdown & RCV_SHUTDOWN != 0 {
            m |= E::RDHUP | E::IN;
        }
        if !s.q.is_empty() {
            m |= E::IN;
        }
        if self.ty.connection() && s.state == State::Close {
            m |= E::HUP;
        }
        let mut writable = s.state != State::Listen && self.wmem.load(Ordering::Acquire).saturating_mul(4) <= self.opts.sndbuf();
        let peer = if self.ty == UType::Dgram { s.peer.clone() } else { None };
        drop(s);
        if writable && let Some(p) = peer {
            let ps = p.st.lock();
            if !ps.peer.as_ref().is_some_and(|x| core::ptr::eq(Arc::as_ptr(x), self)) && ps.q.len() > ps.backlog {
                writable = false;
            }
        }
        if writable {
            m |= E::OUT;
        }
        m
    }

    /// SIOCINQ: bytes waiting (stream/seqpacket: all; datagram: the next one); EINVAL on a listener.
    pub fn inq(&self) -> Result<i64, Errno> {
        let s = self.st.lock();
        if s.state == State::Listen {
            return Err(EINVAL);
        }
        Ok(if self.ty == UType::Dgram { s.q.front().map_or(0, |p| p.data.len()) } else { s.q.iter().map(|p| p.left()).sum::<usize>() } as i64)
    }

    /// SIOCOUTQ: what this socket has queued at others (with the per-packet overhead, as Linux's truesize).
    pub fn outq(&self) -> usize {
        self.wmem.load(Ordering::Acquire)
    }
}

impl InFlight {
    fn files_unix(&self) -> Vec<Arc<UnixSock>> {
        self.sockets()
    }
}
