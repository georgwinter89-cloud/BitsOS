//! rtnetlink, the kernel side of NETLINK_ROUTE (linux-6.6/net/core/rtnetlink.c `rtnetlink_rcv_msg`,
//! `rtnl_getlink`, `rtnl_dump_ifinfo`, `rtnl_fill_ifinfo`, `rtnl_dump_all`; net/ipv4/devinet.c `inet_dump_ifaddr`,
//! `inet_fill_ifaddr`): what `getifaddrs`, `if_nameindex`, `ip link`/`ip addr` and Chromium's address tracker ask.
//!
//! V2 answers RTM_GETLINK (one link or a dump) and RTM_GETADDR (dumps; IPv4, and an empty IPv6 dump because V2 has
//! no IPv6 addresses), from the interface view (`ifview`). Changing requests (NEW/DEL/SET) are EPERM for programs
//! without root, as on Linux; for root they are EOPNOTSUPP: the kernel's stack owns the network configuration.
//! Every other GET type is EOPNOTSUPP (logged once per type: the to-do list of what programs asked for).

use super::nlmsg::*;
use crate::ifview::{self, Addr, Link};
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use uapi::errno::*;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_DELLINK: u16 = 17;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_DELADDR: u16 = 21;
pub const RTM_GETADDR: u16 = 22;
/// The highest message type Linux 6.6 knows (`RTM_MAX`, after RTM_GETTUNNEL).
pub const RTM_MAX: u16 = 123;

pub const AF_UNSPEC: u8 = 0;
pub const AF_INET: u8 = 2;
pub const AF_INET6: u8 = 10;

/// Multicast groups (`enum rtnetlink_groups`) and their number in Linux 6.6 (`RTNLGRP_MAX`).
pub const RTNLGRP_LINK: u32 = 1;
pub const RTNLGRP_IPV4_IFADDR: u32 = 5;
pub const RTNLGRP_MAX: u32 = 33;

// IFLA_* (include/uapi/linux/if_link.h)
const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_QDISC: u16 = 6;
const IFLA_STATS: u16 = 7;
const IFLA_MASTER: u16 = 10;
const IFLA_TXQLEN: u16 = 13;
const IFLA_MAP: u16 = 14;
const IFLA_OPERSTATE: u16 = 16;
const IFLA_LINKMODE: u16 = 17;
const IFLA_LINKINFO: u16 = 18;
const IFLA_STATS64: u16 = 23;
const IFLA_GROUP: u16 = 27;
const IFLA_EXT_MASK: u16 = 29;
const IFLA_PROMISCUITY: u16 = 30;
const IFLA_NUM_TX_QUEUES: u16 = 31;
const IFLA_NUM_RX_QUEUES: u16 = 32;
const IFLA_CARRIER: u16 = 33;
const IFLA_CARRIER_CHANGES: u16 = 35;
const IFLA_PROTO_DOWN: u16 = 39;
const IFLA_GSO_MAX_SEGS: u16 = 40;
const IFLA_GSO_MAX_SIZE: u16 = 41;
const IFLA_XDP: u16 = 43;
const IFLA_TARGET_NETNSID: u16 = 46;
const IFLA_CARRIER_UP_COUNT: u16 = 47;
const IFLA_CARRIER_DOWN_COUNT: u16 = 48;
const IFLA_MIN_MTU: u16 = 50;
const IFLA_MAX_MTU: u16 = 51;
const IFLA_ALT_IFNAME: u16 = 53;
const IFLA_PERM_ADDRESS: u16 = 54;
const IFLA_GRO_MAX_SIZE: u16 = 58;
const IFLA_TSO_MAX_SIZE: u16 = 59;
const IFLA_TSO_MAX_SEGS: u16 = 60;
const IFLA_ALLMULTI: u16 = 61;
const IFLA_GSO_IPV4_MAX_SIZE: u16 = 63;
const IFLA_GRO_IPV4_MAX_SIZE: u16 = 64;
const IFLA_XDP_ATTACHED: u16 = 2;

// IFA_* (include/uapi/linux/if_addr.h)
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;
const IFA_CACHEINFO: u16 = 6;
const IFA_FLAGS: u16 = 8;
const IFA_TARGET_NETNSID: u16 = 10;

/// `GSO_MAX_SEGS`, `GSO_LEGACY_MAX_SIZE` (the default of every limit), the loopback's `GSO_MAX_SIZE` TSO limit.
const GSO_MAX_SEGS: u32 = 65535;
const GSO_LEGACY_MAX_SIZE: u32 = 65536;
const LO_TSO_MAX_SIZE: u32 = 524_280;

/// Who sent a request.
pub struct ReqCtx {
    /// The sender's port id (the answers carry it).
    pub portid: u32,
    /// CAP_NET_ADMIN as `netlink_net_capable` checks it (the sender is root and so was the opener, or the
    /// message was addressed explicitly).
    pub admin: bool,
    /// NETLINK_GET_STRICT_CHK on the sending socket.
    pub strict: bool,
}

/// A refusal: the errno and, for NETLINK_EXT_ACK, Linux's text.
#[derive(Debug, PartialEq, Eq)]
pub struct NlErr {
    pub errno: i64,
    pub msg: Option<&'static str>,
}

impl NlErr {
    fn new(errno: i64) -> NlErr {
        NlErr { errno, msg: None }
    }
    fn msg(errno: i64, msg: &'static str) -> NlErr {
        NlErr { errno, msg: Some(msg) }
    }
}

/// What a request asks the socket to do.
pub enum Action {
    /// Nothing besides an ack when NLM_F_ACK asks for one (`return 0`).
    Nothing,
    /// One answer to the sender, then an ack if asked.
    Reply(Vec<u8>),
    /// A dump: its messages are handed out as the reader reads (no ack: `-EINTR` in Linux).
    Dump(Dump),
}

/// A dump in progress (`struct netlink_callback`): the messages (made when the dump started, from one look at
/// the interfaces) and what NLMSG_DONE will carry.
pub struct Dump {
    pub msgs: VecDeque<Vec<u8>>,
    /// Linux's `dump_done_errno`: > 0 while messages may follow, 0 done, < 0 the error DONE reports.
    pub done_errno: i32,
    pub done_msg: Option<&'static str>,
    pub seq: u32,
    /// NLM_F_DUMP_FILTERED when the request filtered (`cb->answer_flags`).
    pub answer_flags: u16,
}

impl Dump {
    fn new(seq: u32, msgs: VecDeque<Vec<u8>>, answer_flags: u16) -> Dump {
        Dump { msgs, done_errno: i32::MAX, done_msg: None, seq, answer_flags }
    }
    fn failed(seq: u32, e: NlErr) -> Dump {
        Dump { msgs: VecDeque::new(), done_errno: -(e.errno as i32), done_msg: e.msg, seq, answer_flags: 0 }
    }
}

/// `rtnl_msgtype_kind`: NEW, DEL, GET, SET by the low two bits.
fn is_get(ty: u16) -> bool {
    (ty - 16) % 4 == 2
}

static UNSUPPORTED_SEEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Logs a message type V2 does not answer, once per type (types 16..=123 fit two words).
fn unsupported(ty: u16, why: &'static str) {
    let bit = (ty - 16) as usize;
    let (w, b) = (bit / 64, 1u64 << (bit % 64));
    if w < UNSUPPORTED_SEEN.len() && UNSUPPORTED_SEEN[w].fetch_or(b, Ordering::Relaxed) & b == 0 {
        klog::event!(Info, "net", "netlink.unsupported", msg_type = ty, why = why; "a program asked rtnetlink for something V2 does not answer");
    }
}

/// `rtnetlink_rcv_msg` for one request message (`msg`: its declared length; header type >= NLMSG_MIN_TYPE,
/// NLM_F_REQUEST set).
pub fn rcv_msg(msg: &[u8], ctx: &ReqCtx) -> Result<Action, NlErr> {
    let Some(h) = Hdr::parse(msg) else { return Ok(Action::Nothing) };
    if h.ty > RTM_MAX {
        return Err(NlErr::new(EOPNOTSUPP));
    }
    let body = &msg[NLMSG_HDRLEN..];
    // "All the messages must have at least 1 byte length."
    if body.is_empty() {
        return Ok(Action::Nothing);
    }
    let family = body[0];
    if !is_get(h.ty) {
        if !ctx.admin {
            return Err(NlErr::new(EPERM));
        }
        unsupported(h.ty, "changing the network configuration");
        return Err(NlErr::new(EOPNOTSUPP));
    }
    if h.flags & NLM_F_DUMP != 0 {
        return match h.ty {
            RTM_GETLINK => Ok(Action::Dump(dump_links(&h, body, ctx))),
            RTM_GETADDR => Ok(Action::Dump(dump_addrs(&h, body, family, ctx))),
            _ => {
                unsupported(h.ty, "dump");
                Err(NlErr::new(EOPNOTSUPP))
            }
        };
    }
    match h.ty {
        RTM_GETLINK => getlink(&h, body, ctx).map(Action::Reply),
        // Only IPv6 registers a doit for RTM_GETADDR (addrconf.c `inet6_rtm_getaddr`); AF_INET and the rest have
        // a dump only (EOPNOTSUPP below).
        RTM_GETADDR if family == AF_INET6 => Err(getaddr6(body, ctx)),
        _ => {
            unsupported(h.ty, "get");
            Err(NlErr::new(EOPNOTSUPP))
        }
    }
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// `ifinfomsg` fields of a request: (pad, type, index, flags, change).
fn ifinfo(body: &[u8]) -> (u8, u16, i32, u32, u32) {
    (body[1], u16::from_le_bytes([body[2], body[3]]), u32_at(body, 4) as i32, u32_at(body, 8), u32_at(body, 12))
}

const IFINFOMSG: usize = 16;
const IFADDRMSG: usize = 8;

/// `nlmsg_parse_deprecated_strict`'s leftover check (NL_VALIDATE_TRAILING), with Linux's text.
fn strict_attrs(b: &[u8]) -> Result<Vec<(u16, &[u8])>, NlErr> {
    attrs_strict(b).map_err(|e| NlErr::msg(e, "bytes leftover after parsing attributes"))
}

/// An NLA_STRING interface name (policy `.len = IFNAMSIZ-1`): ERANGE when empty or longer; the name up to its
/// first NUL (`nla_strscpy`).
fn name_attr(d: &[u8]) -> Result<&[u8], NlErr> {
    let n = if d.last() == Some(&0) { d.len() - 1 } else { d.len() };
    if d.is_empty() || n > ifview::IFNAME_MAX {
        return Err(NlErr::new(ERANGE));
    }
    let d = &d[..n];
    Ok(&d[..d.iter().position(|b| *b == 0).unwrap_or(d.len())])
}

/// `rtnl_getlink`: one link by index, else by IFLA_IFNAME / IFLA_ALT_IFNAME.
fn getlink(h: &Hdr, body: &[u8], ctx: &ReqCtx) -> Result<Vec<u8>, NlErr> {
    if body.len() < IFINFOMSG {
        return Err(NlErr::msg(EINVAL, "Invalid header for get link"));
    }
    let (pad, ty, index, flags, change) = ifinfo(body);
    let attrs = if ctx.strict {
        if pad != 0 || ty != 0 || flags != 0 || change != 0 {
            return Err(NlErr::msg(EINVAL, "Invalid values in header for get link request"));
        }
        let a = strict_attrs(&body[IFINFOMSG..])?;
        if a.iter().any(|(t, _)| !matches!(*t, IFLA_IFNAME | IFLA_ALT_IFNAME | IFLA_EXT_MASK | IFLA_TARGET_NETNSID)) {
            return Err(NlErr::msg(EINVAL, "Unsupported attribute in get link request"));
        }
        a
    } else {
        attrs(&body[IFINFOMSG..])
    };
    let mut name = None;
    for (t, d) in &attrs {
        match *t {
            IFLA_IFNAME | IFLA_ALT_IFNAME => {
                let n = name_attr(d)?;
                if *t == IFLA_IFNAME || name.is_none() {
                    name = Some(n);
                }
            }
            // No network namespaces besides the one: any id names none (`rtnl_get_net_ns_capable`).
            IFLA_TARGET_NETNSID => return Err(NlErr::msg(EINVAL, "Invalid target network namespace id")),
            _ => {}
        }
    }
    let link = if index > 0 {
        ifview::by_index(index as u32)
    } else if let Some(n) = name {
        ifview::by_name(n)
    } else {
        return Err(NlErr::new(EINVAL));
    };
    let link = link.ok_or(NlErr::new(ENODEV))?;
    Ok(link_msg(&link, RTM_NEWLINK, 0, h.seq, ctx.portid, 0))
}

// IFA_* types in `ifa_ipv6_policy` with a minimum length, and IFA_MAX.
const IFA_RT_PRIORITY: u16 = 9;
const IFA_PROTO: u16 = 11;
const IFA_MAX: u16 = 11;

/// `ifa_ipv6_policy`'s minimum lengths (`validate_nla`): a shorter attribute is ERANGE.
fn ifa6_policy(attrs: &[(u16, &[u8])]) -> Result<(), NlErr> {
    for (t, d) in attrs {
        let min = match *t {
            IFA_ADDRESS | IFA_LOCAL | IFA_CACHEINFO => 16,
            IFA_FLAGS | IFA_RT_PRIORITY | IFA_TARGET_NETNSID => 4,
            IFA_PROTO => 1,
            _ => 0,
        };
        if d.len() < min {
            return Err(NlErr::msg(ERANGE, "Attribute failed policy validation"));
        }
    }
    Ok(())
}

/// `inet6_rtm_getaddr` (with `inet6_rtm_valid_getaddr_req`): one IPv6 address. V2 has none, so a well-formed
/// request ends in EADDRNOTAVAIL; the checks before it are Linux's, in its order (always an error here).
fn getaddr6(body: &[u8], ctx: &ReqCtx) -> NlErr {
    if body.len() < IFADDRMSG {
        return NlErr::msg(EINVAL, "ipv6: Invalid header for get address request");
    }
    let attrs = if ctx.strict {
        if body[1] != 0 || body[2] != 0 || body[3] != 0 {
            return NlErr::msg(EINVAL, "ipv6: Invalid values in header for get address request");
        }
        // `nlmsg_parse_deprecated_strict`: each attribute's type and policy in order, then the leftover bytes.
        let a = attrs(&body[IFADDRMSG..]);
        for (t, d) in &a {
            if *t == 0 || *t > IFA_MAX {
                return NlErr::msg(EINVAL, "Unknown attribute type");
            }
            if let Err(e) = ifa6_policy(&[(*t, d)]) {
                return e;
            }
        }
        if let Err(e) = strict_attrs(&body[IFADDRMSG..]) {
            return e;
        }
        if a.iter().any(|(t, _)| !matches!(*t, IFA_TARGET_NETNSID | IFA_ADDRESS | IFA_LOCAL)) {
            return NlErr::msg(EINVAL, "ipv6: Unsupported attribute in get address request");
        }
        a
    } else {
        let a = attrs(&body[IFADDRMSG..]);
        if let Err(e) = ifa6_policy(&a) {
            return e;
        }
        a
    };
    // No network namespaces besides the one: any id names none (`rtnl_get_net_ns_capable`, no text).
    if attrs.iter().any(|(t, _)| *t == IFA_TARGET_NETNSID) {
        return NlErr::new(EINVAL);
    }
    if !attrs.iter().any(|(t, _)| matches!(*t, IFA_ADDRESS | IFA_LOCAL)) {
        return NlErr::new(EINVAL);
    }
    NlErr::new(EADDRNOTAVAIL)
}

/// `rtnl_dump_ifinfo` (with `rtnl_valid_dump_ifinfo_req`).
fn dump_links(h: &Hdr, body: &[u8], ctx: &ReqCtx) -> Dump {
    let mut flags = NLM_F_MULTI;
    let mut master = 0u32;
    if ctx.strict {
        if body.len() < IFINFOMSG {
            return Dump::failed(h.seq, NlErr::msg(EINVAL, "Invalid header for link dump"));
        }
        let (pad, ty, index, fl, change) = ifinfo(body);
        if pad != 0 || ty != 0 || fl != 0 || change != 0 {
            return Dump::failed(h.seq, NlErr::msg(EINVAL, "Invalid values in header for link dump request"));
        }
        if index != 0 {
            return Dump::failed(h.seq, NlErr::msg(EINVAL, "Filter by device index not supported for link dumps"));
        }
        let a = match strict_attrs(&body[IFINFOMSG..]) {
            Ok(a) => a,
            Err(e) => return Dump::failed(h.seq, e),
        };
        for (t, d) in a {
            match t {
                IFLA_EXT_MASK | IFLA_LINKINFO => {}
                IFLA_MASTER if d.len() >= 4 => master = u32_at(d, 0),
                IFLA_TARGET_NETNSID => return Dump::failed(h.seq, NlErr::msg(EINVAL, "Invalid target network namespace id")),
                _ => return Dump::failed(h.seq, NlErr::msg(EINVAL, "Unsupported attribute in link dump request")),
            }
        }
    } else {
        // Old iproute2 sent a 1-byte rtgenmsg: the attributes follow whichever header fits the length.
        let hdr = if body.len() < IFINFOMSG { 1 } else { IFINFOMSG };
        for (t, d) in attrs(&body[align(hdr).min(body.len())..]) {
            if t == IFLA_MASTER && d.len() >= 4 {
                master = u32_at(d, 0);
            }
            if t == IFLA_TARGET_NETNSID {
                return Dump::failed(h.seq, NlErr::msg(EINVAL, "Invalid target network namespace id"));
            }
        }
    }
    // No V2 interface has a master (no bridges or bonds): a master filter leaves nothing.
    if master != 0 {
        flags |= NLM_F_DUMP_FILTERED;
    }
    let msgs = ifview::links().iter().filter(|_| master == 0).map(|l| link_msg(l, RTM_NEWLINK, flags, h.seq, ctx.portid, 0)).collect();
    Dump::new(h.seq, msgs, 0)
}

/// `inet_dump_ifaddr` (and `rtnl_dump_all` for AF_UNSPEC and families without their own address dump): the
/// IPv4 addresses, then the (empty) IPv6 ones.
fn dump_addrs(h: &Hdr, body: &[u8], family: u8, ctx: &ReqCtx) -> Dump {
    let families: &[u8] = match family {
        AF_INET => &[AF_INET],
        AF_INET6 => &[AF_INET6],
        _ => &[AF_INET, AF_INET6],
    };
    let mut filter = 0u32;
    let mut flags = NLM_F_MULTI;
    if ctx.strict {
        let v6 = families[0] == AF_INET6;
        let pick = |a: &'static str, b: &'static str| if v6 { b } else { a };
        if body.len() < IFADDRMSG {
            return Dump::failed(h.seq, NlErr::msg(EINVAL, pick("ipv4: Invalid header for address dump request", "ipv6: Invalid header for address dump request")));
        }
        if body[1] != 0 || body[2] != 0 || body[3] != 0 {
            return Dump::failed(h.seq, NlErr::msg(EINVAL, pick("ipv4: Invalid values in header for address dump request", "ipv6: Invalid values in header for address dump request")));
        }
        filter = u32_at(body, 4);
        if filter != 0 {
            flags |= NLM_F_DUMP_FILTERED;
        }
        let a = match strict_attrs(&body[IFADDRMSG..]) {
            Ok(a) => a,
            Err(e) => return Dump::failed(h.seq, e),
        };
        // Only IFA_TARGET_NETNSID is allowed, and no id names a namespace here.
        if let Some((t, _)) = a.first() {
            let e = if *t == IFA_TARGET_NETNSID {
                NlErr::msg(EINVAL, pick("ipv4: Invalid target network namespace id", "ipv6: Invalid target network namespace id"))
            } else {
                NlErr::msg(EINVAL, pick("ipv4: Unsupported attribute in dump request", "ipv6: Unsupported attribute in dump request"))
            };
            return Dump::failed(h.seq, e);
        }
    }
    let links = ifview::links();
    if filter != 0 && !links.iter().any(|l| l.index == filter) {
        let mut d = Dump::failed(h.seq, NlErr::new(ENODEV));
        d.answer_flags = NLM_F_DUMP_FILTERED;
        return d;
    }
    let mut msgs = VecDeque::new();
    if families.contains(&AF_INET) {
        for l in links.iter().filter(|l| filter == 0 || l.index == filter) {
            for a in &l.addrs {
                msgs.push_back(addr_msg(l, a, RTM_NEWADDR, flags, h.seq, ctx.portid));
            }
        }
    }
    let answer = if filter != 0 { NLM_F_DUMP_FILTERED } else { 0 };
    Dump::new(h.seq, msgs, answer)
}

/// The 6.6 attribute set of `rtnl_fill_ifinfo` for V2's interfaces, in its order. Not sent: IFLA_IFALIAS (none
/// set), IFLA_LINKINFO (no link kinds), IFLA_AF_SPEC (per-family sysctls), parent device names.
pub fn link_msg(l: &Link, ty: u16, flags: u16, seq: u32, pid: u32, change: u32) -> Vec<u8> {
    let mut fixed = [0u8; IFINFOMSG];
    fixed[2..4].copy_from_slice(&l.arphrd().to_le_bytes());
    fixed[4..8].copy_from_slice(&l.index.to_le_bytes());
    fixed[8..12].copy_from_slice(&l.flags().to_le_bytes());
    fixed[12..16].copy_from_slice(&change.to_le_bytes());
    let mut m = Msg::new(ty, flags, seq, pid, &fixed);
    let tso = if l.loopback { LO_TSO_MAX_SIZE } else { GSO_LEGACY_MAX_SIZE };
    m.str(IFLA_IFNAME, l.name.as_bytes())
        .u32(IFLA_TXQLEN, ifview::TX_QUEUE_LEN)
        .u8(IFLA_OPERSTATE, l.operstate())
        .u8(IFLA_LINKMODE, 0)
        .u32(IFLA_MTU, l.mtu)
        .u32(IFLA_MIN_MTU, l.min_mtu())
        .u32(IFLA_MAX_MTU, l.max_mtu())
        .u32(IFLA_GROUP, 0)
        .u32(IFLA_PROMISCUITY, 0)
        .u32(IFLA_ALLMULTI, 0)
        .u32(IFLA_NUM_TX_QUEUES, 1)
        .u32(IFLA_GSO_MAX_SEGS, GSO_MAX_SEGS)
        .u32(IFLA_GSO_MAX_SIZE, GSO_LEGACY_MAX_SIZE)
        .u32(IFLA_GRO_MAX_SIZE, GSO_LEGACY_MAX_SIZE)
        .u32(IFLA_GSO_IPV4_MAX_SIZE, GSO_LEGACY_MAX_SIZE)
        .u32(IFLA_GRO_IPV4_MAX_SIZE, GSO_LEGACY_MAX_SIZE)
        .u32(IFLA_TSO_MAX_SIZE, tso)
        .u32(IFLA_TSO_MAX_SEGS, GSO_MAX_SEGS)
        .u32(IFLA_NUM_RX_QUEUES, 1)
        .u8(IFLA_CARRIER, l.carrier as u8)
        .str(IFLA_QDISC, b"noqueue")
        .u32(IFLA_CARRIER_CHANGES, l.carrier_up.wrapping_add(l.carrier_down))
        .u32(IFLA_CARRIER_UP_COUNT, l.carrier_up)
        .u32(IFLA_CARRIER_DOWN_COUNT, l.carrier_down)
        .u8(IFLA_PROTO_DOWN, 0)
        .attr(IFLA_MAP, &[0u8; 32])
        .attr(IFLA_ADDRESS, &l.mac)
        .attr(IFLA_BROADCAST, &l.broadcast_hw())
        .attr(IFLA_STATS64, &stats64(l))
        .attr(IFLA_STATS, &stats32(l))
        .nest_start(IFLA_XDP)
        .u8(IFLA_XDP_ATTACHED, 0)
        .nest_end();
    if l.mac != [0; 6] {
        m.attr(IFLA_PERM_ADDRESS, &l.mac);
    }
    m.done()
}

/// `struct rtnl_link_stats64` (25 counters): packets, bytes and drops as the card counts them.
fn stats64(l: &Link) -> [u8; 200] {
    let s = &l.stats;
    let mut b = [0u8; 200];
    for (i, v) in [s.rx_frames, s.tx_frames, s.rx_bytes, s.tx_bytes, 0, 0, s.rx_dropped, s.tx_dropped].iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// `struct rtnl_link_stats` (24 32-bit counters, cut as Linux's `copy_rtnl_link_stats` does).
fn stats32(l: &Link) -> [u8; 96] {
    let s = &l.stats;
    let mut b = [0u8; 96];
    for (i, v) in [s.rx_frames, s.tx_frames, s.rx_bytes, s.tx_bytes, 0, 0, s.rx_dropped, s.tx_dropped].iter().enumerate() {
        b[i * 4..i * 4 + 4].copy_from_slice(&(*v as u32).to_le_bytes());
    }
    b
}

/// `inet_fill_ifaddr`: ADDRESS before LOCAL (musl's getifaddrs reads them in this order), BROADCAST for a card,
/// the label, IFA_FLAGS and the lifetimes. The creation and update stamps are 0 (V2 does not keep them).
pub fn addr_msg(l: &Link, a: &Addr, ty: u16, flags: u16, seq: u32, pid: u32) -> Vec<u8> {
    let mut fixed = [0u8; IFADDRMSG];
    fixed[0] = AF_INET;
    fixed[1] = a.prefix;
    fixed[2] = a.flags() as u8;
    fixed[3] = a.scope;
    fixed[4..8].copy_from_slice(&l.index.to_le_bytes());
    let mut m = Msg::new(ty, flags, seq, pid, &fixed);
    m.attr(IFA_ADDRESS, &a.ip).attr(IFA_LOCAL, &a.ip);
    if let Some(b) = a.broadcast(l.loopback) {
        m.attr(IFA_BROADCAST, &b);
    }
    let mut ci = [0u8; 16];
    ci[0..4].copy_from_slice(&a.preferred_s.to_le_bytes());
    ci[4..8].copy_from_slice(&a.valid_s.to_le_bytes());
    m.str(IFA_LABEL, l.name.as_bytes()).u32(IFA_FLAGS, a.flags()).attr(IFA_CACHEINFO, &ci);
    m.done()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ReqCtx {
        ReqCtx { portid: 77, admin: false, strict: false }
    }

    fn req(ty: u16, flags: u16, body: &[u8]) -> Vec<u8> {
        let mut m = Msg::new(ty, flags, 5, 0, body);
        m.set_flags(flags);
        m.done()
    }

    #[test]
    fn requests_like_linux() {
        let c = ctx();
        assert!(matches!(rcv_msg(&req(0x7fff, NLM_F_REQUEST, &[0; 16]), &c), Err(NlErr { errno: EOPNOTSUPP, .. })));
        assert!(matches!(rcv_msg(&req(RTM_NEWLINK, NLM_F_REQUEST, &[0; 16]), &c), Err(NlErr { errno: EPERM, .. })));
        let root = ReqCtx { admin: true, ..ctx() };
        assert!(matches!(rcv_msg(&req(RTM_NEWLINK, NLM_F_REQUEST, &[0; 16]), &root), Err(NlErr { errno: EOPNOTSUPP, .. })));
        assert!(matches!(rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, &[]), &c), Ok(Action::Nothing)), "no payload: ignored");
        assert!(matches!(rcv_msg(&req(26, NLM_F_REQUEST | NLM_F_DUMP, &[0]), &c), Err(NlErr { errno: EOPNOTSUPP, .. })), "GETROUTE");
        let mut lo = [0u8; 16];
        lo[4] = 1;
        let Ok(Action::Reply(m)) = rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST, &lo), &c) else { panic!("lo by index") };
        let h = Hdr::parse(&m).unwrap();
        assert_eq!((h.ty, h.flags, h.seq, h.pid, h.len as usize), (RTM_NEWLINK, 0, 5, 77, m.len()));
        let a = attrs(&m[NLMSG_HDRLEN + IFINFOMSG..]);
        assert_eq!(a[0], (IFLA_IFNAME, &b"lo\0"[..]));
        lo[4] = 99;
        assert!(matches!(rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST, &lo), &c), Err(NlErr { errno: ENODEV, .. })));
        lo[4] = 0;
        assert!(matches!(rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST, &lo), &c), Err(NlErr { errno: EINVAL, msg: None })));
        assert!(matches!(rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST, &[0; 4]), &c), Err(NlErr { errno: EINVAL, msg: Some(_) })));
        let mut byname = Msg::new(RTM_GETLINK, NLM_F_REQUEST, 6, 0, &[0; 16]);
        byname.str(IFLA_IFNAME, b"lo");
        byname.set_flags(NLM_F_REQUEST);
        assert!(matches!(rcv_msg(&byname.done(), &c), Ok(Action::Reply(_))));
        let mut long = Msg::new(RTM_GETLINK, NLM_F_REQUEST, 6, 0, &[0; 16]);
        long.str(IFLA_IFNAME, b"0123456789abcdef");
        long.set_flags(NLM_F_REQUEST);
        assert!(matches!(rcv_msg(&long.done(), &c), Err(NlErr { errno: ERANGE, .. })));
    }

    #[test]
    fn dumps_and_strict_checks() {
        let c = ctx();
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, &[0]), &c) else { panic!() };
        assert!(!d.msgs.is_empty() && d.done_errno > 0);
        assert_eq!(Hdr::parse(&d.msgs[0]).unwrap().flags, NLM_F_MULTI);
        let s = ReqCtx { strict: true, ..ctx() };
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, &[0]), &s) else { panic!() };
        assert_eq!((d.msgs.len(), d.done_errno, d.done_msg), (0, -(EINVAL as i32), Some("Invalid header for link dump")));
        let mut ifa = [0u8; 8];
        ifa[0] = AF_INET;
        ifa[4] = 1;
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, &ifa), &s) else { panic!() };
        assert_eq!((d.msgs.len(), d.answer_flags), (1, NLM_F_DUMP_FILTERED));
        assert_eq!(Hdr::parse(&d.msgs[0]).unwrap().flags, NLM_F_MULTI | NLM_F_DUMP_FILTERED);
        ifa[4] = 77;
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, &ifa), &s) else { panic!() };
        assert_eq!(d.done_errno, -(ENODEV as i32));
        ifa[4] = 0;
        ifa[1] = 8;
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, &ifa), &s) else { panic!() };
        assert_eq!(d.done_errno, -(EINVAL as i32));
        let Ok(Action::Dump(d)) = rcv_msg(&req(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, &[AF_INET6]), &c) else { panic!() };
        assert!(d.msgs.is_empty(), "no IPv6 addresses");
    }

    #[test]
    fn loopback_messages() {
        let lo = ifview::links().remove(0);
        let m = addr_msg(&lo, &lo.addrs[0], RTM_NEWADDR, NLM_F_MULTI, 1, 2);
        assert_eq!(&m[NLMSG_HDRLEN..NLMSG_HDRLEN + 8], &[2, 8, 0x80, 254, 1, 0, 0, 0]);
        let a = attrs(&m[NLMSG_HDRLEN + 8..]);
        let types: Vec<u16> = a.iter().map(|x| x.0).collect();
        assert_eq!(types, [IFA_ADDRESS, IFA_LOCAL, IFA_LABEL, IFA_FLAGS, IFA_CACHEINFO], "no broadcast on lo");
        let l = link_msg(&lo, RTM_NEWLINK, 0, 0, 0, 0);
        let a = attrs(&l[NLMSG_HDRLEN + IFINFOMSG..]);
        assert!(a.iter().any(|x| x.0 == IFLA_STATS64 && x.1.len() == 200));
        assert!(a.iter().any(|x| x.0 == IFLA_STATS && x.1.len() == 96));
        assert!(!a.iter().any(|x| x.0 == IFLA_PERM_ADDRESS), "lo has no permanent address");
    }
}
