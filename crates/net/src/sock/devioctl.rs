//! The interface ioctls every socket answers (linux-6.6/net/socket.c `sock_ioctl`/`sock_do_ioctl`,
//! net/core/dev_ioctl.c `dev_ioctl`/`dev_ifconf`, net/ipv4/af_inet.c `inet_ioctl`, net/ipv4/devinet.c
//! `devinet_ioctl`/`inet_gifconf`, net/ipv4/arp.c `arp_ioctl`, net/ethtool/ioctl.c): SIOCGIFNAME/INDEX/FLAGS/
//! MTU/METRIC/TXQLEN/HWADDR/MAP/CONF on any socket, the address family (SIOCGIFADDR, NETMASK, BRDADDR, DSTADDR)
//! on AF_INET sockets, ETHTOOL_GLINK. What changes an interface needs CAP_NET_ADMIN (EPERM for programs); for
//! root it is EOPNOTSUPP (the kernel's stack owns the configuration). Every command copies the whole 40-byte
//! `struct ifreq` in first (EFAULT) and the whole struct back when it answers, as Linux does.

use super::abi::{SIOCATMARK, SIOCOUTQNSD};
use super::{Kind, SockFile};
use crate::ifview::{self, Link};
use uapi::errno::*;
use vfs::sys::UserMem;

pub const SIOCADDRT: u64 = 0x890B;
pub const SIOCDELRT: u64 = 0x890C;
pub const SIOCRTMSG: u64 = 0x890D;
pub const SIOCGIFNAME: u64 = 0x8910;
pub const SIOCSIFLINK: u64 = 0x8911;
pub const SIOCGIFCONF: u64 = 0x8912;
pub const SIOCGIFFLAGS: u64 = 0x8913;
pub const SIOCSIFFLAGS: u64 = 0x8914;
pub const SIOCGIFADDR: u64 = 0x8915;
pub const SIOCSIFADDR: u64 = 0x8916;
pub const SIOCGIFDSTADDR: u64 = 0x8917;
pub const SIOCSIFDSTADDR: u64 = 0x8918;
pub const SIOCDIFADDR: u64 = 0x8936;
pub const SIOCGIFBRDADDR: u64 = 0x8919;
pub const SIOCSIFBRDADDR: u64 = 0x891a;
pub const SIOCGIFNETMASK: u64 = 0x891b;
pub const SIOCSIFNETMASK: u64 = 0x891c;
pub const SIOCGIFMETRIC: u64 = 0x891d;
pub const SIOCSIFMETRIC: u64 = 0x891e;
pub const SIOCGIFMEM: u64 = 0x891f;
pub const SIOCSIFMEM: u64 = 0x8920;
pub const SIOCGIFMTU: u64 = 0x8921;
pub const SIOCSIFMTU: u64 = 0x8922;
pub const SIOCSIFNAME: u64 = 0x8923;
pub const SIOCSIFHWADDR: u64 = 0x8924;
pub const SIOCGIFHWADDR: u64 = 0x8927;
pub const SIOCGIFSLAVE: u64 = 0x8929;
pub const SIOCSIFSLAVE: u64 = 0x8930;
pub const SIOCADDMULTI: u64 = 0x8931;
pub const SIOCDELMULTI: u64 = 0x8932;
pub const SIOCGIFINDEX: u64 = 0x8933;
pub const SIOCSIFPFLAGS: u64 = 0x8934;
pub const SIOCGIFPFLAGS: u64 = 0x8935;
pub const SIOCSIFHWBROADCAST: u64 = 0x8937;
pub const SIOCGIFTXQLEN: u64 = 0x8942;
pub const SIOCSIFTXQLEN: u64 = 0x8943;
pub const SIOCETHTOOL: u64 = 0x8946;
pub const SIOCGMIIPHY: u64 = 0x8947;
pub const SIOCGMIIREG: u64 = 0x8948;
pub const SIOCSMIIREG: u64 = 0x8949;
pub const SIOCWANDEV: u64 = 0x894A;
pub const SIOCGSKNS: u64 = 0x894C;
pub const SIOCDARP: u64 = 0x8953;
pub const SIOCGARP: u64 = 0x8954;
pub const SIOCSARP: u64 = 0x8955;
pub const SIOCGIFMAP: u64 = 0x8970;
pub const SIOCSIFMAP: u64 = 0x8971;
pub const SIOCBONDENSLAVE: u64 = 0x8990;
pub const SIOCBONDRELEASE: u64 = 0x8991;
pub const SIOCBONDSETHWADDR: u64 = 0x8992;
pub const SIOCBONDSLAVEINFOQUERY: u64 = 0x8993;
pub const SIOCBONDINFOQUERY: u64 = 0x8994;
pub const SIOCBONDCHANGEACTIVE: u64 = 0x8995;
pub const SIOCBRADDIF: u64 = 0x89a2;
pub const SIOCBRDELIF: u64 = 0x89a3;
pub const SIOCSHWTSTAMP: u64 = 0x89b0;
pub const SIOCGHWTSTAMP: u64 = 0x89b1;
pub const SIOCUNIXFILE: u64 = 0x89e0;
pub const SIOCDEVPRIVATE: u64 = 0x89f0;
pub const ETHTOOL_GLINK: u32 = 0x0a;

/// `struct ifreq`, `struct ifconf`, `struct rtentry`, `struct arpreq`.
const IFREQ: usize = 40;
const IFCONF: usize = 16;
const RTENTRY: usize = 120;
const ARPREQ: usize = 68;
const AF_INET: u16 = 2;

/// Who asks (root has CAP_NET_ADMIN).
pub struct Asker<'a> {
    pub mem: &'a dyn UserMem,
    pub uid: u32,
}

fn get(a: &Asker<'_>, at: u64, n: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    let mut b = alloc::vec![0u8; n];
    if a.mem.read(at, &mut b) { Ok(b) } else { Err(EFAULT) }
}

fn put(a: &Asker<'_>, at: u64, b: &[u8]) -> Result<(), i64> {
    if a.mem.write(at, b) { Ok(()) } else { Err(EFAULT) }
}

/// The interface ioctls of a socket: Some(result) when `cmd` is one of them, None for the socket's own ioctl
/// (`File::ioctl`: SIOCINQ, SIOCATMARK, SIOCGSTAMP, FIOGETOWN, ...) and everything else.
pub fn ioctl(a: &Asker<'_>, k: &SockFile, cmd: u64, arg: u64) -> Option<i64> {
    let r = match cmd {
        SIOCGIFCONF => ifconf(a, arg),
        // The private range goes to the device before the protocol (`sock_ioctl`).
        c if (SIOCDEVPRIVATE..=SIOCDEVPRIVATE + 15).contains(&c) => dev_ioctl(a, cmd, arg),
        // `sock_ioctl` itself: owner and stamps (the socket's own `File::ioctl`).
        0x8901..=0x8904 | 0x8906 | 0x8907 => return None,
        SIOCGSKNS => Err(if a.uid == 0 { EOPNOTSUPP } else { EPERM }),
        // `sock_ioctl` hands SIOCIWFIRST..SIOCIWLAST to wext on every socket (net/socket.c:1271-1273).
        0x8B00..=0x8BFF => wext(a, cmd, arg),
        _ => match &k.kind {
            // `inet6_ioctl`: routes and addresses change with CAP_NET_ADMIN only (no IPv6 configuration here);
            // the IPv4 address ioctls are not AF_INET6's and go to the device path.
            Kind::Inet(i) if i.is_v6() => match cmd {
                SIOCADDRT | SIOCDELRT | SIOCSIFADDR | SIOCDIFADDR | SIOCSIFDSTADDR => need_admin(a),
                SIOCOUTQNSD | SIOCATMARK if i.linux_type() == super::abi::SOCK_STREAM => return None,
                _ if is_socket_ioctl(cmd) => dev_ioctl(a, cmd, arg),
                _ => return None,
            },
            Kind::Inet(i) => match cmd {
                SIOCADDRT | SIOCDELRT => route(a, arg),
                SIOCRTMSG => Err(EINVAL),
                SIOCDARP | SIOCGARP | SIOCSARP => arp(a, cmd, arg),
                SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK | SIOCGIFDSTADDR | SIOCGIFPFLAGS | SIOCSIFADDR | SIOCSIFBRDADDR
                | SIOCSIFNETMASK | SIOCSIFDSTADDR | SIOCSIFPFLAGS | SIOCSIFFLAGS => devinet(a, cmd, arg),
                // `tcp_ioctl`'s own (SIOCATMARK, SIOCOUTQNSD) stay with the socket; UDP has neither (the
                // device path: the ifreq is read, then ENOTTY).
                SIOCOUTQNSD | SIOCATMARK if i.linux_type() == super::abi::SOCK_STREAM => return None,
                _ if is_socket_ioctl(cmd) => dev_ioctl(a, cmd, arg),
                _ => return None,
            },
            Kind::Unix(_) if cmd == SIOCUNIXFILE => Err(if a.uid == 0 { EOPNOTSUPP } else { EPERM }),
            // `unix_ioctl` answers SIOCATMARK itself.
            Kind::Unix(_) if cmd == SIOCATMARK => return None,
            _ if is_socket_ioctl(cmd) => dev_ioctl(a, cmd, arg),
            _ => return None,
        },
    };
    Some(r.unwrap_or_else(|e| -e))
}


/// `is_socket_ioctl_cmd`: type 0x89.
fn is_socket_ioctl(cmd: u64) -> bool {
    (cmd >> 8) & 0xff == 0x89
}

/// The name in an ifreq after `ifr_name[IFNAMSIZ-1] = 0` and the alias cut at ':' (`dev_ioctl`): the name to
/// look up and the position of the colon.
fn name_of(r: &mut [u8]) -> (alloc::vec::Vec<u8>, Option<usize>) {
    r[15] = 0;
    let n = r[..16].iter().position(|b| *b == 0).unwrap_or(15);
    let colon = r[..n].iter().position(|b| *b == b':');
    (r[..colon.unwrap_or(n)].to_vec(), colon)
}

fn by_name(n: &[u8]) -> Result<Link, i64> {
    ifview::by_name(n).ok_or(ENODEV)
}

/// `wext_handle_ioctl` (linux-6.6/net/wireless/wext-core.c:1063-1080, :990-1017, :942-984): the 32-byte
/// iwreq (EFAULT), then the permission check before the device (SET commands, SIOCGIWENCODE and
/// SIOCGIWENCODEEXT need CAP_NET_ADMIN), then ENODEV for an unknown name, then EOPNOTSUPP: no V2 interface has
/// wireless handlers (cfg80211's wext compatibility for the WLAN interface is not built).
fn wext(a: &Asker<'_>, cmd: u64, arg: u64) -> Result<i64, i64> {
    const SIOCGIWENCODE: u64 = 0x8B2B;
    const SIOCGIWENCODEEXT: u64 = 0x8B35;
    let mut r = get(a, arg, 32)?;
    let set = cmd & 1 == 0;
    if (set || cmd == SIOCGIWENCODE || cmd == SIOCGIWENCODEEXT) && a.uid != 0 {
        return Err(EPERM);
    }
    // `__dev_get_by_name` on the whole name: wext cuts no ':' alias, unlike `dev_ioctl`.
    r[15] = 0;
    let n = r[..16].iter().position(|b| *b == 0).unwrap_or(15);
    by_name(&r[..n])?;
    Err(EOPNOTSUPP)
}

fn need_admin(a: &Asker<'_>) -> Result<i64, i64> {
    if a.uid == 0 { Err(EOPNOTSUPP) } else { Err(EPERM) }
}

/// `dev_ioctl` (after `get_user_ifreq`).
fn dev_ioctl(a: &Asker<'_>, cmd: u64, arg: u64) -> Result<i64, i64> {
    let mut r = get(a, arg, IFREQ)?;
    if cmd == SIOCGIFNAME {
        r[15] = 0;
        let index = i32::from_le_bytes([r[16], r[17], r[18], r[19]]);
        let l = if index > 0 { ifview::by_index(index as u32) } else { None }.ok_or(ENODEV)?;
        let n = &l.name.as_bytes()[..l.name.len().min(ifview::IFNAME_MAX)];
        r[..n.len()].copy_from_slice(n);
        r[n.len()] = 0;
        put(a, arg, &r)?;
        return Ok(0);
    }
    let (name, colon) = name_of(&mut r);
    let put_int = |r: &mut [u8], v: u32| r[16..20].copy_from_slice(&v.to_le_bytes());
    match cmd {
        SIOCGIFHWADDR => {
            let l = by_name(&name)?;
            r[16..18].copy_from_slice(&l.arphrd().to_le_bytes());
            r[18..24].copy_from_slice(&l.mac);
        }
        SIOCGIFFLAGS | SIOCGIFMETRIC | SIOCGIFMTU | SIOCGIFSLAVE | SIOCGIFMAP | SIOCGIFINDEX | SIOCGIFTXQLEN => {
            let l = by_name(&name)?;
            match cmd {
                // `ifr_flags` is a short: IFF_LOWER_UP (bit 16) does not fit.
                SIOCGIFFLAGS => r[16..18].copy_from_slice(&(l.flags() as u16).to_le_bytes()),
                SIOCGIFMETRIC => put_int(&mut r, 0),
                SIOCGIFMTU => put_int(&mut r, l.mtu),
                SIOCGIFSLAVE => return Err(EINVAL),
                // `struct ifmap` fields (mem_start, mem_end, base_addr, irq, dma, port): none for these cards.
                SIOCGIFMAP => r[16..37].fill(0),
                SIOCGIFINDEX => put_int(&mut r, l.index),
                _ => put_int(&mut r, ifview::TX_QUEUE_LEN),
            }
        }
        SIOCETHTOOL => return ethtool(a, &name, &r),
        SIOCGMIIPHY | SIOCGMIIREG | SIOCSIFNAME | SIOCSIFMAP | SIOCSIFTXQLEN | SIOCSIFFLAGS | SIOCSIFMETRIC | SIOCSIFMTU
        | SIOCSIFHWADDR | SIOCSIFSLAVE | SIOCADDMULTI | SIOCDELMULTI | SIOCSIFHWBROADCAST | SIOCSMIIREG | SIOCBONDENSLAVE
        | SIOCBONDRELEASE | SIOCBONDSETHWADDR | SIOCBONDCHANGEACTIVE | SIOCBRADDIF | SIOCBRDELIF | SIOCSHWTSTAMP => {
            return need_admin(a);
        }
        // Bonding queries, WAN devices, hardware stamps and the private range: no V2 device has them.
        SIOCBONDSLAVEINFOQUERY | SIOCBONDINFOQUERY | SIOCWANDEV | SIOCGHWTSTAMP => {
            by_name(&name)?;
            return Err(EOPNOTSUPP);
        }
        c if (SIOCDEVPRIVATE..=SIOCDEVPRIVATE + 15).contains(&c) => {
            by_name(&name)?;
            return Err(EOPNOTSUPP);
        }
        _ => return Err(ENOTTY),
    }
    if let Some(c) = colon {
        r[c] = b':';
    }
    put(a, arg, &r)?;
    Ok(0)
}

/// `dev_ethtool`: the command word first (EFAULT), then the device (ENODEV); V2 answers ETHTOOL_GLINK.
fn ethtool(a: &Asker<'_>, name: &[u8], r: &[u8]) -> Result<i64, i64> {
    let data = u64::from_le_bytes(r[16..24].try_into().unwrap_or([0; 8]));
    let cmd = u32::from_le_bytes(get(a, data, 4)?.try_into().unwrap_or([0; 4]));
    let l = by_name(name)?;
    if cmd != ETHTOOL_GLINK {
        return Err(EOPNOTSUPP);
    }
    let mut v = [0u8; 8];
    v[0..4].copy_from_slice(&cmd.to_le_bytes());
    v[4..8].copy_from_slice(&(l.carrier as u32).to_le_bytes());
    put(a, data, &v)?;
    Ok(0)
}

/// `devinet_ioctl` (AF_INET sockets).
fn devinet(a: &Asker<'_>, cmd: u64, arg: u64) -> Result<i64, i64> {
    let mut r = get(a, arg, IFREQ)?;
    let (name, colon) = name_of(&mut r);
    let orig_family = u16::from_le_bytes([r[16], r[17]]);
    let orig_addr = [r[20], r[21], r[22], r[23]];
    match cmd {
        SIOCGIFADDR | SIOCGIFBRDADDR | SIOCGIFDSTADDR | SIOCGIFNETMASK => {
            r[16..32].fill(0);
            r[16..18].copy_from_slice(&AF_INET.to_le_bytes());
        }
        SIOCSIFFLAGS => return need_admin(a),
        SIOCSIFADDR | SIOCSIFBRDADDR | SIOCSIFDSTADDR | SIOCSIFNETMASK => {
            if a.uid != 0 {
                return Err(EPERM);
            }
            if orig_family != AF_INET {
                return Err(EINVAL);
            }
            return Err(EOPNOTSUPP);
        }
        _ => return Err(EINVAL),
    }
    let l = by_name(&name)?;
    if let Some(c) = colon {
        r[c] = b':';
    }
    // The label is the name with its alias part (`ifa_label`); V2's addresses carry the interface's name.
    let full_len = r[..16].iter().position(|b| *b == 0).unwrap_or(15);
    let label_ok = &r[..full_len] == l.name.as_bytes();
    let ifa = if label_ok {
        let by_addr = (orig_family == AF_INET).then(|| l.addrs.iter().find(|x| x.ip == orig_addr)).flatten();
        by_addr.or(l.addrs.first())
    } else {
        None
    };
    let ifa = ifa.ok_or(EADDRNOTAVAIL)?;
    let v = match cmd {
        SIOCGIFADDR | SIOCGIFDSTADDR => ifa.ip,
        SIOCGIFBRDADDR => ifa.broadcast(l.loopback).unwrap_or([0; 4]),
        _ => ifa.mask(),
    };
    r[20..24].copy_from_slice(&v);
    put(a, arg, &r)?;
    Ok(0)
}

/// `dev_ifconf` + `inet_gifconf`: one entry per IPv4 address (label, AF_INET address), in interface order; a
/// NULL buffer counts.
fn ifconf(a: &Asker<'_>, arg: u64) -> Result<i64, i64> {
    let c = get(a, arg, IFCONF)?;
    let len = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
    let buf = u64::from_le_bytes(c[8..16].try_into().unwrap_or([0; 8]));
    let mut total: i32 = 0;
    for l in ifview::links() {
        for x in &l.addrs {
            if buf == 0 {
                total += IFREQ as i32;
                continue;
            }
            if len.wrapping_sub(total) < IFREQ as i32 {
                break;
            }
            let mut e = [0u8; IFREQ];
            let n = l.name.len().min(ifview::IFNAME_MAX);
            e[..n].copy_from_slice(&l.name.as_bytes()[..n]);
            e[16..18].copy_from_slice(&AF_INET.to_le_bytes());
            e[20..24].copy_from_slice(&x.ip);
            // A buffer address at the top of the address space wraps (as Linux's `pos + total`) and fails the
            // copy: EFAULT, never an overflow.
            put(a, buf.wrapping_add(total as u64), &e)?;
            total += IFREQ as i32;
        }
    }
    put(a, arg, &total.to_le_bytes())?;
    Ok(0)
}

/// SIOCADDRT/SIOCDELRT (`ip_rt_ioctl`): the rtentry is read, then CAP_NET_ADMIN.
fn route(a: &Asker<'_>, arg: u64) -> Result<i64, i64> {
    get(a, arg, RTENTRY)?;
    need_admin(a)
}

/// `arp_ioctl`: setting and deleting need CAP_NET_ADMIN; SIOCGARP looks the device up. V2's ARP table lives in
/// the stack (smoltcp keeps it private): no entry is shown (ENXIO, Linux's answer for an unknown address).
fn arp(a: &Asker<'_>, cmd: u64, arg: u64) -> Result<i64, i64> {
    if cmd != SIOCGARP && a.uid != 0 {
        return Err(EPERM);
    }
    let r = get(a, arg, ARPREQ)?;
    if u16::from_le_bytes([r[0], r[1]]) != AF_INET {
        return Err(EPFNOSUPPORT);
    }
    let flags = i32::from_le_bytes([r[48], r[49], r[50], r[51]]);
    const ATF_PUBL: i32 = 0x08;
    const ATF_NETMASK: i32 = 0x20;
    const ATF_DONTPUB: i32 = 0x40;
    if flags & ATF_PUBL == 0 && flags & (ATF_NETMASK | ATF_DONTPUB) != 0 {
        return Err(EINVAL);
    }
    let dev = &r[52..68];
    let n = dev.iter().position(|b| *b == 0).unwrap_or(16);
    if n > 0 {
        by_name(&dev[..n])?;
    } else if cmd == SIOCGARP {
        return Err(ENODEV);
    }
    if cmd != SIOCGARP {
        return Err(EOPNOTSUPP);
    }
    Err(ENXIO)
}

use uapi::errno::EPFNOSUPPORT;
