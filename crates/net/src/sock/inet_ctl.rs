//! Receive ancillary data of AF_INET and AF_INET6 datagram sockets: `ip_cmsg_recv_offset`
//! (linux-6.6/net/ipv4/ip_sockglue.c:56-74,180-200) and `ip6_datagram_recv_common_ctl` / `_specific_ctl`
//! (linux-6.6/net/ipv6/datagram.c:600-635, order as udpv6_recvmsg, net/ipv6/udp.c:419-428). BitsOS senders
//! use the default hop limit and traffic class, so a datagram carries 64 and 0.

use super::inet6::{encode_in6_as, SOL_IPV6};
use super::InetSock;
use crate::sock::abi::SOL_IP;
use crate::{Ipv4, SockAddr};
use alloc::vec::Vec;

const IP_TOS: u32 = 1;
const IP_TTL: u32 = 2;
const IP_PKTINFO: u32 = 8;
const IP_RECVTTL: u32 = 12;
const IP_RECVTOS: u32 = 13;
const IPV6_RECVPKTINFO: u32 = 49;
const IPV6_PKTINFO: u32 = 50;
const IPV6_RECVHOPLIMIT: u32 = 51;
const IPV6_HOPLIMIT: u32 = 52;
const IPV6_RECVTCLASS: u32 = 66;
const IPV6_TCLASS: u32 = 67;
/// `sysctl_ip_default_ttl` and IPv6's default hop limit.
const DEFAULT_HOPS: i32 = 64;

/// How a datagram reached this host.
#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    /// Its destination address (the kernel view: `LO6` is ::1).
    pub to: Ipv4,
    /// Sent by an AF_INET6 socket to ::1 (else an IPv4 datagram).
    pub v6: bool,
    /// The interface it came in on (1: lo).
    pub ifindex: u32,
    /// `ipi_spec_dst`: the local address a reply would come from (`to` unless it was a broadcast).
    pub spec: Ipv4,
}

/// One control message: level, type, data.
pub type Cmsg = (u32, u32, Vec<u8>);

impl InetSock {
    /// The control messages the program asked for with its IP_* / IPV6_RECV* options.
    pub(super) fn recv_ctl(&self, at: &Arrival) -> Vec<Cmsg> {
        let o = self.ip_opts.lock();
        let on = |l: u32, n: u32| o.get(&(l, n)).is_some_and(|v| *v != 0);
        let mut v = Vec::new();
        if self.is_v6() && on(SOL_IPV6, IPV6_RECVPKTINFO) {
            // struct in6_pktinfo: the destination (v4-mapped for an IPv4 datagram), then the interface.
            let mut b = encode_in6_as(SockAddr { ip: at.to, port: 0 }, at.v6)[8..24].to_vec();
            b.extend_from_slice(&at.ifindex.to_le_bytes());
            v.push((SOL_IPV6, IPV6_PKTINFO, b));
        }
        if at.v6 {
            if on(SOL_IPV6, IPV6_RECVHOPLIMIT) {
                v.push((SOL_IPV6, IPV6_HOPLIMIT, DEFAULT_HOPS.to_le_bytes().to_vec()));
            }
            if on(SOL_IPV6, IPV6_RECVTCLASS) {
                v.push((SOL_IPV6, IPV6_TCLASS, 0i32.to_le_bytes().to_vec()));
            }
        } else {
            if on(SOL_IP, IP_PKTINFO) {
                // struct in_pktinfo: ipi_ifindex, ipi_spec_dst, ipi_addr.
                let mut b = at.ifindex.to_le_bytes().to_vec();
                b.extend_from_slice(&at.spec.0);
                b.extend_from_slice(&at.to.0);
                v.push((SOL_IP, IP_PKTINFO, b));
            }
            if on(SOL_IP, IP_RECVTTL) {
                v.push((SOL_IP, IP_TTL, DEFAULT_HOPS.to_le_bytes().to_vec()));
            }
            if on(SOL_IP, IP_RECVTOS) {
                v.push((SOL_IP, IP_TOS, alloc::vec![0u8]));
            }
        }
        v
    }
}
