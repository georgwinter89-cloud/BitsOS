//! The network interfaces as programs see them (NW-6): Linux's `net_device` view of the kernel's stack, for the
//! rtnetlink dumps (`sock::rtnl`) and the SIOCGIF* ioctls (`sock::devioctl`).
//!
//! Index 1 is `lo` (127.0.0.1/8, as on every Linux); the stack's cards follow in registration order from index 2
//! (`igc0` = 2 on the Mini-PC), under the names the drivers gave them (the names `/proc/net/dhcp` and the log
//! use). V2 has IPv4 only, one address per card at most (its DHCP lease or static address), and every card is
//! administratively up (the kernel's stack drives all of them): flags as Linux's `dev_get_flags` shows a card
//! that is up, RUNNING and LOWER_UP with carrier.

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
}

impl Addr {
    pub fn flags(&self) -> u32 {
        if self.valid_s == INFINITY { IFA_F_PERMANENT } else { 0 }
    }
    pub fn mask(&self) -> [u8; 4] {
        let m = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix.min(32) as u32) };
        m.to_be_bytes()
    }
    /// Linux sets a broadcast address for an Ethernet card's address (`ip addr add ... brd +`, what DHCP clients
    /// do) and none for the loopback's; /31 and /32 have none.
    pub fn broadcast(&self, loopback: bool) -> Option<[u8; 4]> {
        if loopback || self.prefix >= 31 {
            return None;
        }
        let m = u32::from_be_bytes(self.mask());
        Some((u32::from_be_bytes(self.ip) | !m).to_be_bytes())
    }
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
}

impl Link {
    /// `dev_get_flags` of a device that is up.
    pub fn flags(&self) -> u32 {
        let base = if self.loopback { IFF_UP | IFF_LOOPBACK } else { IFF_UP | IFF_BROADCAST | IFF_MULTICAST };
        if self.carrier { base | IFF_RUNNING | IFF_LOWER_UP } else { base }
    }
    pub fn arphrd(&self) -> u16 {
        if self.loopback { ARPHRD_LOOPBACK } else { ARPHRD_ETHER }
    }
    /// The loopback has no operational state of its own (UNKNOWN, as Linux shows it).
    pub fn operstate(&self) -> u8 {
        if self.loopback {
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
    Link {
        index: 1,
        name: String::from("lo"),
        loopback: true,
        mac: [0; 6],
        mtu: LOOPBACK_MTU,
        carrier: true,
        carrier_up: 0,
        carrier_down: 0,
        stats: NetStats::default(),
        addrs: alloc::vec![Addr { ip: [127, 0, 0, 1], prefix: 8, scope: RT_SCOPE_HOST, valid_s: INFINITY, preferred_s: INFINITY }],
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
                alloc::vec![Addr { ip: a.0, prefix, scope: RT_SCOPE_UNIVERSE, valid_s: left, preferred_s: left }]
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
        });
    }
    v
}

pub fn by_index(index: u32) -> Option<Link> {
    links().into_iter().find(|l| l.index == index)
}

pub fn by_name(name: &[u8]) -> Option<Link> {
    links().into_iter().find(|l| l.name.as_bytes() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_and_flags() {
        let a = Addr { ip: [192, 168, 100, 113], prefix: 24, scope: 0, valid_s: 100, preferred_s: 100 };
        assert_eq!(a.mask(), [255, 255, 255, 0]);
        assert_eq!(a.broadcast(false), Some([192, 168, 100, 255]));
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
