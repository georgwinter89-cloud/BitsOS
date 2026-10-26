//! NW-3b ktests: the random ephemeral start (RFC 6056), memory refusals as `NoMemory` (R2), and the
//! private-only listener through `KSocket` (spec §2.4) on two stacks joined by an in-kernel cable.

use super::{configured, e, st, SEC};
use crate::stack::{self, ephemeral_start, Stack, EPHEMERAL};
use crate::{Ipv4, KSocket, NetError, SockAddr};
use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use devmodel::net::{LinkState, MacAddr, NetDevice};
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};

/// Ports of these tests (lane range 24500-24999).
const LISTEN_PORT: u16 = 24980;
const CABLE_PORT: u16 = 24981;
const RESERVE_PORT: u16 = 24984;

/// The kernel stack's ephemeral counter starts inside the range, and the seed it comes from moves (the TSC):
/// sixteen seeds taken now give many different starts, so a reloaded kernel (clock at 0 again) does not start
/// where the previous one did. The Reload itself is checked on the Mini-PC (notes check list).
fn ephemeral_random() -> TestResult {
    let s = st()?;
    let first = s.first_ephemeral_port();
    kassert!(EPHEMERAL.contains(&first), "first port {first} outside 49152-65535");
    let starts: BTreeSet<u16> = (0..16)
        .map(|_| {
            sched::yield_now();
            ephemeral_start(stack::seed())
        })
        .collect();
    klog::event!(Info, "net", "ktest.ephemeral", first = first, distinct = starts.len());
    kassert!(starts.len() >= 8, "16 seeds gave only {} different starts: {starts:?}", starts.len());
    // A stack hands out the port its seed chose: two stacks (no cards) seeded like two boots start apart.
    let sa = stack::seed();
    // (1 in 16,384 seeds shares a start; the next differing one is taken so the check never flickers.)
    let sb = (1..64u64).map(|k| sa ^ k).find(|x| ephemeral_start(*x) != ephemeral_start(sa)).ok_or("64 seeds, one start")?;
    let (a, b) = (Stack::new_static(time::now_ns, sa, false), Stack::new_static(time::now_ns, sb, false));
    kassert!(a.first_ephemeral_port() == ephemeral_start(sa) && b.first_ephemeral_port() == ephemeral_start(sb), "first port from the seed");
    kassert!(a.first_ephemeral_port() != b.first_ephemeral_port(), "two seeds, one start {}", a.first_ephemeral_port());
    kassert!(a.ephemeral_port(|_| false) == Some(a.first_ephemeral_port()), "the first port handed out is the seed's");
    kassert!(a.ephemeral_port(|_| true).is_none(), "every port taken: bounded, None");
    Ok(())
}
ktest!("nw.stack.ephemeral_random", Safe, ephemeral_random);

/// R2 on the kernel's stack, with the failure injected on this thread only (MCP and the poll thread are not
/// touched): connect, listen and UDP bind answer `NoMemory`, nothing is left behind, and each works again once
/// memory is back. A 1 TiB receive window is refused by the real heap (no injection).
fn no_memory() -> TestResult {
    let s = st()?;
    if crate::netdev::devices().is_empty() {
        // Without a card no socket buffer is asked for (loopback only): nothing to refuse.
        return ktest::skip("no network card (--net none)");
    }
    let me = Some(sched::current_id());
    let (before, injected) = (stack::NO_MEMORY.get(), stack::NO_MEMORY_INJECTED.get());
    let r = (|| -> TestResult {
        // Listen: the poll thread is held so it cannot fill the listener between registration and refill.
        {
            let _hold = s.hold();
            s.inject_no_memory(1000, me);
            let l = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: LISTEN_PORT }, 2);
            kassert!(l.as_ref().err() == Some(&NetError::NoMemory), "listen without memory: {:?}", l.as_ref().err());
            drop(l);
            s.inject_no_memory(0, None);
        }
        let l = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: LISTEN_PORT }, 2).map_err(|x| format!("listen after the refusal: {x:?}"))?;
        drop(l);
        // UDP bind.
        s.inject_no_memory(1, me);
        let u = s.udp_bind(SockAddr { ip: Ipv4::ANY, port: LISTEN_PORT });
        kassert!(u.as_ref().err() == Some(&NetError::NoMemory), "udp bind: {:?}", u.as_ref().err());
        drop(u);
        s.inject_no_memory(0, None);
        drop(s.udp_bind(SockAddr { ip: Ipv4::ANY, port: LISTEN_PORT }).map_err(|x| format!("udp bind after: {x:?}"))?);
        // Connect needs a route: the leased card's router (nothing is sent: the refusal comes first).
        let Some(i) = configured(s)? else {
            klog::event!(Warn, "net", "ktest.no_memory_partial"; "no lease: connect refusal not checked");
            return Ok(());
        };
        let gw = i.info.router.ok_or("no router in the lease")?;
        let to = SockAddr { ip: gw, port: LISTEN_PORT };
        s.inject_no_memory(1, me);
        let c = s.tcp_connect(to, Some(SEC));
        kassert!(c.as_ref().err() == Some(&NetError::NoMemory), "connect: {:?}", c.as_ref().err());
        let big = s.tcp_connect_sized(to, Some(SEC), 1 << 40, 4096);
        kassert!(big.as_ref().err() == Some(&NetError::NoMemory), "1 TiB window: {:?}", big.as_ref().err());
        Ok(())
    })();
    s.inject_no_memory(0, None);
    r?;
    // NW-7: injected refusals have their own counter; `net.no_memory` grows only by what the heap refused (the
    // 1 TiB window: +1 per run, when a lease was there).
    let (n, inj) = (stack::NO_MEMORY.get() - before, stack::NO_MEMORY_INJECTED.get() - injected);
    klog::event!(Info, "net", "ktest.no_memory", refusals = n, injected = inj);
    kassert!(inj >= 2, "net.no_memory_injected not counted ({inj})");
    Ok(())
}
ktest!("nw.stack.no_memory", Safe, no_memory);

// ---------------------------------------------------------------------------------------------------------
// An in-kernel cable between two stacks of their own (no real card involved).

type Frames = Arc<SpinLock<VecDeque<Vec<u8>>>>;

struct End {
    name: String,
    mac: [u8; 6],
    rx: Frames,
    tx: Frames,
}

impl NetDevice for End {
    fn name(&self) -> &str {
        &self.name
    }
    fn mac(&self) -> MacAddr {
        MacAddr(self.mac)
    }
    fn link(&self) -> LinkState {
        LinkState { up: true, speed_mbps: 1000, full_duplex: true }
    }
    fn transmit(&self, frame: &[u8]) -> Result<(), devmodel::DevError> {
        let mut q = self.tx.lock();
        // A bound against a stuck partner (frames are dropped like on a full ring; TCP resends).
        if q.len() < 256 {
            q.push_back(frame.to_vec());
        }
        Ok(())
    }
    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        let f = self.rx.lock().pop_front()?;
        let n = f.len().min(buf.len());
        buf[..n].copy_from_slice(&f[..n]);
        Some(n)
    }
    fn poll_tx(&self, frame: &[u8]) -> bool {
        self.transmit(frame).is_ok()
    }
}

/// Two stacks of their own joined by an in-kernel cable, polled by a thread of the test (no real card involved).
struct Cable {
    a: &'static Stack,
    b: &'static Stack,
    stop: Arc<AtomicBool>,
    poller: Option<sched::JoinHandle>,
}

impl Cable {
    fn new(tag: u8) -> Result<Cable, String> {
        let (ab, ba): (Frames, Frames) = (Arc::new(SpinLock::new(VecDeque::new())), Arc::new(SpinLock::new(VecDeque::new())));
        let ea = End { name: "ktcable-a".into(), mac: [2, 0, tag, 0x6b, 0, 0xa], rx: ba.clone(), tx: ab.clone() };
        let eb = End { name: "ktcable-b".into(), mac: [2, 0, tag, 0x6b, 0, 0xb], rx: ab, tx: ba };
        let a = Stack::new_static(time::now_ns, stack::seed(), false);
        let b = Stack::new_static(time::now_ns, stack::seed(), false);
        a.add_device(Arc::new(ea), false);
        b.add_device(Arc::new(eb), false);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let poller = sched::spawn("ktest-cable", sched::Class::Normal, move || {
            let (mut pa, mut pb) = (stack::Poller::new(a), stack::Poller::new(b));
            while !flag.load(Ordering::Acquire) {
                pa.round();
                pb.round();
                sched::sleep_ns(50_000);
            }
        })
        .map_err(e)?;
        Ok(Cable { a, b, stop, poller: Some(poller) })
    }

    fn addrs(&self, a: Ipv4, b: Ipv4, prefix: u8) -> TestResult {
        self.a.set_static("ktcable-a", a, prefix, None).map_err(e)?;
        self.b.set_static("ktcable-b", b, prefix, None).map_err(e)
    }

    /// Stops the poller (bounded); false if it did not end.
    fn end(mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.poller.take().is_none_or(|p| p.join_timeout(5 * SEC))
    }
}

impl Drop for Cable {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Spec §2.4 / B8 through the contract: `KSocket::tcp_listen_private` on the stack refuses a peer from a public
/// address with RST (no connection reaches the accept queue, `net.tcp_refused_public` counts it), and since NW-4
/// so does a plain `KSocket::tcp_listen` (every BitsOS listener is private); only the stack's own
/// `tcp_listen_public` (tests) takes that peer. The private listener takes a private one.
fn listen_private() -> TestResult {
    let cab = Cable::new(1)?;
    let (a, b) = (cab.a, cab.b);
    let r = (|| -> TestResult {
        let (pub_a, pub_b) = (Ipv4([198, 51, 100, 1]), Ipv4([198, 51, 100, 2]));
        cab.addrs(pub_a, pub_b, 24)?;
        let ks: &dyn KSocket = b;
        let private = ks.tcp_listen_private(SockAddr { ip: Ipv4::ANY, port: CABLE_PORT }, 2).map_err(e)?;
        let plain = ks.tcp_listen(SockAddr { ip: Ipv4::ANY, port: CABLE_PORT + 2 }, 2).map_err(e)?;
        let open = b.tcp_listen_public(SockAddr { ip: Ipv4::ANY, port: CABLE_PORT + 1 }, 2).map_err(e)?;
        let refused0 = stack::TCP_REFUSED_PUBLIC.get();
        let c = a.tcp_connect(SockAddr { ip: pub_b, port: CABLE_PORT }, Some(10 * SEC));
        kassert!(c.as_ref().err() == Some(&NetError::ConnectionRefused), "public peer to a private listener: {:?}", c.as_ref().err().map(|_| ()));
        kassert!(stack::TCP_REFUSED_PUBLIC.get() > refused0, "net.tcp_refused_public not counted");
        kassert!(private.accept(Some(SEC / 10)).is_err(), "nothing reached the accept queue");
        let c = a.tcp_connect(SockAddr { ip: pub_b, port: CABLE_PORT + 2 }, Some(10 * SEC));
        kassert!(c.as_ref().err() == Some(&NetError::ConnectionRefused), "public peer to a plain KSocket listener: {:?}", c.as_ref().err().map(|_| ()));
        kassert!(plain.accept(Some(SEC / 10)).is_err(), "nothing reached the plain listener");
        let c = a.tcp_connect(SockAddr { ip: pub_b, port: CABLE_PORT + 1 }, Some(10 * SEC)).map_err(|x| format!("public listener: {x:?}"))?;
        let s = open.accept(Some(5 * SEC)).map_err(e)?;
        c.write_all(b"pub", Some(5 * SEC)).map_err(e)?;
        let mut buf = [0u8; 8];
        kassert!(s.read(&mut buf, Some(5 * SEC)) == Ok(3), "data over the public listener");
        drop((c, s));
        // The same private listener takes a peer from a private network.
        let (prv_a, prv_b) = (Ipv4([10, 77, 0, 1]), Ipv4([10, 77, 0, 2]));
        cab.addrs(prv_a, prv_b, 24)?;
        let c = a.tcp_connect(SockAddr { ip: prv_b, port: CABLE_PORT }, Some(10 * SEC)).map_err(|x| format!("private peer: {x:?}"))?;
        let s = private.accept(Some(5 * SEC)).map_err(e)?;
        kassert!(s.peer().ip == prv_a, "peer {:?}", s.peer());
        c.write_all(b"prv", Some(5 * SEC)).map_err(e)?;
        kassert!(s.read(&mut buf, Some(5 * SEC)) == Ok(3), "data over the private listener");
        drop((c, s));
        // Link-local is not private for this listener (RFC 1918 only, V1 `tcp_private`).
        let (ll_a, ll_b) = (Ipv4([169, 254, 77, 1]), Ipv4([169, 254, 77, 2]));
        cab.addrs(ll_a, ll_b, 16)?;
        let refused1 = stack::TCP_REFUSED_PUBLIC.get();
        let c = a.tcp_connect(SockAddr { ip: ll_b, port: CABLE_PORT }, Some(10 * SEC));
        kassert!(c.as_ref().err() == Some(&NetError::ConnectionRefused), "link-local peer: {:?}", c.as_ref().err().map(|_| ()));
        kassert!(stack::TCP_REFUSED_PUBLIC.get() > refused1, "link-local refusal not counted");
        Ok(())
    })();
    kassert!(cab.end(), "cable poller did not end");
    r
}
ktest!("nw.stack.listen_private", Safe, listen_private);

/// The MCP reserve (NW-4, V2-PLAN §6) on a cable stack of its own: with every socket buffer of stack B refused
/// on every thread (memory gone), its reserved listener accepts 3 connections side by side and serves them in
/// four rounds, because its kept sockets come back after each connection; a plain listener made meanwhile gets
/// `NoMemory`. The pool stays bounded and is freed with the listener. Rerunnable (own stacks, own ports).
fn reserve() -> TestResult {
    let cab = Cable::new(2)?;
    let (a, b) = (cab.a, cab.b);
    let r = (|| -> TestResult {
        let (ia, ib) = (Ipv4([10, 78, 0, 1]), Ipv4([10, 78, 0, 2]));
        cab.addrs(ia, ib, 24)?;
        let ks: &dyn KSocket = b;
        let mcp = ks.tcp_listen_private(SockAddr { ip: Ipv4::ANY, port: RESERVE_PORT }, 3).map_err(e)?;
        // Listening sockets and the idle spares exist before memory goes (the poll thread may still be making
        // the last spare when `tcp_listen_private` returns).
        let end = time::now_ns() + 5 * SEC;
        while b.kept_info().idle < b.kept_info().spare && time::now_ns() < end {
            sched::sleep_ns(10_000_000);
        }
        let k0 = b.kept_info();
        let made = k0.made;
        kassert!(k0.spare >= crate::stack::iface::RESERVE_MIN && k0.idle >= k0.spare, "spares not made: {k0:?}");
        kassert!(made >= 2 * crate::stack::iface::RESERVE_MIN, "pre-created {made}");
        b.inject_no_memory(u32::MAX, None);
        let res = (|| -> TestResult {
            let plain = ks.tcp_listen(SockAddr { ip: Ipv4::ANY, port: RESERVE_PORT + 1 }, 2);
            kassert!(plain.as_ref().err() == Some(&NetError::NoMemory), "plain listener without memory: {:?}", plain.as_ref().err());
            for round in 0..4 {
                let k_round = b.kept_info();
                let mut pairs = Vec::new();
                for _ in 0..3 {
                    let c = a.tcp_connect(SockAddr { ip: ib, port: RESERVE_PORT }, Some(10 * SEC)).map_err(|x| format!("round {round}: connect {x:?}"))?;
                    pairs.push(c);
                }
                let mut srv = Vec::new();
                for _ in 0..3 {
                    srv.push(mcp.accept(Some(10 * SEC)).map_err(|x| format!("round {round}: accept {x:?}"))?);
                }
                // Refilled after accept without any heap: the 3 spares went to listening while the connections
                // are still open (taken from the pool, nothing new made).
                let end = time::now_ns() + 10 * SEC;
                while b.kept_info().reused < k_round.reused + 3 && time::now_ns() < end {
                    sched::sleep_ns(10_000_000);
                }
                let k = b.kept_info();
                kassert!(k.reused >= k_round.reused + 3 && k.made == k_round.made, "round {round}: not refilled from the spares: {k:?}");
                for (c, s) in pairs.iter().zip(&srv) {
                    c.write_all(b"log", Some(5 * SEC)).map_err(e)?;
                    let mut buf = [0u8; 8];
                    kassert!(s.read(&mut buf, Some(5 * SEC)) == Ok(3), "round {round}: request");
                    s.write_all(b"answer", Some(5 * SEC)).map_err(e)?;
                    let mut got = 0;
                    while got < 6 {
                        got += c.read(&mut buf[got..], Some(5 * SEC)).map_err(e)?;
                    }
                }
                let returned = b.kept_info().returned;
                drop(srv);
                drop(pairs);
                // The kept sockets come back once the connections have closed (FIN both ways): spares again.
                let end = time::now_ns() + 10 * SEC;
                while (b.kept_info().returned < returned + 3 || b.kept_info().idle < k_round.idle) && time::now_ns() < end {
                    sched::sleep_ns(10_000_000);
                }
                let k = b.kept_info();
                kassert!(k.returned >= returned + 3 && k.idle >= k_round.idle, "round {round}: the 3 kept sockets did not come back within 10 s: {k:?}");
            }
            Ok(())
        })();
        b.inject_no_memory(0, None);
        res?;
        let k = b.kept_info();
        klog::event!(Info, "net", "ktest.reserve", made = k.made, idle = k.idle, cap = k.cap, spare = k.spare, returned = k.returned);
        kassert!(k.made == made, "the pool grew past what it serves (memory was refused all along): {k:?}");
        drop(mcp);
        kassert!(b.kept_info().idle == 0, "kept sockets freed with the listener: {:?}", b.kept_info());
        Ok(())
    })();
    kassert!(cab.end(), "cable poller did not end");
    r
}
ktest!("nw.stack.reserve", Safe, reserve);
