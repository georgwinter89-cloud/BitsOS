//! DHCP decisions around smoltcp's client (pure, host-tested). smoltcp does DISCOVER/REQUEST, renew at T1
//! (unicast), rebind at T2 (broadcast), expiry and NAK; this part adds what V1 learnt on the Mini-PC:
//! - DISCOVER timing: V1 sent 7 tries one second apart (`dhcp.asm`); V2 keeps that start and then does not give
//!   up: 2, 4, 8, 16, 32 s, then once a minute (the cable may be plugged in later; link-up starts over at once).
//! - Cable watch (`netwatch.asm`): link lost = keep the address; link back = ask again (another network may be
//!   behind the cable), and keep the old address until an answer replaces it or the old lease runs out.

use crate::Ipv4;
use alloc::vec::Vec;

const SEC: u64 = 1_000_000_000;
/// V1's tries at one second (`dhcp.asm`: 7 × 1 s).
pub const FAST_TRIES: u32 = 7;
pub const SLOWEST_NS: u64 = 60 * SEC;

/// Time to wait after the `sent`-th DISCOVER (1-based) before the next one.
pub fn discover_interval_ns(sent: u32) -> u64 {
    if sent <= FAST_TRIES {
        return SEC;
    }
    let shift = (sent - FAST_TRIES).min(6);
    (SEC << shift).min(SLOWEST_NS)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub addr: Ipv4,
    pub prefix: u8,
    pub router: Option<Ipv4>,
    pub dns: Vec<Ipv4>,
    pub server: Ipv4,
    /// Lease time from the server (None: infinite).
    pub lease_ns: Option<u64>,
    pub got_at_ns: u64,
}

impl Lease {
    pub fn expires_at_ns(&self) -> Option<u64> {
        self.lease_ns.map(|l| self.got_at_ns.saturating_add(l))
    }
    /// Same address, prefix and router (a renew that changes nothing for routing).
    pub fn same_binding(&self, o: &Lease) -> bool {
        self.addr == o.addr && self.prefix == o.prefix && self.router == o.router
    }
}

/// What the stack does after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Put this address/route on the card (first lease, or a different one).
    Apply(Lease),
    /// Same binding, new times (a renew or rebind): keep everything, note the new expiry.
    Renewed,
    /// Take the address and route off the card.
    Remove,
    /// Restart DHCP on the card (link came back), keeping the address for now.
    Restart,
}

#[derive(Debug, Default)]
pub struct DhcpState {
    pub lease: Option<Lease>,
    /// A restart after link-up is under way: smoltcp's "deconfigured" from that restart is not a loss.
    restarting: bool,
    /// DISCOVERs sent since the last lease or restart.
    pub discovers: u32,
    link_up: Option<bool>,
}

impl DhcpState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The discover interval smoltcp should use for the next DISCOVER.
    pub fn next_interval_ns(&self) -> u64 {
        discover_interval_ns(self.discovers + 1)
    }

    /// A DISCOVER left the card.
    pub fn discover_sent(&mut self) {
        self.discovers = self.discovers.saturating_add(1);
    }

    /// The card's link as seen now. Returns `Restart` when link comes back after being down.
    pub fn link(&mut self, up: bool) -> Action {
        let was = self.link_up.replace(up);
        match (was, up) {
            (Some(false), true) => {
                self.restarting = self.lease.is_some();
                self.discovers = 0;
                Action::Restart
            }
            _ => Action::Nothing,
        }
    }

    /// smoltcp reported a configuration.
    pub fn configured(&mut self, l: Lease) -> Action {
        self.restarting = false;
        self.discovers = 0;
        match self.lease.replace(l.clone()) {
            Some(old) if old.same_binding(&l) => Action::Renewed,
            _ => Action::Apply(l),
        }
    }

    /// smoltcp dropped its configuration (lease expired, NAK, or our own restart).
    pub fn deconfigured(&mut self, now_ns: u64) -> Action {
        if self.restarting && self.lease.as_ref().is_some_and(|l| l.expires_at_ns().is_none_or(|e| now_ns < e)) {
            self.restarting = false;
            return Action::Nothing;
        }
        self.restarting = false;
        if self.lease.take().is_some() { Action::Remove } else { Action::Nothing }
    }

    /// Periodic check: a lease kept across a restart still runs out at its own time.
    pub fn tick(&mut self, now_ns: u64) -> Action {
        match self.lease.as_ref().and_then(Lease::expires_at_ns) {
            Some(e) if now_ns >= e => {
                self.lease = None;
                self.restarting = false;
                Action::Remove
            }
            _ => Action::Nothing,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(last: u8, t: u64, secs: Option<u64>) -> Lease {
        Lease {
            addr: Ipv4([192, 168, 100, last]),
            prefix: 24,
            router: Some(Ipv4([192, 168, 100, 1])),
            dns: alloc::vec![Ipv4([192, 168, 100, 1])],
            server: Ipv4([192, 168, 100, 1]),
            lease_ns: secs.map(|s| s * SEC),
            got_at_ns: t,
        }
    }

    #[test]
    fn discover_timing_is_v1_then_backs_off_without_giving_up() {
        let v: Vec<u64> = (1..=16).map(|n| discover_interval_ns(n) / SEC).collect();
        assert_eq!(v, [1, 1, 1, 1, 1, 1, 1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
        assert_eq!(discover_interval_ns(u32::MAX), SLOWEST_NS);
        let mut s = DhcpState::new();
        assert_eq!(s.next_interval_ns(), SEC);
        for _ in 0..7 {
            s.discover_sent();
        }
        assert_eq!(s.next_interval_ns(), 2 * SEC);
    }

    #[test]
    fn first_lease_applies_renew_keeps_change_applies() {
        let mut s = DhcpState::new();
        assert_eq!(s.configured(lease(113, 0, Some(864_000))), Action::Apply(lease(113, 0, Some(864_000))));
        assert_eq!(s.configured(lease(113, 5, Some(864_000))), Action::Renewed);
        assert!(matches!(s.configured(lease(114, 6, Some(864_000))), Action::Apply(l) if l.addr == Ipv4([192, 168, 100, 114])));
        assert_eq!(s.deconfigured(7), Action::Remove, "expiry or NAK: address goes");
        assert_eq!(s.deconfigured(8), Action::Nothing);
    }

    #[test]
    fn link_flap_keeps_the_address_until_answer_or_expiry() {
        let mut s = DhcpState::new();
        assert_eq!(s.link(true), Action::Nothing, "first sight of the link");
        s.configured(lease(113, 0, Some(100)));
        assert_eq!(s.link(false), Action::Nothing, "link lost: keep");
        assert_eq!(s.link(false), Action::Nothing);
        assert_eq!(s.link(true), Action::Restart);
        assert_eq!(s.deconfigured(10 * SEC), Action::Nothing, "our own restart is no loss");
        assert!(s.lease.is_some());
        assert_eq!(s.tick(99 * SEC), Action::Nothing);
        assert_eq!(s.tick(100 * SEC), Action::Remove, "old lease ran out without an answer");
        assert!(s.lease.is_none());
    }

    #[test]
    fn restart_answered_with_same_or_new_address() {
        let mut s = DhcpState::new();
        s.link(true);
        s.configured(lease(113, 0, Some(100)));
        s.link(false);
        s.discover_sent();
        assert_eq!(s.link(true), Action::Restart);
        assert_eq!(s.discovers, 0, "a restart starts the fast tries again");
        s.deconfigured(SEC);
        assert_eq!(s.configured(lease(113, 2 * SEC, Some(100))), Action::Renewed);
        s.link(false);
        s.link(true);
        s.deconfigured(3 * SEC);
        assert!(matches!(s.configured(lease(7, 4 * SEC, None)), Action::Apply(_)), "another network behind the cable");
        assert_eq!(s.tick(u64::MAX), Action::Nothing, "infinite lease");
    }

    #[test]
    fn restart_after_expiry_is_a_loss() {
        let mut s = DhcpState::new();
        s.link(true);
        s.configured(lease(113, 0, Some(10)));
        s.link(false);
        s.link(true);
        assert_eq!(s.deconfigured(11 * SEC), Action::Remove);
    }
}
