//! ATOMIC-3: a UDP socket's new peer opens its entry in the connection view under `policy.udp`, and that open may
//! double the view's live list (8 bytes per connection: 8 KiB and more past 512). The lock was a spin lock, where
//! the heap may not reclaim; now it is a sleeping one. The test brings the live list to the edge where the next
//! open grows it to two pages or more, lets a new peer's datagram do that open, and judges by the detector's table
//! (`mm.heap.atomic_large`): nothing may be noted meanwhile.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{kassert, ktest, TestResult};
use policy::{IpAddr, NetProto, ProgramId, Subject, UdpPeers};

/// Pids no real process has (pid_max is at most 2^22), apart from policy's own tests (0x7f00_0000 up).
static RUN: AtomicU32 = AtomicU32::new(0);

fn fresh() -> Subject {
    Subject { program: ProgramId(0x4154_3033), pid: 0x7e00_0000 + RUN.fetch_add(1, Ordering::Relaxed), uid: 1000, tree: 0 }
}

fn noted() -> u64 {
    let mut n = mm_core::callers::HEAP_ATOMIC_LARGE.dropped();
    mm_core::callers::HEAP_ATOMIC_LARGE.each(&mut |s| n += s.count);
    n
}

/// Smallest live capacity whose doubling asks for two pages (8-byte entries).
const EDGE: usize = 512;
/// Fillers at most: the live list of a boot is far below this.
const MOST: usize = 8192;

fn udp_peer_grows_view() -> TestResult {
    let (filler, sock) = (fresh(), fresh());
    // The fillers' closes and events must not push the real history out of the kept lists.
    let keep = policy::conn_keep();
    policy::set_conn_keep(keep + 4 * MOST);
    let r = run(filler, sock);
    policy::forget_test_connections(&filler);
    policy::forget_test_connections(&sock);
    policy::set_conn_keep(keep);
    r
}

fn run(filler: Subject, sock: Subject) -> TestResult {
    let peers = UdpPeers::new(sock, 41_000);
    let mut held = Vec::with_capacity(MOST);
    let mut tries = 0u8;
    loop {
        let (len, cap) = policy::live_room();
        if len < cap || cap < EDGE {
            kassert!(held.len() < MOST, "the live list never came to its edge ({len} of {cap})");
            held.push(policy::open_conn(filler, NetProto::Tcp, false, 41_001, IpAddr::V4([192, 0, 2, 30]), 443));
            continue;
        }
        let before = noted();
        peers.sent(IpAddr::V4([192, 0, 2, 31]), 1000 + tries as u16, 64);
        let during = noted() - before;
        let grown = policy::live_room().1 > cap;
        klog::event!(Info, "nw", "ktest.udp_peer_grows_view", fillers = held.len(), capacity = cap, grown = grown, noted = during);
        // Another socket's open in the same moment may have taken the edge: try again at the next.
        if !grown {
            tries += 1;
            kassert!(tries < 5 && (peers.peers() as usize) < policy::udp_peer_keep(), "the peer's open never grew the live list");
            continue;
        }
        kassert!(during == 0, "{during} heap request(s) of two pages or more where the heap may not reclaim");
        kassert!(peers.peers() == tries as usize + 1, "{} peers", peers.peers());
        return Ok(());
    }
}
ktest!("nw.atomic3.udp_peer_grows_view", Safe, udp_peer_grows_view);
