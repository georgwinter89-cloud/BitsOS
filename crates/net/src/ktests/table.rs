//! A card's socket table on the kernel's own heap (`stack::slots`): it grows past its first slots with a
//! listener's whole backlog at once, with connections one by one and with UDP sockets, and takes a burst of
//! orphans, all without a large allocation under the card's lock. The run's `heap.atomic_large` events are the
//! proof (before, `SocketSet::add` under `net.iface` was among them); the test
//! itself checks that every connection is still its own after the moves.

use super::hardening::Cable;
use super::{e, SEC};
use crate::stack::slots::FIRST_SLOTS;
use crate::{Ipv4, KSocket, SockAddr, TcpStream, UdpSocket};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};

const TABLE_PORT: u16 = 24970;
const BACKLOG: usize = 40;

/// Waits (bounded) until `f` holds.
fn until(what: &str, mut f: impl FnMut() -> bool) -> TestResult {
    let end = time::now_ns() + 10 * SEC;
    while !f() {
        kassert!(time::now_ns() < end, "never happened: {what}");
        sched::sleep_ns(5_000_000);
    }
    Ok(())
}

fn table_grows() -> TestResult {
    let cab = Cable::new(3)?;
    let (a, b) = (cab.a, cab.b);
    let r = (|| -> TestResult {
        let (ia, ib) = (Ipv4([10, 79, 0, 1]), Ipv4([10, 79, 0, 2]));
        cab.addrs(ia, ib, 24)?;
        // (slots, sockets, room of the listener list, room of the orphan list) of the one card.
        let table = |s: &'static crate::stack::Stack| s.tables()[0];
        let sound = |t: (usize, usize, usize, usize)| t.1 <= t.0 && t.2 >= t.0 && t.3 >= t.0;
        kassert!(table(b).0 == FIRST_SLOTS, "a new card's table: {:?}", table(b));
        let ks: &dyn KSocket = b;
        let l = ks.tcp_listen(SockAddr { ip: Ipv4::ANY, port: TABLE_PORT }, BACKLOG).map_err(e)?;
        let t = table(b);
        kassert!(t.1 >= BACKLOG + 2 && sound(t), "the backlog's sockets at once: {t:?}");
        let mut cs: Vec<Box<dyn TcpStream>> = Vec::new();
        for i in 0..BACKLOG {
            cs.push(a.tcp_connect(SockAddr { ip: ib, port: TABLE_PORT }, Some(10 * SEC)).map_err(|x| format!("connect {i}: {x:?}"))?);
        }
        let t = table(a);
        kassert!(t.0 >= 2 * FIRST_SLOTS && t.1 == BACKLOG && sound(t), "one connection at a time: {t:?}");
        let mut ss: Vec<Box<dyn TcpStream>> = Vec::new();
        for i in 0..BACKLOG {
            ss.push(l.accept(Some(10 * SEC)).map_err(|x| format!("accept {i}: {x:?}"))?);
        }
        // Each side names the client's port: a socket that changed its handle would answer for another one.
        for c in &cs {
            c.write_all(&c.local().port.to_be_bytes(), Some(5 * SEC)).map_err(e)?;
        }
        let mut buf = [0u8; 2];
        for s in &ss {
            let mut got = 0;
            while got < 2 {
                got += s.read(&mut buf[got..], Some(5 * SEC)).map_err(e)?;
            }
            kassert!(buf == s.peer().port.to_be_bytes(), "a server stream got another connection's bytes");
            s.write_all(&s.peer().port.to_le_bytes(), Some(5 * SEC)).map_err(e)?;
        }
        for c in &cs {
            let mut got = 0;
            while got < 2 {
                got += c.read(&mut buf[got..], Some(5 * SEC)).map_err(e)?;
            }
            kassert!(buf == c.local().port.to_le_bytes(), "a client stream got another connection's bytes");
        }
        // UDP sockets take slots the same way.
        let mut us: Vec<Box<dyn UdpSocket>> = Vec::new();
        for i in 0..BACKLOG as u16 {
            us.push(a.udp_bind(SockAddr { ip: Ipv4::ANY, port: TABLE_PORT + 100 + i }).map_err(e)?);
        }
        let t = table(a);
        kassert!(t.1 == 2 * BACKLOG && sound(t), "{t:?}");
        // Everything closes at once: an orphan per connection on both sides, in lists that have their room.
        drop(us);
        drop(cs);
        drop(ss);
        drop(l);
        until("every socket gone on both cards", || table(a).1 == 0 && table(b).1 == 0)?;
        klog::event!(Info, "net", "ktest.table", slots_a = table(a).0, slots_b = table(b).0);
        Ok(())
    })();
    kassert!(cab.end(), "cable poller did not end");
    r
}
ktest!("nw.stack.table_grows", Safe, table_grows);
