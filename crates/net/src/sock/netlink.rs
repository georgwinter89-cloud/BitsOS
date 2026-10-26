//! AF_NETLINK sockets, protocol NETLINK_ROUTE (linux-6.6/net/netlink/af_netlink.c): port ids (bind, autobind
//! with the process id first, negative ids after a collision), connect, multicast groups, the request path into
//! the kernel (`netlink_rcv_skb` → `rtnl::rcv_msg`, answered before sendmsg returns, as Linux does: musl reads
//! the answers with MSG_DONTWAIT), dumps handed out as the reader reads (`netlink_dump`), acks
//! (`netlink_ack`), overruns (ENOBUFS once per congestion), user-to-user unicast and broadcast (root only, as
//! rtnetlink allows), and the kernel's notifications to subscribed sockets when a card's link or address changes
//! (`changed`, called by the stack's poll thread and `set_static`).
//!
//! Locks (R5): `bind` (a sleeping mutex per socket, bind and autobind only) > `PORTS` > a socket's `st`;
//! `MEMBERS` > a socket's `st` (membership is decided under MEMBERS); one socket's `st` at a time; nothing is
//! built or logged under `st` (answers are made first, then queued). Notification rounds run one at a time
//! (`changed`, without a lock: a flag pair).

use super::abi::*;
use super::nlmsg::*;
use super::rtnl::{self, Action, Dump, NlErr, ReqCtx};
use super::{Caller, Deadline, Opts, Received, Source, Ucred, wait};
use crate::ifview;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::{Mutex, RwSpinLock, SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::Errno;

pub const NETLINK_ROUTE: i32 = 0;
/// `MAX_LINKS`: protocol numbers are below it.
pub const MAX_LINKS: i32 = 32;
pub const SOL_NETLINK: u32 = 270;
pub const NETLINK_ADD_MEMBERSHIP: u32 = 1;
pub const NETLINK_DROP_MEMBERSHIP: u32 = 2;
pub const NETLINK_PKTINFO: u32 = 3;
pub const NETLINK_BROADCAST_ERROR: u32 = 4;
pub const NETLINK_NO_ENOBUFS: u32 = 5;
pub const NETLINK_LISTEN_ALL_NSID: u32 = 8;
pub const NETLINK_LIST_MEMBERSHIPS: u32 = 9;
pub const NETLINK_CAP_ACK: u32 = 10;
pub const NETLINK_EXT_ACK: u32 = 11;
pub const NETLINK_GET_STRICT_CHK: u32 = 12;
/// `struct sockaddr_nl`.
pub const SOCKADDR_NL: usize = 12;

/// `NLMSG_GOODSIZE` (one page less the skb overhead) and the largest dump datagram (`max_recvmsg_len`'s cap,
/// `SKB_WITH_OVERHEAD(32768)`).
const GOODSIZE: usize = 4096 - 320;
const MAX_RECVMSG_LEN: usize = 32768 - 320;
/// What a queued datagram costs beyond its bytes (an skb's truesize overhead).
const SKB_OVERHEAD: usize = 576;

/// Bits of `St::flags` (`NETLINK_F_*`).
mod nf {
    pub const PKTINFO: u32 = 1;
    pub const BROADCAST_ERROR: u32 = 2;
    pub const NO_ENOBUFS: u32 = 4;
    pub const LISTEN_ALL_NSID: u32 = 8;
    pub const CAP_ACK: u32 = 16;
    pub const EXT_ACK: u32 = 32;
    pub const STRICT_CHK: u32 = 64;
}

/// One queued datagram.
struct Pkt {
    data: Vec<u8>,
    /// Sender's port id (0: the kernel) and the group it was sent to (0: unicast).
    from: u32,
    group: u32,
    creds: Ucred,
    truesize: usize,
}

struct St {
    portid: u32,
    bound: bool,
    /// Joined groups, bit n-1 = group n (RTNLGRP_MAX = 33 fits).
    groups: u64,
    /// `nlk->ngroups`: 0 until the socket first joins or binds with groups, then RTNLGRP_MAX.
    ngroups: u32,
    connected: bool,
    dst_portid: u32,
    dst_group: u32,
    flags: u32,
    congested: bool,
    q: VecDeque<Pkt>,
    rmem: usize,
    err: i64,
    dump: Option<Dump>,
    /// The requester's port id and the dump's sequence (answers carry them).
    dump_pid: u32,
    max_recvmsg_len: usize,
    released: bool,
}

pub struct NetlinkSock {
    pub(crate) ty: u32,
    pub(crate) opts: Opts,
    pub(crate) wq: WaitQueue,
    /// The opener's uid (`file_ns_capable` of the socket's file).
    opener_uid: u32,
    bind: Mutex<()>,
    st: SpinLock<St>,
}

/// Bound sockets by port id (one table: NETLINK_ROUTE is the only protocol).
static PORTS: RwSpinLock<BTreeMap<u32, Weak<NetlinkSock>>> = RwSpinLock::named("net.netlink.ports", BTreeMap::new());
/// Sockets with at least one group, by address.
static MEMBERS: SpinLock<BTreeMap<usize, Weak<NetlinkSock>>> = SpinLock::named("net.netlink.members", BTreeMap::new());
static MEMBER_COUNT: AtomicUsize = AtomicUsize::new(0);

klog::counter!(REQUESTS, "net.netlink.requests");
klog::counter!(OVERRUNS, "net.netlink.overruns");
klog::counter!(NOTIFIED, "net.netlink.notified");

fn key(s: &NetlinkSock) -> usize {
    s as *const NetlinkSock as usize
}

impl NetlinkSock {
    /// `netlink_create`: SOCK_RAW or SOCK_DGRAM (ESOCKTNOSUPPORT), a protocol below MAX_LINKS that exists
    /// (EPROTONOSUPPORT: V2 has NETLINK_ROUTE only).
    pub fn create(ty: u32, protocol: i32, c: &Caller) -> Result<Arc<NetlinkSock>, Errno> {
        if ty != SOCK_RAW && ty != SOCK_DGRAM {
            return Err(ESOCKTNOSUPPORT);
        }
        if !(0..MAX_LINKS).contains(&protocol) || protocol != NETLINK_ROUTE {
            return Err(EPROTONOSUPPORT);
        }
        Ok(Arc::new(NetlinkSock {
            ty,
            opts: Opts::new(WMEM_DEFAULT, RMEM_DEFAULT),
            wq: WaitQueue::new(),
            opener_uid: c.uid,
            bind: Mutex::named("net.netlink.bind", ()),
            st: SpinLock::named(
                "net.netlink.sock",
                St {
                    portid: 0,
                    bound: false,
                    groups: 0,
                    ngroups: 0,
                    connected: false,
                    dst_portid: 0,
                    dst_group: 0,
                    flags: 0,
                    congested: false,
                    q: VecDeque::new(),
                    rmem: 0,
                    err: 0,
                    dump: None,
                    dump_pid: 0,
                    max_recvmsg_len: 0,
                    released: false,
                },
            ),
        }))
    }

    pub fn linux_type(&self) -> u32 {
        self.ty
    }

    fn portid(&self) -> u32 {
        self.st.lock().portid
    }

    /// `netlink_insert` under the socket's bind lock: EADDRINUSE when the id is taken.
    fn insert(self: &Arc<Self>, portid: u32) -> Result<(), Errno> {
        {
            let mut t = PORTS.write();
            if t.get(&portid).is_some_and(|w| w.strong_count() > 0) {
                return Err(EADDRINUSE);
            }
            t.insert(portid, Arc::downgrade(self));
        }
        let mut st = self.st.lock();
        st.portid = portid;
        st.bound = true;
        Ok(())
    }

    /// `netlink_autobind`: the process id if free, else a negative id from a random start below -4096.
    fn autobind(self: &Arc<Self>, c: &Caller) -> Result<(), Errno> {
        let _g = self.bind.lock();
        if self.st.lock().bound {
            return Ok(());
        }
        if self.insert(c.pid).is_ok() {
            return Ok(());
        }
        let span = (i32::MIN as i64).unsigned_abs() - 4097;
        let mut rover = (i32::MIN as i64 + (crate::stack::mix64(super::now() ^ key(self) as u64) % span) as i64) as i32;
        loop {
            if self.insert(rover as u32).is_ok() {
                return Ok(());
            }
            rover = if rover == i32::MIN { -4097 } else { rover - 1 };
        }
    }

    fn ensure_bound(self: &Arc<Self>, c: &Caller) -> Result<(), Errno> {
        if self.st.lock().bound { Ok(()) } else { self.autobind(c) }
    }

    /// `netlink_allowed` for NETLINK_ROUTE: receiving multicast is open to all (NL_CFG_F_NONROOT_RECV), sending
    /// to a group or another socket needs CAP_NET_ADMIN.
    fn admin(c: &Caller) -> bool {
        c.uid == 0
    }

    /// bind(2).
    pub fn bind(self: &Arc<Self>, raw: &[u8], c: &Caller) -> Result<(), Errno> {
        if raw.len() < SOCKADDR_NL || u16::from_le_bytes([raw[0], raw[1]]) != AF_NETLINK {
            return Err(EINVAL);
        }
        let pid = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let groups = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as u64;
        let _g = self.bind.lock();
        let (bound, portid) = {
            let st = self.st.lock();
            (st.bound, st.portid)
        };
        if bound && pid != portid {
            return Err(EINVAL);
        }
        if !bound {
            if pid != 0 {
                self.insert(pid)?;
            } else {
                drop(_g);
                self.autobind(c)?;
            }
        }
        let mut st = self.st.lock();
        if groups != 0 {
            st.ngroups = rtnl::RTNLGRP_MAX;
        }
        // Groups beyond the socket's group count are dropped (none: the count is 0 or 33 > 32 bits).
        let groups = if st.ngroups == 0 { 0 } else { groups };
        if groups == 0 && st.groups & 0xffff_ffff == 0 {
            return Ok(());
        }
        st.groups = (st.groups & !0xffff_ffff) | groups;
        drop(st);
        self.sync_member();
        Ok(())
    }

    /// Puts the socket into MEMBERS or takes it out by its groups as they are now. The look at `st` happens
    /// under MEMBERS (order MEMBERS > st), so two threads joining and leaving at once cannot leave the table
    /// and the socket's groups apart (a stale "member" decided before the lock could be applied last).
    fn sync_member(self: &Arc<Self>) {
        let mut m = MEMBERS.lock();
        let on = {
            let st = self.st.lock();
            st.groups != 0 && !st.released
        };
        let had = m.contains_key(&key(self));
        if on && !had {
            m.insert(key(self), Arc::downgrade(self));
            MEMBER_COUNT.fetch_add(1, Ordering::AcqRel);
        } else if !on && had {
            m.remove(&key(self));
            MEMBER_COUNT.fetch_sub(1, Ordering::AcqRel);
        }
        drop(m);
        if on && !had {
            // The first member's view is the base the next change is compared with (a round with no base
            // only takes one).
            changed();
        }
    }

    /// connect(2): AF_UNSPEC disconnects; a destination other than the kernel needs CAP_NET_ADMIN.
    pub fn connect(self: &Arc<Self>, raw: &[u8], c: &Caller) -> Result<(), Errno> {
        if raw.len() < 2 {
            return Err(EINVAL);
        }
        let family = u16::from_le_bytes([raw[0], raw[1]]);
        if family == AF_UNSPEC {
            let mut st = self.st.lock();
            st.connected = false;
            st.dst_portid = 0;
            st.dst_group = 0;
            return Ok(());
        }
        if family != AF_NETLINK || raw.len() < SOCKADDR_NL {
            return Err(EINVAL);
        }
        let pid = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let groups = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
        if (groups != 0 || pid != 0) && !Self::admin(c) {
            return Err(EPERM);
        }
        self.ensure_bound(c)?;
        let mut st = self.st.lock();
        st.connected = true;
        st.dst_portid = pid;
        st.dst_group = ffs(groups);
        Ok(())
    }

    /// getsockname/getpeername (`netlink_getname`): always succeeds.
    pub fn getname(&self, peer: bool) -> Vec<u8> {
        let st = self.st.lock();
        let (pid, groups) = if peer { (st.dst_portid, group_mask(st.dst_group)) } else { (st.portid, st.groups as u32) };
        let mut v = alloc::vec![0u8; SOCKADDR_NL];
        v[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
        v[4..8].copy_from_slice(&pid.to_le_bytes());
        v[8..12].copy_from_slice(&groups.to_le_bytes());
        v
    }

    // ---- sending ----------------------------------------------------------------------------------------

    /// sendmsg (`netlink_sendmsg`).
    #[allow(clippy::too_many_arguments)]
    pub fn send(self: &Arc<Self>, src: &mut dyn Source, len: usize, flags: u32, nonblock: bool, to: Option<&[u8]>, c: &Caller, creds: Option<Ucred>) -> Result<usize, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        if len == 0 {
            return Err(ENODATA);
        }
        let explicit = to.is_some_and(|t| !t.is_empty());
        let (dst_portid, dst_group) = match to.filter(|t| !t.is_empty()) {
            Some(t) => {
                if t.len() < SOCKADDR_NL || u16::from_le_bytes([t[0], t[1]]) != AF_NETLINK {
                    return Err(EINVAL);
                }
                let pid = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
                let group = ffs(u32::from_le_bytes([t[8], t[9], t[10], t[11]]));
                if (group != 0 || pid != 0) && !Self::admin(c) {
                    return Err(EPERM);
                }
                (pid, group)
            }
            None => {
                let st = self.st.lock();
                (st.dst_portid, st.dst_group)
            }
        };
        self.ensure_bound(c)?;
        if len > self.opts.sndbuf().saturating_sub(32) {
            return Err(EMSGSIZE);
        }
        let mut data = super::buffer(len).map_err(|_| ENOBUFS)?;
        src.fill(&mut data)?;
        let creds = creds.unwrap_or(c.ucred());
        let me = self.portid();
        if dst_group != 0 {
            broadcast(Some(me), dst_group, &data, creds);
        }
        if dst_portid == 0 {
            REQUESTS.inc();
            self.kernel_input(&data, c, explicit);
            return Ok(len);
        }
        let target = lookup(dst_portid).ok_or(ECONNREFUSED)?;
        target.unicast_from_user(&data, me, creds, Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.sndtimeo.load(Ordering::Relaxed)))?;
        Ok(len)
    }

    /// `netlink_rcv_skb` over the kernel's NETLINK_ROUTE input: every complete message in order; a broken
    /// header ends the datagram silently.
    fn kernel_input(self: &Arc<Self>, data: &[u8], c: &Caller, explicit: bool) {
        let (portid, flags) = {
            let st = self.st.lock();
            (st.portid, st.flags)
        };
        let ctx = ReqCtx { portid, admin: Self::admin(c) && (explicit || self.opener_uid == 0), strict: flags & nf::STRICT_CHK != 0 };
        let mut rest = data;
        while rest.len() >= NLMSG_HDRLEN {
            let Some(h) = Hdr::parse(rest) else { return };
            let len = h.len as usize;
            if len < NLMSG_HDRLEN || rest.len() < len {
                return;
            }
            let msg = &rest[..len];
            let res = if h.flags & NLM_F_REQUEST == 0 || h.ty < NLMSG_MIN_TYPE { Ok(Action::Nothing) } else { rtnl::rcv_msg(msg, &ctx) };
            let ack: Option<NlErr> = match res {
                Ok(Action::Nothing) => (h.flags & NLM_F_ACK != 0).then_some(NlErr { errno: 0, msg: None }),
                Ok(Action::Reply(m)) => match self.kernel_unicast(m, 0) {
                    Ok(()) => (h.flags & NLM_F_ACK != 0).then_some(NlErr { errno: 0, msg: None }),
                    Err(e) => Some(NlErr { errno: e, msg: None }),
                },
                Ok(Action::Dump(d)) => self.start_dump(d, portid).err().map(|e| NlErr { errno: e, msg: None }),
                Err(e) => Some(e),
            };
            if let Some(e) = ack {
                self.ack(msg, e);
            }
            rest = &rest[align(len).min(rest.len())..];
        }
    }

    /// `netlink_ack`: NLMSG_ERROR with the error and the request's header, the whole request unless the error is
    /// 0 or NETLINK_CAP_ACK is set (then NLM_F_CAPPED), the text with NETLINK_EXT_ACK.
    fn ack(&self, req: &[u8], e: NlErr) {
        let Some(h) = Hdr::parse(req) else { return };
        let (portid, f) = {
            let st = self.st.lock();
            (st.portid, st.flags)
        };
        let mut fixed = [0u8; 20];
        fixed[0..4].copy_from_slice(&(-(e.errno as i32)).to_le_bytes());
        fixed[4..20].copy_from_slice(&req[..NLMSG_HDRLEN]);
        let capped = e.errno == 0 || f & nf::CAP_ACK != 0;
        let tlv = if f & nf::EXT_ACK != 0 { e.msg } else { None };
        let mut flags = if capped { NLM_F_CAPPED } else { 0 };
        if tlv.is_some() {
            flags |= NLM_F_ACK_TLVS;
        }
        let mut m = Msg::new(NLMSG_ERROR, flags, h.seq, portid, &fixed);
        if !capped {
            m.raw(&req[NLMSG_HDRLEN..h.len as usize]);
        }
        if let Some(t) = tlv {
            m.str(NLMSGERR_ATTR_MSG, t.as_bytes());
        }
        let _ = self.kernel_unicast(m.done(), 0);
    }

    /// A datagram from the kernel (`netlink_unicast` with MSG_DONTWAIT): dropped with an overrun when the queue
    /// is over SO_RCVBUF or congested.
    fn kernel_unicast(&self, data: Vec<u8>, group: u32) -> Result<(), i64> {
        let truesize = data.len() + SKB_OVERHEAD;
        let mut st = self.st.lock();
        if st.released {
            return Err(ECONNREFUSED);
        }
        if st.rmem > self.opts.rcvbuf() || st.congested {
            let report = Self::overrun(&mut st);
            drop(st);
            self.overrun_done(report);
            return Err(EAGAIN);
        }
        st.rmem += truesize;
        st.q.push_back(Pkt { data, from: 0, group, creds: Ucred::default(), truesize });
        drop(st);
        self.wq.wake_all();
        Ok(())
    }

    /// `netlink_overrun`: the first drop of a congestion sets ENOBUFS for the reader (unless NETLINK_NO_ENOBUFS).
    fn overrun(st: &mut St) -> bool {
        if st.flags & nf::NO_ENOBUFS != 0 {
            return false;
        }
        if !st.congested {
            st.congested = true;
            st.err = ENOBUFS;
            return true;
        }
        false
    }

    fn overrun_done(&self, reported: bool) {
        OVERRUNS.inc();
        if reported {
            klog::event!(Info, "net", "netlink.overrun", portid = self.portid(); "a netlink reader's queue is full: ENOBUFS, later messages are dropped until it reads");
            self.wq.wake_all();
        }
    }

    /// `netlink_unicast` to a user socket (`netlink_attachskb`): waits for room within the sender's timeout.
    fn unicast_from_user(&self, data: &[u8], from: u32, creds: Ucred, dl: Deadline) -> Result<(), Errno> {
        {
            let st = self.st.lock();
            if st.connected && st.dst_portid != from {
                return Err(ECONNREFUSED);
            }
        }
        let room = || {
            let st = self.st.lock();
            st.released || (st.rmem <= self.opts.rcvbuf() && !st.congested)
        };
        if !wait(&self.wq, dl, room)? {
            return Err(EAGAIN);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(data.len()).map_err(|_| ENOBUFS)?;
        v.extend_from_slice(data);
        let mut st = self.st.lock();
        if st.released {
            return Err(ECONNREFUSED);
        }
        st.rmem += v.len() + SKB_OVERHEAD;
        st.q.push_back(Pkt { truesize: v.len() + SKB_OVERHEAD, data: v, from, group: 0, creds });
        drop(st);
        self.wq.wake_all();
        Ok(())
    }

    /// A broadcast datagram (`do_one_broadcast`): skipped by the sender, dropped with an overrun when full.
    fn deliver_group(&self, data: &[u8], from: u32, group: u32, creds: Ucred) -> bool {
        let mut st = self.st.lock();
        if st.released || st.groups & (1u64 << (group - 1)) == 0 || (from != 0 && st.portid == from) {
            return false;
        }
        if st.rmem > self.opts.rcvbuf() || st.congested {
            let r = Self::overrun(&mut st);
            drop(st);
            self.overrun_done(r);
            return false;
        }
        let mut v = Vec::new();
        if v.try_reserve_exact(data.len()).is_err() {
            return false;
        }
        v.extend_from_slice(data);
        st.rmem += v.len() + SKB_OVERHEAD;
        st.q.push_back(Pkt { truesize: v.len() + SKB_OVERHEAD, data: v, from, group, creds });
        drop(st);
        self.wq.wake_all();
        true
    }

    // ---- dumps -------------------------------------------------------------------------------------------

    /// `__netlink_dump_start`: EBUSY while a dump runs; the first part is queued at once.
    pub(super) fn start_dump(&self, d: Dump, requester: u32) -> Result<(), i64> {
        {
            let mut st = self.st.lock();
            if st.dump.is_some() {
                return Err(EBUSY);
            }
            st.dump = Some(d);
            st.dump_pid = requester;
        }
        self.dump_step()
    }

    /// `netlink_dump`: one datagram of the running dump, as large as the reader's buffers have been (at least a
    /// page), with NLMSG_DONE once nothing is left (in a datagram of its own after the last messages).
    fn dump_step(&self) -> Result<(), i64> {
        let mut st = self.st.lock();
        let rcvbuf = self.opts.rcvbuf();
        if st.rmem >= rcvbuf {
            return Err(ENOBUFS);
        }
        let cap = GOODSIZE.max(st.max_recvmsg_len);
        let (pid, ext) = (st.dump_pid, st.flags & nf::EXT_ACK != 0);
        // Another reader of the same socket may have finished the dump in between: nothing to do (Linux's
        // unlocked `cb_running` look would report EINVAL to this reader; a race, not an error of the program).
        let Some(d) = st.dump.as_mut() else { return Ok(()) };
        let mut skb: Vec<u8> = Vec::new();
        if d.done_errno > 0 {
            while let Some(m) = d.msgs.front() {
                if skb.len() + m.len() > cap {
                    break;
                }
                if let Some(m) = d.msgs.pop_front() {
                    skb.extend_from_slice(&m);
                }
            }
            d.done_errno = if !skb.is_empty() {
                skb.len().min(i32::MAX as usize) as i32
            } else if d.msgs.is_empty() {
                0
            } else {
                -(EMSGSIZE as i32)
            };
        }
        let finished = d.done_errno <= 0 && cap - skb.len() >= NLMSG_HDRLEN + 4;
        if finished {
            let mut flags = NLM_F_MULTI | d.answer_flags;
            let msg = if ext { d.done_msg } else { None };
            if msg.is_some() {
                flags |= NLM_F_ACK_TLVS;
            }
            let mut m = Msg::new(NLMSG_DONE, flags, d.seq, pid, &d.done_errno.to_le_bytes());
            if let Some(t) = msg {
                m.str(NLMSGERR_ATTR_MSG, t.as_bytes());
            }
            skb.extend_from_slice(&m.done());
            st.dump = None;
        }
        st.rmem += cap + SKB_OVERHEAD;
        st.q.push_back(Pkt { data: skb, from: 0, group: 0, creds: Ucred::default(), truesize: cap + SKB_OVERHEAD });
        drop(st);
        self.wq.wake_all();
        Ok(())
    }

    // ---- receiving ---------------------------------------------------------------------------------------

    /// recvmsg (`netlink_recvmsg`): one datagram (MSG_TRUNC when cut), the sender's address, NETLINK_PKTINFO's
    /// group; a running dump makes its next part once the queue is at most half of SO_RCVBUF.
    pub fn recv(&self, size: usize, flags: u32, nonblock: bool, sink: &mut dyn FnMut(&[u8]) -> Result<(), Errno>) -> Result<Received, Errno> {
        if flags & MSG_OOB != 0 {
            return Err(EOPNOTSUPP);
        }
        let dl = Deadline::new(nonblock || flags & MSG_DONTWAIT != 0, self.opts.rcvtimeo.load(Ordering::Relaxed));
        let ready = || {
            let st = self.st.lock();
            st.err != 0 || !st.q.is_empty()
        };
        loop {
            {
                let mut st = self.st.lock();
                // `sock_error` comes before the queue.
                if st.err != 0 {
                    let e = st.err;
                    st.err = 0;
                    return Err(e);
                }
                if !st.q.is_empty() {
                    break;
                }
            }
            if !wait(&self.wq, dl, ready)? {
                return Err(EAGAIN);
            }
        }
        let mut st = self.st.lock();
        let p = if flags & MSG_PEEK != 0 {
            st.q.front().map(|p| (p.data.clone(), p.from, p.group, p.creds))
        } else {
            st.q.pop_front().map(|p| {
                st.rmem = st.rmem.saturating_sub(p.truesize);
                (p.data, p.from, p.group, p.creds)
            })
        };
        let Some((data, from, group, creds)) = p else { return Err(EAGAIN) };
        // `netlink_rcv_wake`: an empty queue ends a congestion, and a socket that is not congested wakes the
        // senders waiting for room (a root program's unicast in `unicast_from_user`).
        if st.rmem == 0 {
            st.congested = false;
        }
        let wake_senders = flags & MSG_PEEK == 0 && !st.congested;
        st.max_recvmsg_len = st.max_recvmsg_len.max(size).min(MAX_RECVMSG_LEN);
        let pktinfo = (st.flags & nf::PKTINFO != 0).then_some(group);
        let run_dump = st.dump.is_some() && st.rmem <= self.opts.rcvbuf() / 2;
        drop(st);
        if wake_senders {
            self.wq.wake_all();
        }
        let n = data.len().min(size);
        let copied = sink(&data[..n]);
        if run_dump && let Err(e) = self.dump_step() {
            self.set_error(e);
        }
        copied?;
        let mut r = Received { n: if flags & MSG_TRUNC != 0 { data.len() } else { n }, ..Default::default() };
        if n < data.len() {
            r.flags |= MSG_TRUNC;
        }
        let mut name = alloc::vec![0u8; SOCKADDR_NL];
        name[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
        name[4..8].copy_from_slice(&from.to_le_bytes());
        name[8..12].copy_from_slice(&group_mask(group).to_le_bytes());
        r.from = Some(name);
        r.pktinfo = pktinfo;
        if self.opts.has(super::flag::PASSCRED) {
            r.creds = Some(creds);
        }
        Ok(r)
    }

    /// `datagram_poll`.
    pub fn poll(&self) -> u32 {
        use vfs::Events as E;
        let st = self.st.lock();
        let mut m = E::OUT;
        if st.err != 0 {
            m |= E::ERR;
        }
        if !st.q.is_empty() {
            m |= E::IN;
        }
        m
    }

    pub fn take_error(&self) -> i64 {
        let mut st = self.st.lock();
        core::mem::take(&mut st.err)
    }

    pub fn set_error(&self, e: i64) {
        self.st.lock().err = e;
        self.wq.wake_all();
    }

    // ---- options -----------------------------------------------------------------------------------------

    /// setsockopt(SOL_NETLINK) (`netlink_setsockopt`); `val` is None when optlen < 4 (read as 0).
    pub fn setsockopt(self: &Arc<Self>, name: u32, val: u32, c: &Caller) -> Result<(), Errno> {
        let bit = match name {
            NETLINK_PKTINFO => nf::PKTINFO,
            NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP => {
                let mut st = self.st.lock();
                st.ngroups = rtnl::RTNLGRP_MAX;
                if val == 0 || val > st.ngroups {
                    return Err(EINVAL);
                }
                if name == NETLINK_ADD_MEMBERSHIP {
                    st.groups |= 1u64 << (val - 1);
                } else {
                    st.groups &= !(1u64 << (val - 1));
                }
                drop(st);
                self.sync_member();
                return Ok(());
            }
            NETLINK_BROADCAST_ERROR => nf::BROADCAST_ERROR,
            NETLINK_NO_ENOBUFS => {
                let mut st = self.st.lock();
                if val != 0 {
                    st.flags |= nf::NO_ENOBUFS;
                    st.congested = false;
                } else {
                    st.flags &= !nf::NO_ENOBUFS;
                }
                drop(st);
                self.wq.wake_all();
                return Ok(());
            }
            NETLINK_LISTEN_ALL_NSID => {
                if c.uid != 0 {
                    return Err(EPERM);
                }
                nf::LISTEN_ALL_NSID
            }
            NETLINK_CAP_ACK => nf::CAP_ACK,
            NETLINK_EXT_ACK => nf::EXT_ACK,
            NETLINK_GET_STRICT_CHK => nf::STRICT_CHK,
            _ => return Err(ENOPROTOOPT),
        };
        let mut st = self.st.lock();
        if val != 0 {
            st.flags |= bit;
        } else {
            st.flags &= !bit;
        }
        Ok(())
    }

    /// getsockopt(SOL_NETLINK): (value bytes, the length written back). `len` is the caller's (>= 0).
    pub fn getsockopt(&self, name: u32, len: usize) -> Result<(Vec<u8>, u32), Errno> {
        let st = self.st.lock();
        let bit = match name {
            NETLINK_PKTINFO => nf::PKTINFO,
            NETLINK_BROADCAST_ERROR => nf::BROADCAST_ERROR,
            NETLINK_NO_ENOBUFS => nf::NO_ENOBUFS,
            NETLINK_LIST_MEMBERSHIPS => {
                // As many 32-bit words as fit; the length is always the full group bitmap's.
                let full = st.ngroups.div_ceil(8).next_multiple_of(4) as usize;
                let mut v = Vec::new();
                let mut pos = 0usize;
                while pos * 8 < st.ngroups as usize && len.saturating_sub(pos) >= 4 {
                    v.extend_from_slice(&((st.groups >> (pos * 8)) as u32).to_le_bytes());
                    pos += 4;
                }
                return Ok((v, full as u32));
            }
            NETLINK_CAP_ACK => nf::CAP_ACK,
            NETLINK_EXT_ACK => nf::EXT_ACK,
            NETLINK_GET_STRICT_CHK => nf::STRICT_CHK,
            _ => return Err(ENOPROTOOPT),
        };
        if len < 4 {
            return Err(EINVAL);
        }
        Ok((((st.flags & bit != 0) as u32).to_le_bytes().to_vec(), 4))
    }

    /// The socket's last file went (`netlink_release`): out of the tables, queue and dump dropped.
    pub fn release(&self) {
        let (portid, bound) = {
            let mut st = self.st.lock();
            st.released = true;
            st.q.clear();
            st.rmem = 0;
            st.dump = None;
            (st.portid, st.bound)
        };
        if bound {
            let mut t = PORTS.write();
            if t.get(&portid).is_some_and(|w| core::ptr::eq(w.as_ptr(), self)) {
                t.remove(&portid);
            }
        }
        let mut m = MEMBERS.lock();
        if m.remove(&key(self)).is_some() {
            MEMBER_COUNT.fetch_sub(1, Ordering::AcqRel);
        }
        drop(m);
        self.wq.wake_all();
    }
}

fn ffs(x: u32) -> u32 {
    if x == 0 { 0 } else { x.trailing_zeros() + 1 }
}

fn group_mask(group: u32) -> u32 {
    if group == 0 || group > 32 { 0 } else { 1 << (group - 1) }
}

fn lookup(portid: u32) -> Option<Arc<NetlinkSock>> {
    PORTS.read().get(&portid).and_then(|w| w.upgrade())
}

fn members() -> Vec<Arc<NetlinkSock>> {
    MEMBERS.lock().values().filter_map(|w| w.upgrade()).collect()
}

/// `netlink_broadcast` to every member of `group` but the sender: how many took it.
fn broadcast(from: Option<u32>, group: u32, data: &[u8], creds: Ucred) -> usize {
    if group == 0 || group > rtnl::RTNLGRP_MAX || MEMBER_COUNT.load(Ordering::Acquire) == 0 {
        return 0;
    }
    members().iter().filter(|s| s.deliver_group(data, from.unwrap_or(0), group, creds)).count()
}

// ---- the kernel's notifications --------------------------------------------------------------------------

/// What the last notification round saw of each interface: (index, flags, addresses).
type Snap = Vec<(u32, u32, Vec<ifview::Addr>)>;
static LAST: SpinLock<Option<Snap>> = SpinLock::named("net.netlink.last", None);

fn snap(links: &[ifview::Link]) -> Snap {
    links.iter().map(|l| (l.index, l.flags(), l.addrs.iter().map(|a| ifview::Addr { valid_s: 0, preferred_s: 0, ..a.clone() }).collect())).collect()
}

/// Only one notification round runs at a time (`changed` may come from the stack's poll thread and from
/// `set_static` on a program's thread at once): a caller that finds a round running leaves a note, and the
/// running caller goes once more. No caller waits (B4), and the snapshots in LAST are stored in the order they
/// were taken, so no change is announced twice, backwards or out of order.
static ROUND_RUNNING: AtomicBool = AtomicBool::new(false);
static ROUND_PENDING: AtomicBool = AtomicBool::new(false);

/// A card's link or address may have changed (the stack's poll thread after link and DHCP events, a static
/// address, a new card), or a socket became the first member: RTM_NEWLINK to RTNLGRP_LINK for a new card or
/// changed flags, RTM_DELADDR/RTM_NEWADDR to RTNLGRP_IPV4_IFADDR for addresses that went or came. Without
/// members a round only drops the old view.
pub fn changed() {
    ROUND_PENDING.store(true, Ordering::Release);
    loop {
        if ROUND_RUNNING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        while ROUND_PENDING.swap(false, Ordering::AcqRel) {
            round();
        }
        ROUND_RUNNING.store(false, Ordering::Release);
        if !ROUND_PENDING.load(Ordering::Acquire) {
            return;
        }
    }
}

/// One notification round (serialised by `changed`); the broadcasts go out in the round, in order.
fn round() {
    if MEMBER_COUNT.load(Ordering::Acquire) == 0 {
        *LAST.lock() = None;
        return;
    }
    let links = ifview::links();
    let now = snap(&links);
    // Without a base (the first member just joined) the round only takes one.
    let Some(before) = LAST.lock().replace(now.clone()) else { return };
    let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
    for (l, (index, flags, addrs)) in links.iter().zip(now.iter()) {
        let old = before.iter().find(|b| b.0 == *index);
        match old {
            None => out.push((rtnl::RTNLGRP_LINK, rtnl::link_msg(l, rtnl::RTM_NEWLINK, 0, 0, 0, u32::MAX))),
            Some(o) if o.1 != *flags => out.push((rtnl::RTNLGRP_LINK, rtnl::link_msg(l, rtnl::RTM_NEWLINK, 0, 0, 0, 0))),
            _ => {}
        }
        let gone: Vec<&ifview::Addr> = old.map(|o| o.2.iter().filter(|a| !addrs.contains(a)).collect()).unwrap_or_default();
        for a in gone {
            out.push((rtnl::RTNLGRP_IPV4_IFADDR, rtnl::addr_msg(l, a, rtnl::RTM_DELADDR, 0, 0, 0)));
        }
        for a in l.addrs.iter() {
            let plain = ifview::Addr { valid_s: 0, preferred_s: 0, ..a.clone() };
            if !old.is_some_and(|o| o.2.contains(&plain)) {
                out.push((rtnl::RTNLGRP_IPV4_IFADDR, rtnl::addr_msg(l, a, rtnl::RTM_NEWADDR, 0, 0, 0)));
            }
        }
    }
    for (group, m) in out {
        let n = broadcast(None, group, &m, Ucred::default());
        NOTIFIED.add(n as u64);
        let ty = Hdr::parse(&m).map(|h| h.ty).unwrap_or(0);
        klog::event!(Debug, "net", "netlink.notify", msg_type = ty, group = group, readers = n);
    }
}

/// No notification round runs or is due (host tests wait for this after a change).
#[cfg(test)]
pub(crate) fn rounds_idle() -> bool {
    !ROUND_RUNNING.load(Ordering::Acquire) && !ROUND_PENDING.load(Ordering::Acquire)
}

/// Is the socket in MEMBERS (host tests).
#[cfg(test)]
pub(crate) fn is_member(s: &Arc<NetlinkSock>) -> bool {
    MEMBERS.lock().contains_key(&key(s))
}

/// Netlink sockets bound now (for `summary` and tests).
pub fn bound() -> usize {
    PORTS.read().values().filter(|w| w.strong_count() > 0).count()
}
