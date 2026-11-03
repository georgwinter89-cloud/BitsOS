//! The network interfaces as programs see them (NW-6): Linux's `net_device` view of the kernel's stack, for the
//! rtnetlink dumps (`sock::rtnl`) and the SIOCGIF* ioctls (`sock::devioctl`).
//!
//! Index 1 is `lo` (127.0.0.1/8, as on every Linux); the stack's cards follow in registration order from index 2
//! (`igc0` = 2 on the Mini-PC), under the names the drivers gave them (the names `/proc/net/dhcp` and the log
//! use). V2 has IPv4 only, one address per card at most (its DHCP lease or static address; NW-12: a network
//! namespace's lo has the addresses its programs gave it, and ::1 while it is UP), and every card is
//! administratively up (the kernel's stack drives all of them): flags as Linux's `dev_get_flags` shows a card
//! that is up, RUNNING and LOWER_UP with carrier.

use crate::netns::Net;
use crate::stack::Stack;
use alloc::string::String;
use alloc::vec::Vec;
use devmodel::net::NetStats;

/// `include/uapi/linux/if.h` flags (`dev_get_flags`).
pub const IFF_UP: u32 = 0x1;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_MULTICAST: u32 = 0x1000;
pub const IFF_LOWER_UP: u32 = 0x10000;
/// `if_arp.h` device types.
pub const ARPHRD_ETHER: u16 = 1;
pub const ARPHRD_LOOPBACK: u16 = 772;
/// RFC 2863 operational states (`IF_OPER_*`).
pub const IF_OPER_UNKNOWN: u8 = 0;
pub const IF_OPER_DOWN: u8 = 2;
pub const IF_OPER_UP: u8 = 6;
/// `rt_scope_t`.
pub const RT_SCOPE_UNIVERSE: u8 = 0;
pub const RT_SCOPE_HOST: u8 = 254;
/// `IFA_F_PERMANENT`: an address without a lifetime.
pub const IFA_F_PERMANENT: u32 = 0x80;
/// The lifetime that never ends (`INFINITY_LIFE_TIME`).
pub const INFINITY: u32 = 0xFFFF_FFFF;
/// The loopback's MTU (`LOOPBACK_MTU`, drivers/net/loopback.c).
pub const LOOPBACK_MTU: u32 = 65536;
/// `DEFAULT_TX_QUEUE_LEN`: what `ip link` shows as qlen for every device (V2 has no queueing discipline).
pub const TX_QUEUE_LEN: u32 = 1000;
/// `ETH_MIN_MTU`.
pub const ETH_MIN_MTU: u32 = 68;
/// `IFNAMSIZ - 1`: the longest interface name.
pub const IFNAME_MAX: usize = 15;

/// One IPv4 address of an interface (`struct in_ifaddr`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr {
    pub ip: [u8; 4],
    pub prefix: u8,
    pub scope: u8,
    /// Seconds left (`INFINITY` with IFA_F_PERMANENT for an address without a lifetime).
    pub valid_s: u32,
    pub preferred_s: u32,
    /// NW-12: the flags besides IFA_F_PERMANENT (IFA_F_SECONDARY, and what a program asked for, such as
    /// IFA_F_NOPREFIXROUTE).
    pub extra: u32,
    /// NW-12: the label (`ifa_label`) when it is not the interface's name (an alias such as "lo:1").
    pub label: Option<String>,
    /// NW-12: a broadcast address a program gave a loopback address (cards compute theirs, `broadcast`).
    pub brd: Option<[u8; 4]>,
}

impl Addr {
    /// An address without extra flags, label or set broadcast address.
    pub fn plain(ip: [u8; 4], prefix: u8, scope: u8, valid_s: u32, preferred_s: u32) -> Addr {
        Addr { ip, prefix, scope, valid_s, preferred_s, extra: 0, label: None, brd: None }
    }
    pub fn flags(&self) -> u32 {
        self.extra | if self.valid_s == INFINITY { IFA_F_PERMANENT } else { 0 }
    }
    pub fn secondary(&self) -> bool {
        self.extra & IFA_F_SECONDARY != 0
    }
    /// The label as `ifa_label` holds it: the alias or the interface's name.
    pub fn label_or<'a>(&'a self, ifname: &'a str) -> &'a str {
        self.label.as_deref().unwrap_or(ifname)
    }
    /// Whether `ip` is in this address's subnet (`inet_ifa_match`).
    pub fn covers(&self, ip: [u8; 4]) -> bool {
        let m = u32::from_be_bytes(self.mask());
        u32::from_be_bytes(self.ip) & m == u32::from_be_bytes(ip) & m
    }
    pub fn mask(&self) -> [u8; 4] {
        let m = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix.min(32) as u32) };
        m.to_be_bytes()
    }
    /// Linux sets a broadcast address for an Ethernet card's address (`ip addr add ... brd +`, what DHCP clients
    /// do) and none for the loopback's; /31 and /32 have none.
    pub fn broadcast(&self, loopback: bool) -> Option<[u8; 4]> {
        if loopback {
            return self.brd;
        }
        if self.prefix >= 31 {
            return None;
        }
        let m = u32::from_be_bytes(self.mask());
        Some((u32::from_be_bytes(self.ip) | !m).to_be_bytes())
    }
}

/// NW-12: one IPv6 address of an interface (`struct inet6_ifaddr`): only a namespace's ::1 so far.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr6 {
    pub ip: [u8; 16],
    pub prefix: u8,
    pub scope: u8,
    pub flags: u32,
    /// `ifa_proto` (IFAPROT_KERNEL_LO for ::1).
    pub proto: u8,
}

/// `IFAPROT_KERNEL_LO` (include/uapi/linux/if_addr.h).
pub const IFAPROT_KERNEL_LO: u8 = 1;
pub const IFA_F_SECONDARY: u32 = 0x01;

/// lo's ::1/128 (`init_loopback`, linux-6.6/net/ipv6/addrconf.c:3214).
pub fn lo6() -> Addr6 {
    let mut ip = [0u8; 16];
    ip[15] = 1;
    Addr6 { ip, prefix: 128, scope: RT_SCOPE_HOST, flags: IFA_F_PERMANENT, proto: IFAPROT_KERNEL_LO }
}

/// One interface (`struct net_device` as far as programs see it).
#[derive(Clone, Debug)]
pub struct Link {
    pub index: u32,
    pub name: String,
    pub loopback: bool,
    pub mac: [u8; 6],
    pub mtu: u32,
    pub carrier: bool,
    pub carrier_up: u32,
    pub carrier_down: u32,
    pub stats: NetStats,
    pub addrs: Vec<Addr>,
    /// NW-12: IPv6 addresses (a namespace's lo while it is UP: ::1; V2 shows none elsewhere).
    pub addrs6: Vec<Addr6>,
    /// NW-12: `tx_queue_len` (DEFAULT_TX_QUEUE_LEN unless a program changed a namespace's lo).
    pub txqlen: u32,
    /// The flags set administratively: IFF_UP and the changeable ones a program set (IFF_PROMISC, IFF_NOARP,
    /// ...). Every card and the initial namespace's lo: IFF_UP; NW-11: a new network namespace's lo starts at 0.
    pub admin_flags: u32,
}

impl Link {
    /// `dev_get_flags`: RUNNING and LOWER_UP only while the device is up.
    pub fn flags(&self) -> u32 {
        let base = (if self.loopback { IFF_LOOPBACK } else { IFF_BROADCAST | IFF_MULTICAST }) | self.admin_flags;
        if self.up() && self.carrier { base | IFF_RUNNING | IFF_LOWER_UP } else { base }
    }
    /// Administratively up (IFF_UP).
    pub fn up(&self) -> bool {
        self.admin_flags & IFF_UP != 0
    }
    pub fn arphrd(&self) -> u16 {
        if self.loopback { ARPHRD_LOOPBACK } else { ARPHRD_ETHER }
    }
    /// The loopback has no operational state of its own (UNKNOWN, as Linux shows it) while it is up; a device
    /// that is down is DOWN (`rfc2863_policy`).
    pub fn operstate(&self) -> u8 {
        if !self.up() {
            IF_OPER_DOWN
        } else if self.loopback {
            IF_OPER_UNKNOWN
        } else if self.carrier {
            IF_OPER_UP
        } else {
            IF_OPER_DOWN
        }
    }
    pub fn broadcast_hw(&self) -> [u8; 6] {
        if self.loopback { [0; 6] } else { [0xff; 6] }
    }
    /// `min_mtu`/`max_mtu`: 0/0 for the loopback (Linux leaves them unset), ETH_MIN_MTU and the MTU the card
    /// runs at for a card (V2 does not change MTUs).
    pub fn min_mtu(&self) -> u32 {
        if self.loopback { 0 } else { ETH_MIN_MTU }
    }
    pub fn max_mtu(&self) -> u32 {
        if self.loopback { 0 } else { self.mtu }
    }
}

static VIEW_STACK: ksync::OnceRef<Stack> = ksync::OnceRef::new();

/// Host tests and ktests show the interfaces of their own stack instead of the kernel's.
pub fn set_stack(s: &'static Stack) {
    VIEW_STACK.set(s);
}

fn stack() -> Option<&'static Stack> {
    VIEW_STACK.get().or_else(crate::sock::inet::stack_opt)
}

fn loopback() -> Link {
    lo_link(IFF_UP, alloc::vec![lo_addr()], Vec::new(), LOOPBACK_MTU, TX_QUEUE_LEN)
}

/// lo's 127.0.0.1/8 (`inetdev_event` NETDEV_UP for a loopback, linux-6.6/net/ipv4/devinet.c:1575).
pub fn lo_addr() -> Addr {
    Addr::plain([127, 0, 0, 1], 8, RT_SCOPE_HOST, INFINITY, INFINITY)
}

/// lo with administrative flags `admin_flags` and these addresses, MTU and queue length.
pub(crate) fn lo_link(admin_flags: u32, addrs: Vec<Addr>, addrs6: Vec<Addr6>, mtu: u32, txqlen: u32) -> Link {
    Link {
        index: 1,
        name: String::from("lo"),
        loopback: true,
        mac: [0; 6],
        mtu,
        carrier: true,
        carrier_up: 0,
        carrier_down: 0,
        stats: NetStats::default(),
        addrs,
        addrs6,
        txqlen,
        admin_flags,
    }
}

/// A driver's name as an interface name (`dev_valid_name`): '/', ':', blanks and every non-ASCII character
/// become '_' (so one character is one byte), at most 15 bytes, never empty, "." or "..".
fn ifname(n: &str) -> String {
    let s: String = n.chars().map(|c| if c == '/' || c == ':' || c.is_whitespace() || !c.is_ascii() { '_' } else { c }).take(IFNAME_MAX).collect();
    if s.is_empty() || s == "." || s == ".." { String::from("net") } else { s }
}

/// `name` made unique among `taken` (two cards whose names are cut to the same 15 bytes, or a card called
/// "lo"): the card's index is appended, the name shortened to keep 15 bytes; a further clash takes the next
/// number (at most `taken.len()` of them can clash, so this ends).
fn unique(name: String, index: u32, taken: &[Link]) -> String {
    let clash = |n: &str| taken.iter().any(|l| l.name == n);
    if !clash(&name) {
        return name;
    }
    let mut k = index as u64;
    loop {
        let suffix = alloc::format!("{k}");
        let keep = IFNAME_MAX.saturating_sub(suffix.len()).min(name.len());
        let n = alloc::format!("{}{suffix}", &name[..keep]);
        if !clash(&n) {
            return n;
        }
        k += 1;
    }
}

/// Every interface, in index order: `lo`, then the stack's cards.
pub fn links() -> Vec<Link> {
    let mut v = alloc::vec![loopback()];
    let Some(st) = stack() else { return v };
    let now = crate::sock::now();
    for (i, d) in st.interface_details().into_iter().enumerate() {
        let c = &d.summary;
        let inf = &c.info;
        let addrs = inf
            .addr
            .map(|(a, prefix)| {
                let left = match (inf.dhcp, inf.lease_expires_ns) {
                    (true, Some(e)) => (e.saturating_sub(now) / 1_000_000_000).min(INFINITY as u64 - 1) as u32,
                    _ => INFINITY,
                };
                alloc::vec![Addr::plain(a.0, prefix, RT_SCOPE_UNIVERSE, left, left)]
            })
            .unwrap_or_default();
        let index = i as u32 + 2;
        let name = unique(ifname(&c.name), index, &v);
        v.push(Link {
            index,
            name,
            loopback: false,
            mac: c.mac,
            mtu: d.mtu,
            carrier: inf.link_up,
            carrier_up: d.carrier_up,
            carrier_down: d.carrier_down,
            stats: d.stats,
            addrs,
            addrs6: Vec::new(),
            txqlen: TX_QUEUE_LEN,
            admin_flags: IFF_UP,
        });
    }
    v
}

/// NW-12: one route of the initial namespace's main table as /proc/net/route shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route4 {
    pub dev: String,
    pub dst: [u8; 4],
    pub gateway: [u8; 4],
    pub prefix: u8,
    pub metric: u32,
}

/// NW-12: the main table of the initial namespace (the cards' routes; lo's are all in the local table): a
/// default route for each card with a router, a connected route for each card's subnet, each with the card's
/// metric, in table order (by destination, then metric).
pub fn routes4() -> Vec<Route4> {
    let names = links();
    let Some(st) = stack() else { return Vec::new() };
    let mut v = Vec::new();
    for (i, c) in st.interfaces().into_iter().enumerate() {
        let Some((a, prefix)) = c.info.addr else { continue };
        let Some(l) = names.iter().find(|l| l.index == i as u32 + 2) else { continue };
        if let Some(r) = c.info.router {
            v.push(Route4 { dev: l.name.clone(), dst: [0; 4], gateway: r.0, prefix: 0, metric: c.info.metric });
        }
        let m = crate::route::mask(prefix);
        let net = (u32::from_be_bytes(a.0) & m).to_be_bytes();
        v.push(Route4 { dev: l.name.clone(), dst: net, gateway: [0; 4], prefix, metric: c.info.metric });
    }
    v.sort_by_key(|r| (u32::from_be_bytes(r.dst), r.prefix, r.metric));
    v
}

pub fn by_index(index: u32) -> Option<Link> {
    links().into_iter().find(|l| l.index == index)
}

pub fn by_name(name: &[u8]) -> Option<Link> {
    links().into_iter().find(|l| l.name.as_bytes() == name)
}

/// NW-11: the interfaces of network namespace `net`: the initial one has `links()`, any other only its own lo.
pub fn links_in(net: &Net) -> Vec<Link> {
    match net {
        None => links(),
        Some(v) => alloc::vec![v.lo_link()],
    }
}

pub fn by_index_in(net: &Net, index: u32) -> Option<Link> {
    match net {
        None => by_index(index),
        Some(_) => links_in(net).into_iter().find(|l| l.index == index),
    }
}

pub fn by_name_in(net: &Net, name: &[u8]) -> Option<Link> {
    match net {
        None => by_name(name),
        Some(_) => links_in(net).into_iter().find(|l| l.name.as_bytes() == name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_flags() {
        let a = Addr::plain([192, 0, 2, 113], 24, 0, 100, 100);
        assert_eq!(a.mask(), [255, 255, 255, 0]);
        assert_eq!(a.broadcast(false), Some([192, 0, 2, 255]));
        assert_eq!(a.broadcast(true), None);
        assert_eq!(a.flags(), 0);
        let p = Addr { prefix: 32, valid_s: INFINITY, ..a.clone() };
        assert_eq!((p.mask(), p.broadcast(false), p.flags()), ([255; 4], None, IFA_F_PERMANENT));
        assert_eq!(Addr { prefix: 0, ..a }.mask(), [0; 4]);
        let lo = loopback();
        assert_eq!((lo.flags(), lo.arphrd(), lo.operstate()), (0x10049, 772, 0));
        let mut card = Link { loopback: false, carrier: true, ..lo };
        assert_eq!(card.flags(), 0x11043);
        card.carrier = false;
        assert_eq!((card.flags(), card.operstate()), (0x1003, IF_OPER_DOWN));
        assert_eq!(ifname("a very long card name/x:y"), "a_very_long_car");
        // Bytes, not characters: a non-ASCII name stays within 15 bytes; names stay unique.
        assert_eq!(ifname("kärtchen-ünd-mehr-zeichen"), "k_rtchen-_nd-me");
        assert_eq!(ifname(""), "net");
        let taken = alloc::vec![loopback(), Link { index: 2, name: String::from("a_very_long_car"), ..loopback() }];
        assert_eq!(unique(String::from("lo"), 3, &taken), "lo3");
        assert_eq!(unique(String::from("a_very_long_car"), 3, &taken), "a_very_long_ca3");
        assert_eq!(unique(String::from("igc1"), 3, &taken), "igc1");
        let taken2 = alloc::vec![loopback(), Link { index: 2, name: String::from("lo3"), ..loopback() }];
        assert_eq!(unique(String::from("lo"), 3, &taken2), "lo4");
    }
}
