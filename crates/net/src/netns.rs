//! Network namespaces as the socket layer sees them (NW-11): what a namespace below the initial one has of its
//! own. The initial namespace is `None` everywhere (`Net`): its sockets keep the crate's tables, the kernel's
//! stack and every card, exactly as before namespaces existed.
//!
//! A new namespace starts as Linux's does (linux-6.6/drivers/net/loopback.c:208 `loopback_net_init`): one
//! interface, `lo` at index 1, administratively DOWN and without addresses. Setting it UP (SIOCSIFFLAGS or
//! RTM_NEWLINK with CAP_NET_ADMIN over the namespace's user namespace) gives it 127.0.0.1/8 (devinet.c
//! `inetdev_event` NETDEV_UP for a loopback device); the address stays when it goes down again, its local
//! route does not (`fib_disable_ip`), so 127/8 is reachable only while lo is UP. Everything else of a namespace
//! is its own too: the loopback's TCP listeners, the TCP and UDP ports, abstract AF_UNIX names, netlink port
//! ids and multicast members. No traffic of a namespace reaches the cards or another namespace's loopback.
//!
//! The object that owns a view is the process side's namespace (`proc::ns::misc::NetNs`); sockets keep their
//! namespace's view alive (Linux: a socket holds its `struct net`).

use crate::ifview::{self, Addr, IFA_F_SECONDARY, LOOPBACK_MTU, Link, TX_QUEUE_LEN};
use crate::loopback::Loopback;
use crate::sock::{inet, netlink, unix};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::{EEXIST, EINVAL, ENOBUFS};

/// A socket's or a caller's network namespace: `None` is the initial one.
pub type Net = Option<Arc<NetView>>;

/// Whether two namespaces are the same one.
pub fn same(a: &Net, b: &Net) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

static LIVE: AtomicU64 = AtomicU64::new(0);

/// Views of namespaces below the initial one that exist now (ktests check that none is left behind).
pub fn live() -> u64 {
    LIVE.load(Ordering::Relaxed)
}

/// `__dev_change_flags`'s changeable flags besides IFF_UP: DEBUG, NOTRAILERS, NOARP, PROMISC, ALLMULTI,
/// MULTICAST, PORTSEL, AUTOMEDIA, DYNAMIC.
const CHANGEABLE: u32 = 0x4 | 0x20 | 0x80 | 0x100 | 0x200 | 0x1000 | 0x2000 | 0x4000 | 0x8000;

/// The first port the ephemeral picker of a namespace hands out (IANA's range, as the kernel stack's).
const EPHEMERAL_FIRST: u16 = 49152;

/// lo of a namespace as far as V2 models its `net_device`, `in_device` and IPv6 side (NW-12).
#[derive(Clone)]
pub(crate) struct LoDev {
    /// Administratively up (IFF_UP).
    pub(crate) up: bool,
    /// The changeable flags besides IFF_UP (`CHANGEABLE`).
    pub(crate) flags: u32,
    pub(crate) mtu: u32,
    pub(crate) txqlen: u32,
    /// IPv4 is on for lo (`in_device`): off while its MTU is below 68 (`inetdev_valid_mtu`), which deletes its
    /// addresses.
    pub(crate) inet: bool,
    /// The local routing table exists (its first local route, i.e. the first address, made it): before that
    /// every IPv4 address passes a bind (`__inet_dev_addr_type`, linux-6.6/net/ipv4/fib_frontend.c:206) and
    /// /proc/net/route is empty (local and main share one table until custom rules exist).
    pub(crate) table: bool,
    /// ::1 is on lo: added at every UP (`init_loopback`), removed at DOWN and by an MTU below 1280
    /// (`addrconf_ifdown`); an MTU raised again does not bring it back before the next UP (CachyOS shows it).
    pub(crate) six: bool,
    /// The IPv4 addresses in `ifa_list` order: primaries first by scope, secondaries after them.
    pub(crate) addrs: Vec<Addr>,
    /// The local prefix routes of lo's primary addresses (net, prefix): `fib_add_ifaddr` makes one when the
    /// address comes while lo is UP or at lo's next UP; they stay while lo is down again (as CachyOS shows:
    /// `ip route show table local` keeps them). Each address's own /32 local route needs no entry.
    pub(crate) routes: Vec<([u8; 4], u8)>,
}

/// The smallest MTUs IPv4 and IPv6 run on (`inetdev_valid_mtu`, IPV6_MIN_MTU).
const IPV4_MIN_MTU: u32 = 68;
const IPV6_MIN_MTU: u32 = 1280;

impl LoDev {
    /// lo carries ::1 (`six`).
    pub(crate) fn has6(&self) -> bool {
        self.six
    }

    /// `ip` is a local IPv4 address of lo: inside one of its addresses' subnets (a loopback's prefix route is
    /// RTN_LOCAL, `fib_add_ifaddr`).
    pub(crate) fn covers(&self, ip: [u8; 4]) -> bool {
        self.addrs.iter().any(|a| a.ip == ip) || self.routes.iter().any(|(net, p)| Addr::plain(*net, *p, 0, 0, 0).covers(ip))
    }

    /// The source address for a destination on lo (the route's `prefsrc`): the address itself, else the
    /// primary whose subnet holds it, else 127.0.0.1.
    pub(crate) fn src_for(&self, ip: [u8; 4]) -> [u8; 4] {
        if self.addrs.iter().any(|a| a.ip == ip) {
            return ip;
        }
        self.addrs.iter().find(|a| !a.secondary() && a.covers(ip)).map_or([127, 0, 0, 1], |a| a.ip)
    }

    /// `fib_add_ifaddr`'s prefix route of a primary address (a loopback's is RTN_LOCAL): none for a secondary,
    /// with IFA_F_NOPREFIXROUTE, for the zero net or a /32.
    fn add_route(&mut self, a: &Addr) {
        const IFA_F_NOPREFIXROUTE: u32 = 0x200;
        let m = u32::from_be_bytes(a.mask());
        let net = (u32::from_be_bytes(a.ip) & m).to_be_bytes();
        if a.secondary() || a.extra & IFA_F_NOPREFIXROUTE != 0 || net == [0; 4] || (a.prefix == 32 && net == a.ip) || self.routes.contains(&(net, a.prefix)) {
            return;
        }
        self.routes.push((net, a.prefix));
    }

    /// lo came UP: every address gets its prefix route (`fib_netdev_event` NETDEV_UP).
    pub(crate) fn route_all(&mut self) {
        for a in self.addrs.clone() {
            self.add_route(&a);
        }
    }

    /// `__inet_insert_ifa` (linux-6.6/net/ipv4/devinet.c:476) after `find_matching_ifa`: an address of the
    /// subnet of an existing primary becomes its secondary (EINVAL for a different scope), the same local
    /// address in the same subnet is EEXIST, 0.0.0.0 is ignored. A primary goes after the last primary whose
    /// scope is not below its own, a secondary to the end.
    pub(crate) fn insert(&mut self, mut a: Addr) -> Result<(), NlFail> {
        if !self.inet {
            return Err(NlFail(ENOBUFS, None));
        }
        if a.ip == [0; 4] {
            return Ok(());
        }
        a.extra &= !(IFA_F_SECONDARY | IPV6ONLY_FLAGS);
        let mut last_primary = 0;
        for (i, b) in self.addrs.iter().enumerate() {
            if !b.secondary() && a.scope <= b.scope {
                last_primary = i + 1;
            }
            if b.prefix == a.prefix && a.covers(b.ip) {
                if b.ip == a.ip {
                    return Err(NlFail(EEXIST, Some("ipv4: Address already assigned")));
                }
                if b.scope != a.scope {
                    return Err(NlFail(EINVAL, Some("ipv4: Invalid scope value")));
                }
                a.extra |= IFA_F_SECONDARY;
            }
        }
        let at = if a.secondary() { self.addrs.len() } else { last_primary };
        if self.up {
            self.add_route(&a);
        }
        self.addrs.insert(at, a);
        self.table = true;
        Ok(())
    }

    /// `__inet_del_ifa` (linux-6.6/net/ipv4/devinet.c) with `promote_secondaries` on, as every systemd
    /// system sets it (CachyOS's new namespaces inherit it): deleting a primary promotes the first secondary of
    /// its subnet, which moves behind the last primary of a scope not narrower than the deleted one's.
    pub(crate) fn delete(&mut self, i: usize) -> Addr {
        let a = self.addrs[i].clone();
        let mut last_prim = i;
        let mut promote = None;
        let mut skipped = false;
        if !a.secondary() {
            for (j, b) in self.addrs.iter().enumerate().skip(i + 1) {
                if !b.secondary() && a.scope <= b.scope {
                    last_prim = j + 1;
                }
                if b.secondary() && b.prefix == a.prefix && a.covers(b.ip) {
                    promote = Some(j);
                    break;
                }
                skipped = true;
            }
        }
        self.addrs.remove(i);
        if let Some(j) = promote {
            // Indices after `i` moved down by one with the removal.
            let mut p = self.addrs.remove(j - 1);
            p.extra &= !IFA_F_SECONDARY;
            let at = if skipped { if last_prim == i { i } else { last_prim - 1 } } else { j - 1 };
            self.addrs.insert(at.min(self.addrs.len()), p);
        }
        // `fib_del_ifaddr`: a prefix route goes with the last primary of its subnet.
        let addrs = &self.addrs;
        self.routes.retain(|(net, p)| addrs.iter().any(|b| !b.secondary() && b.prefix == *p && b.covers(*net)));
        a
    }

    /// `dev_set_mtu` on lo (no bounds: lo's min_mtu and max_mtu are 0): below 68 IPv4 goes with all addresses
    /// (`inetdev_destroy`), at 68 and more it comes back without any (`inetdev_init`).
    pub(crate) fn set_mtu(&mut self, mtu: u32) {
        self.mtu = mtu;
        if mtu < IPV6_MIN_MTU {
            self.six = false;
        }
        if mtu < IPV4_MIN_MTU {
            self.inet = false;
            self.addrs.clear();
            self.routes.clear();
        } else {
            self.inet = true;
        }
    }
}

/// `IPV6ONLY_FLAGS` (devinet.c): NODAD, OPTIMISTIC, DADFAILED, HOMEADDRESS, TENTATIVE, MANAGETEMPADDR,
/// STABLE_PRIVACY.
const IPV6ONLY_FLAGS: u32 = 0x02 | 0x04 | 0x08 | 0x10 | 0x40 | 0x100 | 0x800;

/// An error with the extended-ack text Linux gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NlFail(pub vfs::Errno, pub Option<&'static str>);

pub struct NetView {
    inum: u32,
    /// lo's device state (copied out by readers; changed only under `change`).
    dev: SpinLock<LoDev>,
    /// Serialises lo changes with their netlink notifications (no change is announced twice or out of order).
    change: ksync::Mutex<()>,
    /// TCP over 127/8 and ::1 (its own listeners and connections).
    pub(crate) lo: Loopback,
    pub(crate) inet: inet::Tables,
    pub(crate) unix: unix::Names,
    pub(crate) netlink: netlink::Tables,
    next_port: SpinLock<u16>,
}

impl NetView {
    /// The view of a new namespace (`inum`: its /proc/<pid>/ns/net inode number, for the log).
    pub fn new(inum: u32) -> Arc<NetView> {
        LIVE.fetch_add(1, Ordering::Relaxed);
        klog::event!(Info, "net", "netns.create", inum = inum);
        let dev = LoDev { up: false, flags: 0, mtu: LOOPBACK_MTU, txqlen: TX_QUEUE_LEN, inet: true, table: false, six: false, addrs: Vec::new(), routes: Vec::new() };
        Arc::new(NetView {
            inum,
            dev: SpinLock::named("net.netns.lo", dev),
            change: ksync::Mutex::named("net.netns.change", ()),
            lo: Loopback::new(),
            inet: inet::Tables::new(),
            unix: unix::Names::new(),
            netlink: netlink::Tables::new(),
            next_port: SpinLock::named("net.netns.ports", EPHEMERAL_FIRST),
        })
    }

    pub fn inum(&self) -> u32 {
        self.inum
    }

    /// lo is UP: its addresses (127/8 and ::1 after the UP) can be reached. Host tests only today (C1).
    pub fn lo_up(&self) -> bool {
        self.dev.lock().up
    }

    /// A copy of lo's state (for `edit` and the views; the packet paths use `with_dev`).
    pub(crate) fn lo_dev(&self) -> LoDev {
        self.dev.lock().clone()
    }

    /// `f` on lo's state under its lock, without a copy: bind, connect and sendto ask this on every call, and a
    /// copy would allocate under the spinlock as many entries as a program gave lo addresses.
    pub(crate) fn with_dev<T>(&self, f: impl FnOnce(&LoDev) -> T) -> T {
        f(&self.dev.lock())
    }

    /// lo as an interface (`ifview::links_in`).
    pub(crate) fn lo_link(&self) -> Link {
        let d = self.lo_dev();
        let six = if d.has6() { alloc::vec![ifview::lo6()] } else { Vec::new() };
        ifview::lo_link(if d.up { ifview::IFF_UP } else { 0 } | d.flags, d.addrs, six, d.mtu, d.txqlen)
    }

    /// One change of lo (`f` on a copy of its state, kept when it answers Ok): serialised by `change`; the
    /// namespace's netlink members hear what differs (RTM_NEWLINK, RTM_NEWADDR/RTM_DELADDR for IPv4 and ::1).
    pub(crate) fn edit<T>(self: &Arc<Self>, what: &'static str, f: impl FnOnce(&mut LoDev) -> Result<T, NlFail>) -> Result<T, NlFail> {
        self.apply(what, false, f)
    }

    /// As `edit`, but what `f` changed is kept (and announced) when it fails too: `devinet_ioctl` deletes the
    /// old address before it inserts the changed one, and a failed insert does not bring the old one back.
    pub(crate) fn edit_partial<T>(self: &Arc<Self>, what: &'static str, f: impl FnOnce(&mut LoDev) -> Result<T, NlFail>) -> Result<T, NlFail> {
        self.apply(what, true, f)
    }

    fn apply<T>(self: &Arc<Self>, what: &'static str, keep_failed: bool, f: impl FnOnce(&mut LoDev) -> Result<T, NlFail>) -> Result<T, NlFail> {
        let _g = self.change.lock();
        let net = Some(self.clone());
        let before = ifview::links_in(&net);
        let mut d = self.lo_dev();
        let r = f(&mut d);
        if r.is_err() && !keep_failed {
            return r;
        }
        let (up, flags, mtu, n) = (d.up, d.flags, d.mtu, d.addrs.len());
        *self.dev.lock() = d;
        let after = ifview::links_in(&net);
        if !netlink::same_view(&before, &after) {
            klog::event!(Info, "net", "netns.lo", inum = self.inum, what = what, up = up, flags = klog::Hex(flags as u64), mtu = mtu, addrs = n);
        }
        netlink::announce(&self.netlink, &before, &after);
        if let Err(e) = &r {
            klog::event!(Warn, "net", "netns.lo_partial", inum = self.inum, what = what, errno = e.0);
        }
        r
    }

    /// `__dev_change_flags` on lo (linux-6.6/net/core/dev.c): `flags` is what its flags are to become. IFF_UP
    /// brings it up or down; every UP gives it 127.0.0.1/8 unless that is there (`inetdev_event` NETDEV_UP, its
    /// EEXIST ignored), and ::1 (`init_loopback`); DOWN keeps the IPv4 addresses and drops ::1. The other
    /// changeable flags (IFF_DEBUG, IFF_NOARP, IFF_PROMISC, IFF_ALLMULTI, ...) are kept and shown (they change
    /// nothing on a loopback); the rest (LOOPBACK, RUNNING) is ignored.
    pub(crate) fn change_lo_flags(self: &Arc<Self>, flags: u32) -> Result<(), vfs::Errno> {
        let (up, extra) = (flags & ifview::IFF_UP != 0, flags & CHANGEABLE);
        self.edit("flags", |d| {
            let was = d.up;
            d.flags = extra;
            d.up = up;
            if up && !was {
                if d.inet {
                    // An existing 127.0.0.1/8 (EEXIST) or one in another scope (EINVAL) stays as it is.
                    let _ = d.insert(ifview::lo_addr());
                    d.route_all();
                }
                d.six = true;
            }
            if !up {
                d.six = false;
            }
            Ok(())
        })
        .map_err(|e| e.0)
    }

    /// A free ephemeral port (49152-65535) that `taken` does not name; one pass at most (B4), `None`: all taken.
    pub(crate) fn ephemeral(&self, taken: impl Fn(u16) -> bool) -> Option<u16> {
        let mut p = self.next_port.lock();
        for _ in EPHEMERAL_FIRST..=u16::MAX {
            let cur = *p;
            *p = if cur == u16::MAX { EPHEMERAL_FIRST } else { cur + 1 };
            if !taken(cur) {
                return Some(cur);
            }
        }
        None
    }
}

impl Drop for NetView {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        klog::event!(Info, "net", "netns.destroy", inum = self.inum);
    }
}

impl core::fmt::Debug for NetView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "NetView({})", self.inum)
    }
}
