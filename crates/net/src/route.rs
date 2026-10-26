//! Which card a packet to an address leaves through (pure, host-tested). All cards are up at once; each card
//! with an address has a connected route for its subnet; the default route belongs to the card with the smallest
//! route metric (WL-4 CR 1: the cable 100 before the WLAN 600, as NetworkManager), among equals the one that got
//! the first DHCP lease, and moves to the next leased card with link while that one has no link (spec §2.3: the
//! routing picks the interface with link). 127.0.0.0/8 never gets here (the loopback takes it).

use crate::Ipv4;

/// What routing needs to know about one card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfRoute {
    /// Position in the stack's card list (registration order).
    pub index: usize,
    pub link_up: bool,
    /// Address and prefix length.
    pub addr: Option<(Ipv4, u8)>,
    pub router: Option<Ipv4>,
    /// Order in which the cards got their first lease (smaller = earlier); None = never leased (static).
    pub lease_seq: Option<u64>,
    /// Route metric, smaller wins (`NetDevice::route_metric`).
    pub metric: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pick {
    pub index: usize,
    /// The next hop when it is not the destination itself.
    pub via: Option<Ipv4>,
}

pub fn mask(prefix: u8) -> u32 {
    match prefix {
        0 => 0,
        p if p >= 32 => u32::MAX,
        p => u32::MAX << (32 - p),
    }
}

pub fn in_subnet(a: Ipv4, net: Ipv4, prefix: u8) -> bool {
    let m = mask(prefix);
    u32::from_be_bytes(a.0) & m == u32::from_be_bytes(net.0) & m
}

/// The card that owns the default route right now.
pub fn default_route(cards: &[IfRoute]) -> Option<Pick> {
    cards
        .iter()
        .filter(|c| c.link_up && c.addr.is_some() && c.router.is_some())
        .min_by_key(|c| (c.metric, c.lease_seq.unwrap_or(u64::MAX), c.index))
        .map(|c| Pick { index: c.index, via: c.router })
}

/// Where a packet to `dst` goes: a connected subnet first (among cards in it, the smallest metric, then the
/// first registered), else the default route.
/// None: no card can reach it (no address yet, no link, or no default route).
pub fn pick(cards: &[IfRoute], dst: Ipv4) -> Option<Pick> {
    if dst.is_unspecified() || dst.is_loopback() || dst == Ipv4::BROADCAST {
        return None;
    }
    let connected = cards
        .iter()
        .filter(|c| c.link_up && c.addr.is_some_and(|(a, p)| in_subnet(dst, a, p)))
        .min_by_key(|c| (c.metric, c.index));
    if let Some(c) = connected {
        return Some(Pick { index: c.index, via: None });
    }
    default_route(cards)
}

/// WL-4 CR 2: as `pick`, for a socket bound to one card (SO_BINDTODEVICE, Linux `sk_bound_dev_if`: the route
/// lookup only sees that card's routes): its connected subnet, else its own router, else none (ENETUNREACH).
/// `dev` None is `pick`.
pub fn pick_on(cards: &[IfRoute], dst: Ipv4, dev: Option<usize>) -> Option<Pick> {
    let Some(dev) = dev else { return pick(cards, dst) };
    if dst.is_unspecified() || dst.is_loopback() || dst == Ipv4::BROADCAST {
        return None;
    }
    let c = cards.iter().find(|c| c.index == dev && c.link_up)?;
    let (a, p) = c.addr?;
    if in_subnet(dst, a, p) {
        return Some(Pick { index: c.index, via: None });
    }
    c.router.map(|r| Pick { index: c.index, via: Some(r) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(index: usize, link_up: bool, addr: Option<([u8; 4], u8)>, router: Option<[u8; 4]>, lease_seq: Option<u64>) -> IfRoute {
        IfRoute { index, link_up, addr: addr.map(|(a, p)| (Ipv4(a), p)), router: router.map(Ipv4), lease_seq, metric: 100 }
    }

    /// WL-4 CR 2: a socket bound to the WLAN leaves through it although the cable owns the default route.
    #[test]
    fn a_bound_socket_keeps_to_its_card() {
        let mut cards = [
            IfRoute { metric: 600, ..card(0, true, Some(([192, 168, 100, 40], 24)), Some([192, 168, 100, 1]), Some(1)) },
            card(1, true, Some(([192, 168, 100, 113], 24)), Some([192, 168, 100, 1]), Some(2)),
            card(2, true, Some(([10, 0, 2, 15], 24)), None, None),
        ];
        assert_eq!(pick_on(&cards, Ipv4([8, 8, 8, 8]), Some(0)), Some(Pick { index: 0, via: Some(Ipv4([192, 168, 100, 1])) }));
        assert_eq!(pick_on(&cards, Ipv4([192, 168, 100, 20]), Some(0)), Some(Pick { index: 0, via: None }));
        assert_eq!(pick_on(&cards, Ipv4([8, 8, 8, 8]), None).map(|p| p.index), Some(1));
        assert_eq!(pick_on(&cards, Ipv4([8, 8, 8, 8]), Some(2)), None, "no router on the bound card");
        assert_eq!(pick_on(&cards, Ipv4([10, 0, 2, 2]), Some(2)), Some(Pick { index: 2, via: None }));
        assert_eq!(pick_on(&cards, Ipv4([8, 8, 8, 8]), Some(7)), None, "no such card");
        cards[0].link_up = false;
        assert_eq!(pick_on(&cards, Ipv4([8, 8, 8, 8]), Some(0)), None, "the bound card has no link");
    }

    /// WL-4 CR 1: the WLAN (600) leased first and registered first, the cable (100) leased later: the cable owns
    /// the default route and the shared subnet; without the cable's link both go to the WLAN, and come back.
    #[test]
    fn the_cable_wins_over_the_wlan() {
        let mut cards = [
            IfRoute { metric: 600, ..card(0, true, Some(([192, 168, 100, 40], 24)), Some([192, 168, 100, 1]), Some(1)) },
            card(1, true, Some(([192, 168, 100, 113], 24)), Some([192, 168, 100, 1]), Some(2)),
        ];
        assert_eq!(default_route(&cards).map(|p| p.index), Some(1));
        assert_eq!(pick(&cards, Ipv4([192, 168, 100, 20])).map(|p| p.index), Some(1));
        assert_eq!(pick(&cards, Ipv4([8, 8, 8, 8])), Some(Pick { index: 1, via: Some(Ipv4([192, 168, 100, 1])) }));
        cards[1].link_up = false;
        assert_eq!(default_route(&cards).map(|p| p.index), Some(0));
        assert_eq!(pick(&cards, Ipv4([192, 168, 100, 20])).map(|p| p.index), Some(0));
        cards[1].link_up = true;
        assert_eq!(default_route(&cards).map(|p| p.index), Some(1));
    }

    #[test]
    fn masks() {
        assert_eq!(mask(0), 0);
        assert_eq!(mask(24), 0xFFFF_FF00);
        assert_eq!(mask(32), u32::MAX);
        assert_eq!(mask(40), u32::MAX);
        assert!(in_subnet(Ipv4([10, 0, 2, 2]), Ipv4([10, 0, 2, 15]), 24));
        assert!(!in_subnet(Ipv4([10, 0, 3, 2]), Ipv4([10, 0, 2, 15]), 24));
        assert!(in_subnet(Ipv4([1, 2, 3, 4]), Ipv4([10, 0, 2, 15]), 0));
    }

    #[test]
    fn connected_beats_default_and_first_lease_owns_default() {
        // Card 0 leased second (QEMU user net), card 1 leased first (the fake LAN).
        let cards = [
            card(0, true, Some(([10, 0, 2, 15], 24)), Some([10, 0, 2, 2]), Some(2)),
            card(1, true, Some(([192, 168, 77, 20], 24)), Some([192, 168, 77, 1]), Some(1)),
        ];
        assert_eq!(pick(&cards, Ipv4([10, 0, 2, 2])), Some(Pick { index: 0, via: None }));
        assert_eq!(pick(&cards, Ipv4([192, 168, 77, 1])), Some(Pick { index: 1, via: None }));
        assert_eq!(pick(&cards, Ipv4([8, 8, 8, 8])), Some(Pick { index: 1, via: Some(Ipv4([192, 168, 77, 1])) }));
    }

    #[test]
    fn default_moves_while_link_is_down_and_comes_back() {
        let mut cards = [
            card(0, true, Some(([10, 0, 2, 15], 24)), Some([10, 0, 2, 2]), Some(1)),
            card(1, true, Some(([192, 168, 77, 20], 24)), Some([192, 168, 77, 1]), Some(2)),
        ];
        assert_eq!(default_route(&cards).map(|p| p.index), Some(0));
        cards[0].link_up = false;
        assert_eq!(default_route(&cards).map(|p| p.index), Some(1));
        assert_eq!(pick(&cards, Ipv4([10, 0, 2, 2])).map(|p| p.index), Some(1), "no link: not even the connected route");
        cards[0].link_up = true;
        assert_eq!(default_route(&cards).map(|p| p.index), Some(0));
    }

    #[test]
    fn nothing_without_address_or_router() {
        let cards = [card(0, true, None, None, None), card(1, false, Some(([192, 168, 1, 5], 24)), Some([192, 168, 1, 1]), Some(1))];
        assert_eq!(pick(&cards, Ipv4([8, 8, 8, 8])), None);
        let cards = [card(0, true, Some(([192, 168, 1, 5], 24)), None, None)];
        assert_eq!(pick(&cards, Ipv4([192, 168, 1, 9])), Some(Pick { index: 0, via: None }), "static, no router: connected only");
        assert_eq!(pick(&cards, Ipv4([8, 8, 8, 8])), None);
        assert_eq!(pick(&cards, Ipv4::LOCALHOST), None);
        assert_eq!(pick(&cards, Ipv4::BROADCAST), None);
        assert_eq!(pick(&[], Ipv4([1, 1, 1, 1])), None);
    }

    #[test]
    fn same_subnet_on_two_cards_takes_the_first() {
        let cards = [
            card(0, true, Some(([192, 168, 100, 20], 24)), Some([192, 168, 100, 1]), Some(2)),
            card(1, true, Some(([192, 168, 100, 21], 24)), Some([192, 168, 100, 1]), Some(1)),
        ];
        assert_eq!(pick(&cards, Ipv4([192, 168, 100, 1])).map(|p| p.index), Some(0));
    }
}
