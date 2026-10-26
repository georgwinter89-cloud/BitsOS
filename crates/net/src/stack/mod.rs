//! The kernel's `KSocket` (NW-3): a smoltcp interface on every network card the drivers register (now or
//! later), the loopback for 127.0.0.0/8, routing over all cards (`route`), DHCP with renew/rebind and V1's
//! DISCOVER timing (`dhcp`), ARP and ICMP echo (smoltcp), TCP and UDP sockets, and one poll thread (class
//! KernelRt, B4) that wakes on a `time::HrTimer` at smoltcp's next deadline, on a kick from a socket call, and
//! at least every `RX_POLL_NS` to look for received frames (the cards are polled, NW-2's MSI comes later).
//!
//! Locks (R5): `ifaces`, `listeners` and each card's `info` are short spin sections; a card's `inner` is held for
//! one bounded poll round or one socket call, never across a wait. Accept queues are fed and streams dropped
//! only after the card's lock is released (a dropped stream takes it again). Waiting happens on `wq` (socket
//! readiness, woken after every round with activity) with the caller's timeout.
//!
//! R9: the stack itself only sends DHCP, ARP and answers (ICMP echo replies, TCP resets); every other packet
//! comes from a socket a kernel user opened. The emergency transmit (`NetDevice::poll_tx`) is not touched.

pub mod dhcp;
pub mod iface;
#[cfg(target_os = "none")]
mod kernel;
mod phy;
pub(crate) mod sockets;
pub mod tcp;
pub mod udp;
pub(crate) mod wake;

use crate::accept::AcceptQueue;
use crate::loopback::Loopback;
use crate::route::{self, IfRoute, Pick};
use crate::{Ipv4, NetError};
#[cfg(test)]
use crate::{KSocket, SockAddr, TcpStream};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use devmodel::net::NetDevice;
use iface::{Iface, Note};
#[cfg(target_os = "none")]
pub use kernel::{hold_cards, kernel, seed, start};

/// Off the kernel there is no kernel stack to hold (the drivers' ktest files also build on the host).
#[cfg(not(target_os = "none"))]
pub fn hold_cards() -> Option<Hold> {
    None
}

/// Off the kernel there is no kernel stack (the drivers' ktest files also build on the host).
#[cfg(not(target_os = "none"))]
pub fn kernel() -> Option<&'static Stack> {
    None
}
use ksync::{SpinLock, WaitQueue};

/// Longest a quiet stack goes without looking at the cards (they are polled; with MSI this becomes a fallback).
pub const RX_POLL_NS: u64 = 500_000;
/// While traffic flows (a frame or a socket call within `ACTIVE_NS`), the cards are looked at this often: an
/// answer is seen within tens of µs instead of half a millisecond (ping, MCP requests, ACK clocking).
pub const RX_POLL_ACTIVE_NS: u64 = 50_000;
pub const ACTIVE_NS: u64 = 5_000_000;
/// With every card interrupt-driven (NW-4) the cards are still looked at this often, in case an interrupt was
/// lost (a moved vector, a card that fell off the bus): Linux igc's watchdog raises a software interrupt for the
/// same reason. Cheap: one round over quiet cards every 100 ms.
pub const IRQ_SAFETY_NS: u64 = 100_000_000;
/// After a round that hit the frame budget: a short pause so lower classes get the CPU (cooperative G0).
#[cfg(target_os = "none")]
const BUSY_PAUSE_NS: u64 = 20_000;
/// After the heap refused a background socket buffer (a listener's refill, the reserve's spares) the poll thread
/// waits this long before it asks again, doubling up to `BG_RETRY_MAX_NS` (NW-7). Every refusal costs the heap a
/// pass through its caches; asking every round (20 µs apart under load) would keep the heap busy for nothing.
pub(crate) const BG_RETRY_MIN_NS: u64 = 1_000_000;
pub(crate) const BG_RETRY_MAX_NS: u64 = 250_000_000;

klog::counter!(TCP_CONNECTS, "net.tcp_connects");
klog::counter!(TCP_ACCEPTS, "net.tcp_accepts");
klog::counter!(pub TCP_REFUSED_PUBLIC, "net.tcp_refused_public");
klog::counter!(pub(crate) TX_DROPPED, "net.tx_dropped");
// Frames a full TX ring refused, held and sent in a later round (NW-7 fixer).
klog::counter!(pub(crate) TX_REQUEUED, "net.tx_requeued");
klog::counter!(pub NO_MEMORY, "net.no_memory");
// Refusals a test injected (`inject_no_memory`): kept apart so `net.no_memory` counts only what the heap refused
// (NW-7: the Mini-PC's `net.no_memory=529` in A1 were all injected by the Safe ktests).
klog::counter!(pub NO_MEMORY_INJECTED, "net.no_memory_injected");
// Background allocations (refills, the reserve's spares) skipped because the last one was refused recently.
klog::counter!(pub BG_ALLOC_DEFERRED, "net.bg_alloc_deferred");
klog::counter!(pub RX_BUSY_ROUNDS, "net.rx_budget_rounds");
klog::counter!(pub POLL_ROUNDS, "net.poll_rounds");

/// A listener's shared part: the poll thread keeps `backlog` listening sockets per card for it.
pub struct ListenShared {
    pub ip: Ipv4,
    pub port: u16,
    pub backlog: usize,
    pub queue: Arc<AcceptQueue>,
    /// Spec §2.4: SYNs from outside the private networks get RST (MCP always; a per-listener policy).
    pub private_only: bool,
    /// The lifeline's reserve (MCP, NW-4): the listening sockets come from the stack's kept pool and go back
    /// there, at least `iface::RESERVE_MIN` per card.
    pub reserve: bool,
    /// Who listens (named in the connection records of what it accepts, B3).
    pub owner: policy::Subject,
    /// A program's listener (NW-5): its connections' buffer sizes (receive, send), and at most
    /// `iface::PROG_PRECREATE` listening sockets made ahead per card (its backlog is still honoured: refills
    /// come every round while the accept queue is below the backlog).
    pub prog: Option<(usize, usize)>,
}

/// One card as `interfaces()` reports it.
#[derive(Clone, Debug)]
pub struct IfSummary {
    pub name: String,
    pub mac: [u8; 6],
    pub info: iface::Info,
    pub default_route: bool,
}

/// A card as netlink and the SIOCGIF* ioctls show it (NW-6): the summary plus MTU, counters and carrier changes.
pub struct IfDetail {
    pub summary: IfSummary,
    pub mtu: u32,
    pub stats: devmodel::net::NetStats,
    pub carrier_up: u32,
    pub carrier_down: u32,
}

pub struct Stack {
    pub(crate) clock: fn() -> u64,
    pub(crate) lo: Loopback,
    ifaces: SpinLock<Vec<Arc<Iface>>>,
    listeners: SpinLock<Vec<Arc<ListenShared>>>,
    /// Changes whenever a listener comes or goes (the poll thread's copy is refreshed then).
    listeners_gen: AtomicU64,
    next_port: SpinLock<u16>,
    /// Where the ephemeral counter started (random per boot, RFC 6056; for `stack.start` and the ktest).
    first_port: u16,
    /// Socket readiness: every waiting socket call sleeps here; woken after each round with activity.
    pub(crate) wq: WaitQueue,
    poll_wq: WaitQueue,
    kicked: AtomicBool,
    /// Last round with traffic or the last kick (for the fast poll interval).
    last_active_ns: AtomicU64,
    hold: AtomicUsize,
    lease_seq: AtomicU64,
    default_if: AtomicUsize,
    seed: u64,
    /// Cards come from `netdev` (kernel) or are added by hand (host tests).
    auto_devices: bool,
    /// The stack itself, once it has its final ('static) place: sockets keep a reference to it.
    me: ksync::OnceRef<Stack>,
    /// Fault injection (tests): this many buffer allocations fail as if memory were short ...
    inject_nomem: AtomicU32,
    /// ... but only on this thread (`ANY_THREAD`: on every thread), so the poll thread's own refills and other
    /// users of the kernel's stack are not hit by a ktest's injection.
    inject_thread: AtomicU64,
    /// Background allocations wait until this time after a refusal (`bg_alloc_open`), with this pause (doubling).
    bg_next_ns: AtomicU64,
    bg_gap_ns: AtomicU64,
    /// Connections smoltcp woke since the last round (NW-7, `wake.rs`), woken after the round.
    pub(crate) wakes: Arc<wake::WakeList>,
    /// The kept pool (MCP reserve, NW-4): TCP sockets with their buffers, made while memory was there, reused
    /// by reserved listeners and never handed back to the heap below `kept_idle_cap` idle ones.
    kept: SpinLock<Vec<smoltcp::socket::tcp::Socket<'static>>>,
    /// Kept sockets made so far (in the pool, listening, or carrying a connection).
    kept_made: AtomicUsize,
    /// Idle kept sockets the pool holds at most per card: what every reserved listener wants (its listening
    /// sockets plus its spares). Beyond it a returning socket is freed (a burst of connections does not pin
    /// memory for good).
    kept_want: AtomicUsize,
    /// Idle spares the pool keeps ready per card (`RESERVE_MIN` per reserved listener): made while memory is
    /// there (at `tcp_listen_private`, topped up every round), so the listening sockets an accept used are
    /// refilled from the pool even when the heap is empty.
    kept_spare: AtomicUsize,
    /// Spares being made right now (claimed under `kept`'s lock).
    kept_pending: AtomicUsize,
    /// The last top-up ran out of memory (logged once per change).
    kept_short: AtomicBool,
    /// Kept sockets taken again from the pool (this stack's share of `net.kept_reused`).
    kept_reused: AtomicU64,
    /// Kept sockets given back after their connection or listener ended.
    kept_returned: AtomicU64,
}

/// What `give_back` takes: a TCP socket, either as smoltcp's socket enum (fresh out of a socket set; anything
/// but TCP is dropped) or as the TCP socket itself.
pub(crate) struct KeptSock(Option<smoltcp::socket::tcp::Socket<'static>>);

impl From<smoltcp::socket::tcp::Socket<'static>> for KeptSock {
    fn from(s: smoltcp::socket::tcp::Socket<'static>) -> Self {
        KeptSock(Some(s))
    }
}

impl From<smoltcp::socket::Socket<'static>> for KeptSock {
    fn from(s: smoltcp::socket::Socket<'static>) -> Self {
        match s {
            smoltcp::socket::Socket::Tcp(t) => KeptSock(Some(t)),
            _ => KeptSock(None),
        }
    }
}

klog::counter!(pub KEPT_REUSED, "net.kept_reused");
klog::counter!(pub KEPT_MADE, "net.kept_made");
klog::counter!(KEPT_FREED, "net.kept_freed");

/// The kept pool's numbers (net_info, ktests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeptInfo {
    /// Idle sockets in the pool.
    pub idle: usize,
    /// Made in total and not freed (idle + listening + carrying a connection).
    pub made: usize,
    /// Idle sockets the pool keeps at most.
    pub cap: usize,
    /// Idle spares the pool keeps ready (topped up while memory is there).
    pub spare: usize,
    /// Taken again from the pool so far.
    pub reused: u64,
    /// Given back after their connection (or listener) ended, so far.
    pub returned: u64,
}

const ANY_THREAD: u64 = u64::MAX;

pub const EPHEMERAL: core::ops::RangeInclusive<u16> = 49152..=65535;
const NO_IF: usize = usize::MAX;

/// SplitMix64's finaliser: spreads every bit of a weak seed (a TSC reading) over the result.
pub fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// RFC 6056 §3.3.2 ("simple hash-based" in its simplest form, Linux's random offset): the ephemeral counter
/// starts at a point of the range chosen from the stack's seed, then counts up by one. A reloaded kernel thus
/// does not start at 49152 again and reuse the previous kernel's 4-tuples, which a server still holding the old
/// connection (TIME_WAIT, slirp) answers by dropping the SYN (RB-5a: 7 s of SYN retries per fetch).
pub fn ephemeral_start(seed: u64) -> u16 {
    let n = (*EPHEMERAL.end() - *EPHEMERAL.start()) as u64 + 1;
    *EPHEMERAL.start() + (mix64(seed) % n) as u16
}

impl Stack {
    /// A stack without cards. `clock` gives monotonic nanoseconds; `auto_devices` takes cards from
    /// `netdev::devices()` on every round (the kernel); host tests add them with `add_device`.
    pub fn new(clock: fn() -> u64, seed: u64, auto_devices: bool) -> Self {
        let first_port = ephemeral_start(seed);
        Stack {
            clock,
            lo: Loopback::new(),
            ifaces: SpinLock::named("net.ifaces", Vec::new()),
            listeners: SpinLock::named("net.listeners", Vec::new()),
            listeners_gen: AtomicU64::new(0),
            next_port: SpinLock::named("net.ports", first_port),
            first_port,
            wq: WaitQueue::new(),
            poll_wq: WaitQueue::new(),
            kicked: AtomicBool::new(false),
            last_active_ns: AtomicU64::new(0),
            hold: AtomicUsize::new(0),
            lease_seq: AtomicU64::new(1),
            default_if: AtomicUsize::new(NO_IF),
            seed,
            auto_devices,
            me: ksync::OnceRef::new(),
            inject_nomem: AtomicU32::new(0),
            inject_thread: AtomicU64::new(ANY_THREAD),
            bg_next_ns: AtomicU64::new(0),
            bg_gap_ns: AtomicU64::new(0),
            wakes: wake::WakeList::new(),
            kept: SpinLock::named("net.kept", Vec::new()),
            kept_made: AtomicUsize::new(0),
            kept_want: AtomicUsize::new(0),
            kept_spare: AtomicUsize::new(0),
            kept_pending: AtomicUsize::new(0),
            kept_short: AtomicBool::new(false),
            kept_reused: AtomicU64::new(0),
            kept_returned: AtomicU64::new(0),
        }
    }

    /// A socket for a reserved listener: one from the kept pool (its spares, or one a closed connection gave
    /// back), else a new one (made with memory asked for, `NoMemory` when there is none: the spares are made
    /// while memory is there, `top_up_kept`, so that this rarely happens).
    pub(crate) fn take_kept(&self) -> Result<smoltcp::socket::tcp::Socket<'static>, NetError> {
        if let Some(s) = self.kept.lock().pop() {
            KEPT_REUSED.inc();
            self.kept_reused.fetch_add(1, Ordering::Relaxed);
            return Ok(s);
        }
        let s = tcp::new_socket(self, tcp::LISTEN_RX, tcp::LISTEN_TX)?;
        self.kept_made.fetch_add(1, Ordering::AcqRel);
        KEPT_MADE.inc();
        Ok(s)
    }

    /// A kept socket whose connection ended (or whose listener went): back into the pool, or freed when the pool
    /// already holds what every reserved listener wants on every card. Short (the card's lock may be held).
    pub(crate) fn give_back(&self, s: impl Into<KeptSock>) {
        let KeptSock(Some(mut s)) = s.into() else { return };
        self.kept_returned.fetch_add(1, Ordering::Relaxed);
        // Its old connection must never answer again: an abort only changes state here (the socket is out of
        // every socket set, nothing is sent); `listen` resets it fully when it is taken.
        s.abort();
        let cap = self.kept_cap();
        let mut p = self.kept.lock();
        if p.len() < cap {
            p.push(s);
            return;
        }
        drop(p);
        self.kept_made.fetch_sub(1, Ordering::AcqRel);
        KEPT_FREED.inc();
        drop(s);
    }

    /// Frees idle kept sockets beyond the cap (a reserved listener went).
    pub(crate) fn trim_kept(&self) {
        let cap = self.kept_cap();
        let extra: Vec<smoltcp::socket::tcp::Socket<'static>> = {
            let mut p = self.kept.lock();
            let n = p.len().saturating_sub(cap);
            let at = p.len() - n;
            p.split_off(at)
        };
        self.kept_made.fetch_sub(extra.len(), Ordering::AcqRel);
        KEPT_FREED.add(extra.len() as u64);
        drop(extra);
    }

    /// Makes idle spares until the pool holds `kept_spare` per card (the poll thread every round, and
    /// `tcp_listen_private` before it returns). Allocates nothing when the pool is full (a quiet round stays
    /// allocation-free) and stops at the first buffer the heap refuses (R2; the next round tries again). Returns
    /// whether the pool is full afterwards.
    pub(crate) fn top_up_kept(&self, background: bool) -> bool {
        let per_card = self.kept_spare.load(Ordering::Acquire);
        if per_card == 0 {
            self.kept_short.store(false, Ordering::Release);
            return true;
        }
        let target = per_card.saturating_mul(self.ifaces.lock().len().max(1));
        let mut made = 0usize;
        let full = loop {
            {
                // A slot is claimed under the pool's lock, so the poll thread and `tcp_listen_private` topping up
                // at once never make more than the target together.
                let p = self.kept.lock();
                if p.len() + self.kept_pending.load(Ordering::Acquire) >= target {
                    break true;
                }
                self.kept_pending.fetch_add(1, Ordering::AcqRel);
            }
            // Made outside the pool's lock (the allocation may take long; `take_kept` must not wait for it).
            let s = if background && !self.bg_alloc_open() {
                Err(NetError::NoMemory)
            } else {
                let s = tcp::new_socket(self, tcp::LISTEN_RX, tcp::LISTEN_TX);
                if background {
                    self.bg_alloc_done(s.is_ok());
                }
                s
            };
            let mut p = self.kept.lock();
            self.kept_pending.fetch_sub(1, Ordering::AcqRel);
            match s {
                Ok(s) => {
                    self.kept_made.fetch_add(1, Ordering::AcqRel);
                    KEPT_MADE.inc();
                    p.push(s);
                    made += 1;
                }
                Err(_) => break false,
            }
        };
        if self.kept_short.swap(!full, Ordering::AcqRel) != !full {
            let idle = self.kept.lock().len();
            if full {
                klog::event!(Info, "net", "kept.spares_back", idle = idle, target = target, made = made);
            } else {
                klog::event!(Warn, "net", "kept.spares_short", idle = idle, target = target, made = made;
                    "no memory for the MCP reserve's spares; listening sockets refill from what is idle; retried after a growing pause (up to 250 ms)");
            }
        }
        full
    }

    fn kept_cap(&self) -> usize {
        let cards = self.ifaces.lock().len().max(1);
        self.kept_want.load(Ordering::Acquire).saturating_mul(cards)
    }

    pub fn kept_info(&self) -> KeptInfo {
        let cards = self.ifaces.lock().len().max(1);
        KeptInfo {
            idle: self.kept.lock().len(),
            made: self.kept_made.load(Ordering::Acquire),
            cap: self.kept_cap(),
            spare: self.kept_spare.load(Ordering::Acquire).saturating_mul(cards),
            reused: self.kept_reused.load(Ordering::Relaxed),
            returned: self.kept_returned.load(Ordering::Relaxed),
        }
    }

    /// A reserved listener comes (`on`) or goes: the pool's cap and spare target per card follow it.
    pub(crate) fn kept_reserve(&self, listen_want: usize, on: bool) {
        let want = listen_want.saturating_add(iface::RESERVE_MIN);
        if on {
            self.kept_want.fetch_add(want, Ordering::AcqRel);
            self.kept_spare.fetch_add(iface::RESERVE_MIN, Ordering::AcqRel);
        } else {
            self.kept_want.fetch_sub(want, Ordering::AcqRel);
            self.kept_spare.fetch_sub(iface::RESERVE_MIN, Ordering::AcqRel);
            self.trim_kept();
        }
    }

    /// A stack in its final place (leaked; host tests make several).
    pub fn new_static(clock: fn() -> u64, seed: u64, auto_devices: bool) -> &'static Stack {
        let s: &'static Stack = Box::leak(Box::new(Stack::new(clock, seed, auto_devices)));
        s.me.set(s);
        s
    }

    fn this(&self) -> Result<&'static Stack, NetError> {
        self.me.get().ok_or(NetError::Other)
    }

    pub(crate) fn now(&self) -> u64 {
        (self.clock)()
    }

    pub(crate) fn next_lease_seq(&self) -> u64 {
        self.lease_seq.fetch_add(1, Ordering::AcqRel)
    }

    /// Wakes the poll thread (data queued, a socket closed, a listener came or went).
    pub fn kick(&self) {
        self.last_active_ns.store(self.now(), Ordering::Relaxed);
        self.wake_poll();
    }

    /// Wakes the poll thread without counting as traffic (its timer).
    pub(crate) fn wake_poll(&self) {
        self.kicked.store(true, Ordering::Release);
        if self.poll_wq.waiter_count() > 0 {
            self.poll_wq.wake_all();
        }
    }

    /// Whether traffic flowed within `ACTIVE_NS` (the cards are then looked at every `RX_POLL_ACTIVE_NS`).
    #[cfg_attr(not(target_os = "none"), allow(dead_code))]
    pub(crate) fn active(&self, now: u64) -> bool {
        now.saturating_sub(self.last_active_ns.load(Ordering::Relaxed)) < ACTIVE_NS
    }

    /// Wakes every socket call waiting for readiness.
    pub(crate) fn wake_sockets(&self) {
        self.wake_kernel_waiters();
        crate::sock::inet::notify();
    }

    /// Wakes the kernel's waits on the stack's queue (connects, UDP, ping; NW-7: no stream reads or writes,
    /// those sleep on their own connection).
    fn wake_kernel_waiters(&self) {
        if self.wq.waiter_count() > 0 {
            self.wq.wake_all();
        }
    }

    /// Adds a card (host tests; the kernel's stack picks up `netdev` registrations itself). With `dhcp` false
    /// the card waits for `set_static`.
    pub fn add_device(&self, dev: Arc<dyn NetDevice>, dhcp: bool) -> usize {
        let index = {
            let mut l = self.ifaces.lock();
            self.push_iface(&mut l, dev.clone(), dhcp);
            l.len() - 1
        };
        self.added(&*dev, index, dhcp);
        index
    }

    fn push_iface(&self, l: &mut Vec<Arc<Iface>>, dev: Arc<dyn NetDevice>, dhcp: bool) {
        let index = l.len();
        l.push(Arc::new(Iface::new(index, dev, self.now(), self.seed.wrapping_add(index as u64), dhcp)));
    }

    fn added(&self, dev: &dyn NetDevice, index: usize, dhcp: bool) {
        let link = dev.link();
        klog::event!(Info, "net", "iface.added", ifname = dev.name(), index = index, dhcp = dhcp, mac = crate::netdev::mac_str(dev.mac().0),
            link_up = link.up, speed_mbps = link.speed_mbps);
        self.kick();
    }

    /// Cards the drivers registered since the last look (checked and added under one lock: no card twice).
    fn sync_devices(&self) {
        if !self.auto_devices || crate::netdev::count() <= self.ifaces.lock().len() {
            return;
        }
        let devs = crate::netdev::devices();
        let new: Vec<(Arc<dyn NetDevice>, usize)> = {
            let mut l = self.ifaces.lock();
            let have = l.len();
            devs.into_iter()
                .skip(have)
                .map(|d| {
                    self.push_iface(&mut l, d.clone(), true);
                    (d, l.len() - 1)
                })
                .collect()
        };
        for (d, i) in new {
            self.added(&*d, i, true);
        }
    }

    /// A fixed address on card `name` instead of DHCP.
    pub fn set_static(&self, name: &str, addr: Ipv4, prefix: u8, router: Option<Ipv4>) -> Result<(), NetError> {
        let i = self.ifaces.lock().iter().find(|i| i.name == name).cloned().ok_or(NetError::Unreachable)?;
        i.set_static(addr, prefix, router);
        klog::event!(Info, "net", "addr.static", ifname = name, ip = ip_str(addr), prefix = prefix);
        self.kick();
        crate::sock::netlink::changed();
        Ok(())
    }

    /// Every card, including ones registered a moment ago (a socket call never misses a new card).
    fn iface_list(&self) -> Vec<Arc<Iface>> {
        self.sync_devices();
        self.ifaces.lock().clone()
    }

    fn routes(&self) -> (Vec<Arc<Iface>>, Vec<IfRoute>) {
        let list = self.iface_list();
        let r = list
            .iter()
            .map(|i| {
                let inf = i.info.lock();
                IfRoute { index: i.index, link_up: inf.link_up, addr: inf.addr, router: inf.router, lease_seq: inf.lease_seq, metric: inf.metric }
            })
            .collect();
        (list, r)
    }

    /// The card a packet to `dst` leaves through, or None.
    pub(crate) fn route(&self, dst: Ipv4) -> Option<(Arc<Iface>, Pick)> {
        self.route_on(dst, None)
    }

    /// As `route`, for a socket bound to card `dev` (its position, `ifindex - 2`; WL-4 CR 2).
    pub(crate) fn route_on(&self, dst: Ipv4, dev: Option<usize>) -> Option<(Arc<Iface>, Pick)> {
        let (list, r) = self.routes();
        let p = route::pick_on(&r, dst, dev)?;
        list.get(p.index).cloned().map(|i| (i, p))
    }

    /// Every card with its state (for MCP `net_info` and tests).
    pub fn interfaces(&self) -> Vec<IfSummary> {
        let (list, r) = self.routes();
        let def = route::default_route(&r).map(|p| p.index);
        list.iter().map(|i| IfSummary { name: i.name.clone(), mac: i.mac, info: i.info(), default_route: def == Some(i.index) }).collect()
    }

    /// Every card with what programs see of it (NW-6's interface view), in registration order.
    pub fn interface_details(&self) -> Vec<IfDetail> {
        let list = self.iface_list();
        self.interfaces()
            .into_iter()
            .zip(list.iter())
            .map(|(summary, i)| IfDetail {
                summary,
                mtu: i.dev.mtu().min(u32::MAX as usize) as u32,
                stats: i.dev.stats(),
                carrier_up: i.carrier_up.load(Ordering::Relaxed),
                carrier_down: i.carrier_down.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// Waits until some card has an address (DHCP done), up to `timeout_ns`. Returns that card's name and address.
    pub fn wait_configured(&self, timeout_ns: u64) -> Option<(String, Ipv4)> {
        let found = || self.iface_list().iter().find_map(|i| i.info.lock().addr.map(|(a, _)| (i.name.clone(), a)));
        self.wq.wait_until_timeout(|| found().is_some(), timeout_ns);
        found()
    }

    /// Tests: the next `count` socket buffer allocations fail with `NoMemory` (only on thread `thread` if given,
    /// a `sched::current_id()`); 0 ends the injection.
    pub fn inject_no_memory(&self, count: u32, thread: Option<u64>) {
        self.inject_thread.store(thread.unwrap_or(ANY_THREAD), Ordering::Release);
        self.inject_nomem.store(count, Ordering::Release);
    }

    fn injected(&self) -> bool {
        if self.inject_nomem.load(Ordering::Acquire) == 0 {
            return false;
        }
        let t = self.inject_thread.load(Ordering::Acquire);
        if t != ANY_THREAD && t != sched::current_id() {
            return false;
        }
        let mut c = self.inject_nomem.load(Ordering::Acquire);
        while c > 0 {
            match self.inject_nomem.compare_exchange_weak(c, c - 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) => c = now,
            }
        }
        false
    }

    /// `n` copies of `v` with the memory asked for, never assumed (R2: a socket buffer the heap cannot give is
    /// `NoMemory` to the caller, not a panic). Counted in `net.no_memory`; the caller logs.
    pub(crate) fn buffer<T: Clone>(&self, n: usize, v: T) -> Result<Vec<T>, NetError> {
        let mut b = Vec::new();
        if self.injected() {
            NO_MEMORY_INJECTED.inc();
            return Err(NetError::NoMemory);
        }
        if b.try_reserve_exact(n).is_err() {
            NO_MEMORY.inc();
            return Err(NetError::NoMemory);
        }
        b.resize(n, v);
        Ok(b)
    }

    /// Whether the poll thread may ask the heap for a background socket now (not within the pause after a
    /// refusal); counts the skipped ones.
    pub(crate) fn bg_alloc_open(&self) -> bool {
        if self.now() >= self.bg_next_ns.load(Ordering::Acquire) {
            return true;
        }
        BG_ALLOC_DEFERRED.inc();
        false
    }

    /// The outcome of a background allocation: a refusal doubles the pause (1 ms .. 250 ms), success ends it.
    pub(crate) fn bg_alloc_done(&self, ok: bool) {
        if ok {
            if self.bg_gap_ns.load(Ordering::Relaxed) != 0 {
                self.bg_gap_ns.store(0, Ordering::Relaxed);
                self.bg_next_ns.store(0, Ordering::Release);
            }
            return;
        }
        let gap = (self.bg_gap_ns.load(Ordering::Relaxed) * 2).clamp(BG_RETRY_MIN_NS, BG_RETRY_MAX_NS);
        self.bg_gap_ns.store(gap, Ordering::Relaxed);
        self.bg_next_ns.store(self.now().saturating_add(gap), Ordering::Release);
    }

    /// The first ephemeral port this stack hands out (random per boot).
    pub fn first_ephemeral_port(&self) -> u16 {
        self.first_port
    }

    /// A free ephemeral port (49152-65535, IANA's range; the start is random per boot, `ephemeral_start`),
    /// skipping ports a listener holds and those `taken` names (TCP: a connection on the card with the same
    /// 4-tuple, Linux `__inet_hash_connect`; UDP: a bound port). One pass over the range at most (B4: bounded,
    /// even with every port held): `None` = nothing free, the caller answers `AddrInUse`.
    pub(crate) fn ephemeral_port(&self, taken: impl Fn(u16) -> bool) -> Option<u16> {
        let held: Vec<u16> = self.listeners.lock().iter().map(|l| l.port).collect();
        let mut p = self.next_port.lock();
        for _ in EPHEMERAL {
            let cur = *p;
            *p = if cur >= *EPHEMERAL.end() { *EPHEMERAL.start() } else { cur + 1 };
            if !held.contains(&cur) && !taken(cur) {
                return Some(cur);
            }
        }
        None
    }

    /// While a guard lives the poll thread leaves the cards alone (drivers' own ktests send and receive raw
    /// frames on the same card).
    pub fn hold(&'static self) -> Hold {
        self.hold.fetch_add(1, Ordering::AcqRel);
        Hold(self)
    }

    fn check_default_route(&self) {
        let (list, r) = self.routes();
        let now = route::default_route(&r).map(|p| p.index).unwrap_or(NO_IF);
        let before = self.default_if.swap(now, Ordering::AcqRel);
        if before == now {
            return;
        }
        match (list.get(now), r.iter().find(|x| x.index == now)) {
            (Some(i), Some(x)) => klog::event!(Info, "net", "route.default", ifname = i.name.as_str(),
                via = x.router.map(ip_str).unwrap_or_default(), before = list.get(before).map(|b| b.name.clone()).unwrap_or_default()),
            _ => klog::event!(Warn, "net", "route.default_lost", before = list.get(before).map(|b| b.name.clone()).unwrap_or_default();
                "no card with link and a router: only connected networks are reachable"),
        }
    }

    fn log_note(&self, i: &Iface, n: &Note) {
        let ifname = i.name.as_str();
        match n {
            Note::Link { up: true, speed_mbps } => klog::event!(Info, "net", "iface.link_up", ifname = ifname, speed_mbps = *speed_mbps),
            Note::Link { up: false, .. } => klog::event!(Warn, "net", "iface.link_down", ifname = ifname; "address kept; DHCP asks again when the link is back"),
            Note::Lease(l) => klog::event!(Info, "net", "dhcp.lease", ifname = ifname, ip = ip_str(l.addr), prefix = l.prefix,
                router = l.router.map(ip_str).unwrap_or_default(), dns = l.dns.iter().map(|d| ip_str(*d)).collect::<Vec<_>>().join(","),
                server = ip_str(l.server), lease_s = l.lease_ns.map(|n| n / 1_000_000_000).unwrap_or(u64::MAX)),
            Note::Renewed(l) => klog::event!(Info, "net", "dhcp.renewed", ifname = ifname, ip = ip_str(l.addr),
                lease_s = l.lease_ns.map(|n| n / 1_000_000_000).unwrap_or(u64::MAX)),
            Note::Lost => klog::event!(Warn, "net", "dhcp.lost", ifname = ifname; "lease expired or refused (NAK); asking again"),
            Note::Restart { keep } => klog::event!(Info, "net", "dhcp.restart", ifname = ifname, keep_ip = keep.map(ip_str).unwrap_or_default();
                "link is back: asking again"),
            Note::Discover { attempt, next_ns } => {
                if *attempt == dhcp::FAST_TRIES + 1 {
                    klog::event!(Warn, "net", "dhcp.slow", ifname = ifname, attempt = *attempt, next_s = next_ns / 1_000_000_000;
                        "no answer to 7 DISCOVERs; asking less often, never giving up");
                } else if *attempt > 1 {
                    klog::event!(Info, "net", "dhcp.retry", ifname = ifname, attempt = *attempt, next_s = next_ns / 1_000_000_000);
                } else {
                    klog::event!(Debug, "net", "dhcp.discover", ifname = ifname);
                }
            }
            Note::TxDropped(n) => {
                TX_DROPPED.add(*n as u64);
                klog::event!(Debug, "net", "tx.dropped", ifname = ifname, frames = *n);
            }
            Note::ListenShort(true) => klog::event!(Warn, "net", "listen.no_memory", ifname = ifname;
                "no memory for more listening sockets: a SYN beyond the ones left gets RST; retried after a growing pause (up to 250 ms)"),
            Note::ListenShort(false) => klog::event!(Info, "net", "listen.memory_back", ifname = ifname),
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // ICMP echo

    /// Sends one echo request to `dst` and waits for the answer. Returns the round trip in nanoseconds.
    pub fn ping(&'static self, dst: Ipv4, timeout_ns: u64) -> Result<u64, NetError> {
        udp::ping(self, dst, timeout_ns)
    }
}

/// The poll thread's view: copies of the card and listener lists, refreshed only when they change, so a
/// quiet round allocates nothing (the G0 heap never reuses memory).
pub struct Poller {
    stack: &'static Stack,
    ifaces: Vec<Arc<Iface>>,
    listeners: Vec<Arc<ListenShared>>,
    listeners_gen: u64,
    /// The previous round's earliest smoltcp deadline: a round at or after it ran timers.
    last_next: Option<u64>,
}

/// What one round found.
pub struct Round {
    /// smoltcp's earliest deadline over all cards.
    pub next_ns: Option<u64>,
    /// The frame budget ran out on a card: more is waiting.
    pub busy: bool,
    /// Socket waiters were woken: traffic, a link or address change, or a smoltcp timer that ran (a stall
    /// timeout closes a socket without sending anything when the peer has no ARP answer or the link is down).
    pub woke: bool,
}

impl Poller {
    pub fn new(stack: &'static Stack) -> Self {
        Poller { stack, ifaces: Vec::new(), listeners: Vec::new(), listeners_gen: u64::MAX, last_next: None }
    }

    /// One poll round over every card.
    pub fn round(&mut self) -> Round {
        let st = self.stack;
        let now = st.now();
        if st.hold.load(Ordering::Acquire) > 0 {
            return Round { next_ns: None, busy: false, woke: false };
        }
        POLL_ROUNDS.inc();
        st.sync_devices();
        let mut new_card = false;
        {
            let l = st.ifaces.lock();
            if l.len() != self.ifaces.len() {
                self.ifaces.clone_from(&l);
                new_card = true;
            }
        }
        let generation = st.listeners_gen.load(Ordering::Acquire);
        if generation != self.listeners_gen {
            self.listeners_gen = generation;
            self.listeners.clone_from(&st.listeners.lock());
        }
        let (mut any, mut busy, mut routing, mut noted) = (false, false, false, false);
        let timers_ran = self.last_next.is_some_and(|d| now >= d);
        let mut next: Option<u64> = None;
        for i in &self.ifaces {
            let rep = i.poll(st, now, &self.listeners);
            any |= rep.activity;
            busy |= rep.busy;
            next = match (next, rep.next_ns) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            for n in &rep.notes {
                routing |= !matches!(n, Note::TxDropped(_) | Note::Discover { .. } | Note::ListenShort(_));
                noted |= !matches!(n, Note::TxDropped(_) | Note::Discover { .. } | Note::Renewed(_) | Note::ListenShort(_));
                st.log_note(i, n);
            }
            for (q, s) in rep.accepted {
                TCP_ACCEPTS.inc();
                let _ = q.push(s);
            }
        }
        // NW-7: the connections smoltcp woke in this round, now that no card's lock is held. A program's
        // connection among them wakes the program sockets' queue too.
        let prog_woke = st.wakes.fire_all();
        // The MCP reserve's spares, after the listeners took what they needed this round.
        st.top_up_kept(true);
        if busy {
            RX_BUSY_ROUNDS.inc();
        }
        if routing {
            st.check_default_route();
        }
        // Programs watching rtnetlink groups hear about links, addresses and new cards (NW-6; a load when
        // nobody listens).
        if routing || new_card {
            crate::sock::netlink::changed();
        }
        if any {
            st.last_active_ns.store(now, Ordering::Relaxed);
        }
        // B4: a socket that a timer closed (smoltcp's stall timeout) or a link change wakes its waiter even when
        // nothing was sent or received; `wake_sockets` costs nothing without waiters.
        let woke = any || noted || timers_ran;
        if woke {
            st.wake_kernel_waiters();
        }
        // Program sockets (one queue for all, `inet::READY`) hear about their own connections' changes and about
        // links and addresses; a round that moved only kernel connections (MCP, downloads) leaves them asleep.
        // Their waits look again every slice anyway (B4).
        if prog_woke || noted {
            crate::sock::inet::notify();
        }
        self.last_next = next;
        Round { next_ns: next, busy, woke }
    }
}

pub struct Hold(&'static Stack);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.hold.fetch_sub(1, Ordering::AcqRel);
        self.0.kick();
    }
}

pub fn ip_str(a: Ipv4) -> String {
    alloc::format!("{}.{}.{}.{}", a.0[0], a.0[1], a.0[2], a.0[3])
}

#[cfg(test)]
mod tests;
