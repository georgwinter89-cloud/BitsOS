//! A card's socket table (`slots.rs`): it grows with the connections, with no lock held, and every socket keeps
//! its handle; a table that cannot grow refuses programs, not the MCP reserve. A socket added without a free
//! slot, or a list beside the table that would grow under the card's lock, ends these tests (debug assertions
//! in `Sockets::add`, `Iface::orphan`, `refill`).

use super::*;
use crate::stack::slots::FIRST_SLOTS;
use crate::UdpSocket;

/// The table has a slot for every socket and the lists beside it a place for every slot.
fn check(card: &Iface) -> (usize, usize) {
    let (slots, used, listen, orphans) = card.table_info();
    assert!(used <= slots && listen >= slots && orphans >= slots, "slots {slots} used {used} listen {listen} orphans {orphans}");
    (slots, used)
}

#[test]
fn the_table_grows_with_listeners_connections_and_orphans() {
    let n = net();
    let (ca, cb) = (n.a.ifaces.lock()[0].clone(), n.b.ifaces.lock()[0].clone());
    assert_eq!(check(&cb).0, FIRST_SLOTS);
    // A kernel listener makes its whole backlog at once: more sockets than the table had slots.
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7400), 40).unwrap();
    let (slots, used) = check(&cb);
    assert!(used >= 42 && slots >= used, "{used} of {slots}");
    // One connection at a time on the other side: its table doubles on the way.
    let cs: Vec<Box<dyn TcpStream>> = (0..40).map(|_| n.a.tcp_connect(at(B, 7400), T).unwrap()).collect();
    assert!(check(&ca).0 >= 64);
    let ss: Vec<Box<dyn TcpStream>> = (0..40).map(|_| l.accept(T).unwrap()).collect();
    // Every stream is still its own connection after the moves: each says its local port, the far side knows it
    // as the peer's.
    for c in &cs {
        c.write_all(&c.local().port.to_be_bytes(), T).unwrap();
    }
    for s in &ss {
        assert_eq!(read_all(&**s, 2), s.peer().port.to_be_bytes());
        s.write_all(&s.peer().port.to_le_bytes(), T).unwrap();
    }
    for c in &cs {
        assert_eq!(read_all(&**c, 2), c.local().port.to_le_bytes());
    }
    eventually("listening sockets refilled", || check(&cb).1 >= 80);
    // All 40 close at once: 40 orphans on each side, in lists that have their room.
    drop(cs);
    drop(ss);
    eventually("orphans gone", || ca.orphan_count() == 0 && cb.orphan_count() == 0);
    assert_eq!(check(&ca).1, 0);
    drop(l);
    eventually("listener's sockets gone", || check(&cb).1 == 0);
    // The freed slots are used again: no larger table for the next connections.
    let slots = check(&ca).0;
    let l = n.b.tcp_listen(at(Ipv4::ANY, 7401), 4).unwrap();
    let c = n.a.tcp_connect(at(B, 7401), T).unwrap();
    let s = l.accept(T).unwrap();
    c.write_all(b"again", T).unwrap();
    assert_eq!(read_all(&*s, 5), b"again");
    assert_eq!(check(&ca).0, slots);
}

/// V2-PLAN §6 for the table: programs fill it, memory for a larger one is refused, and MCP still gets its three
/// connections with their listening sockets refilled, out of the slots that were held back for the kept pool.
#[test]
fn a_table_that_cannot_grow_refuses_programs_not_the_reserve() {
    let n = net();
    let mcp = n.b.tcp_listen_private(at(Ipv4::ANY, 7410), 3).unwrap();
    let shared = n.b.listeners.lock().iter().find(|x| x.port == 7410).cloned().unwrap();
    let card = n.b.ifaces.lock()[0].clone();
    let want = iface::listen_want(&shared);
    eventually("spares made", || n.b.kept_info().idle == iface::RESERVE_MIN);
    let pool_room = n.b.kept.lock().capacity();
    assert!(pool_room >= n.b.kept_info().cap, "the pool has its room before a socket comes back: {pool_room}");
    // Programs take every slot they may.
    let mut udp: Vec<Box<dyn UdpSocket>> = Vec::new();
    while card.inner.lock().admits(n.b, 0, 1) {
        udp.push(n.b.udp_bind(at(Ipv4::ANY, 7500 + udp.len() as u16)).unwrap());
    }
    let (slots, used) = check(&card);
    assert_eq!(slots, FIRST_SLOTS, "no larger table yet");
    assert_eq!(slots - used, iface::RESERVE_MIN, "held back: what the pool's spares may still need");
    n.b.inject_no_memory(u32::MAX, None);
    {
        let mut room = iface::Room::new();
        let (g, ok) = card.lock_room(n.b, &mut room, 0, 1, false);
        assert!(!ok, "a program's socket fits neither the table nor a larger one");
        drop(g);
        let (g, ok) = card.lock_room(n.b, &mut room, 1, 0, false);
        assert!(ok, "a kept socket takes a held slot");
        drop(g);
    }
    assert_eq!(n.b.udp_bind(at(Ipv4::ANY, 7499)).err(), Some(NetError::NoMemory));
    assert_eq!(n.b.tcp_connect(at(A, 9), T).err(), Some(NetError::NoMemory));
    for round in 0..3 {
        let returned = n.b.kept_info().returned;
        let cs: Vec<Box<dyn TcpStream>> = (0..3).map(|_| n.a.tcp_connect(at(B, 7410), T).expect("reserve takes it")).collect();
        let ss: Vec<Box<dyn TcpStream>> = (0..3).map(|_| mcp.accept(T).unwrap()).collect();
        eventually("listening sockets refilled into the held slots", || card.listen_count(&shared) == want);
        assert_eq!(check(&card), (FIRST_SLOTS, FIRST_SLOTS), "round {round}: the table is full and did not grow");
        for (c, s) in cs.iter().zip(&ss) {
            c.write_all(b"log", T).unwrap();
            assert_eq!(read_all(&**s, 3), b"log");
        }
        drop(ss);
        drop(cs);
        eventually("kept sockets back as spares", || {
            n.b.kept_info().returned >= returned + 3 && card.listen_count(&shared) == want && card.orphan_count() == 0
        });
    }
    assert_eq!(n.b.kept.lock().capacity(), pool_room, "the pool never grew under its lock");
    // Memory is back: the next program socket gets a larger table, and every socket is still itself.
    n.b.inject_no_memory(0, None);
    udp.push(n.b.udp_bind(at(Ipv4::ANY, 7500 + udp.len() as u16)).unwrap());
    assert_eq!(check(&card).0, 2 * FIRST_SLOTS);
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    for i in 0..udp.len() as u16 {
        ua.send_to(&i.to_be_bytes(), at(B, 7500 + i)).unwrap();
    }
    let mut buf = [0u8; 8];
    for (i, u) in udp.iter().enumerate() {
        let (len, _) = u.recv_from(&mut buf, T).unwrap();
        assert_eq!(buf[..len], (i as u16).to_be_bytes());
    }
    let c = n.a.tcp_connect(at(B, 7410), T).unwrap();
    let s = mcp.accept(T).unwrap();
    s.write_all(b"still here", T).unwrap();
    assert_eq!(read_all(&*c, 10), b"still here");
}

/// Sockets come from several threads while the table doubles under them: a slot made with the lock dropped may
/// be gone when the thread is back (`SmolUdp::sync` looks again under the lock). No add without a slot, and
/// every socket ends up on its own port.
#[test]
fn sockets_from_many_threads_while_the_table_grows() {
    let n = net();
    let card = n.b.ifaces.lock()[0].clone();
    const THREADS: u16 = 6;
    const EACH: u16 = 25;
    let makers: Vec<thread::JoinHandle<Vec<(u16, Box<dyn UdpSocket>)>>> = (0..THREADS)
        .map(|t| {
            let b = n.b;
            thread::spawn(move || (0..EACH).map(|i| (8000 + t * 100 + i, b.udp_bind(at(Ipv4::ANY, 8000 + t * 100 + i)).unwrap())).collect())
        })
        .collect();
    let socks: Vec<(u16, Box<dyn UdpSocket>)> = makers.into_iter().flat_map(|m| m.join().unwrap()).collect();
    let (slots, used) = check(&card);
    assert_eq!(used, (THREADS * EACH) as usize);
    assert!(slots >= used && slots <= 2 * used.next_power_of_two(), "{slots} slots for {used} sockets");
    let ua = n.a.udp_bind(at(Ipv4::ANY, 0)).unwrap();
    let mut buf = [0u8; 8];
    for (port, u) in &socks {
        ua.send_to(&port.to_be_bytes(), at(B, *port)).unwrap();
        let (len, _) = u.recv_from(&mut buf, T).unwrap();
        assert_eq!(buf[..len], port.to_be_bytes());
    }
    drop(socks);
    assert_eq!(check(&card).1, 0);
}
