//! PM-2 ktests: the connection view and its names. Safe ones are rerunnable (the Mini-PC runs them over MCP,
//! several times per boot): each run uses pids of its own above any real pid (pid_max is at most 2^22) and
//! looks only at its own connections; nothing is flushed, and each forgets what it left in the kept lists
//! (`forget_test_connections`), so many runs never push the real history out.

use crate::*;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ktest::{kassert, ktest, TestResult};

const P: u32 = 0x504D_0020;
static RUN: AtomicU32 = AtomicU32::new(0);

/// A subject no real process has: a new pid per call.
pub(crate) fn fresh(program: u32) -> Subject {
    Subject { program: ProgramId(program), pid: 0x7f00_0000 + RUN.fetch_add(1, Ordering::Relaxed), uid: 1000 }
}

/// One answer record for `build_answer`: owner name, type, data (A/AAAA bytes, or a CNAME target name).
pub(crate) enum Rr<'a> {
    A(&'a str, [u8; 4]),
    Aaaa(&'a str, [u8; 16]),
    Cname(&'a str, &'a str),
}

fn put_name(m: &mut Vec<u8>, name: &str) {
    for l in name.split('.').filter(|l| !l.is_empty()) {
        m.push(l.len() as u8);
        m.extend_from_slice(l.as_bytes());
    }
    m.push(0);
}

/// A DNS answer as a resolver sends it (RFC 1035 §4.1; the question's name compressed where it repeats).
pub(crate) fn build_answer(qname: &str, qtype: u16, rrs: &[Rr<'_>]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&[0x12, 0x34, 0x81, 0x80, 0, 1]);
    m.extend_from_slice(&(rrs.len() as u16).to_be_bytes());
    m.extend_from_slice(&[0, 0, 0, 0]);
    put_name(&mut m, qname);
    m.extend_from_slice(&qtype.to_be_bytes());
    m.extend_from_slice(&[0, 1]);
    for r in rrs {
        let (owner, ty) = match r {
            Rr::A(o, _) => (*o, 1u16),
            Rr::Aaaa(o, _) => (*o, 28),
            Rr::Cname(o, _) => (*o, 5),
        };
        if owner == qname {
            m.extend_from_slice(&[0xC0, 12]);
        } else {
            put_name(&mut m, owner);
        }
        m.extend_from_slice(&ty.to_be_bytes());
        m.extend_from_slice(&[0, 1, 0, 0, 0x0e, 0x10]);
        let mut data = Vec::new();
        match r {
            Rr::A(_, a) => data.extend_from_slice(a),
            Rr::Aaaa(_, a) => data.extend_from_slice(a),
            Rr::Cname(_, t) => put_name(&mut data, t),
        }
        m.extend_from_slice(&(data.len() as u16).to_be_bytes());
        m.extend_from_slice(&data);
    }
    m
}

pub(crate) fn mine(s: &Subject, live_only: bool) -> Vec<ConnInfo> {
    connections(&ConnFilter { pid: Some(s.pid), live_only, ..ConnFilter::default() }, usize::MAX)
}

pub(crate) const YT: [u8; 4] = [192, 0, 2, 10];
pub(crate) const YT6: [u8; 16] = [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10];

/// A name through a CNAME chain, live bytes, the close, an address the program never resolved.
pub(crate) fn named_and_counted() -> TestResult {
    let before = dns_names();
    let s = fresh(P);
    let ans = build_answer("www.YouTube.com", 1, &[
        Rr::Cname("www.youtube.com", "youtube-ui.l.google.com"),
        Rr::A("youtube-ui.l.google.com", YT),
        Rr::Aaaa("youtube-ui.l.google.com", YT6),
    ]);
    note_dns_answer(&s, &ans);
    let h = open_conn(s, NetProto::Tcp, false, 40001, IpAddr::V4(YT), 443);
    h.add_in(1000);
    h.add_out(100);
    let other = open_conn(s, NetProto::Tcp, false, 40002, IpAddr::V4([192, 0, 2, 99]), 80);
    let v = mine(&s, true);
    kassert!(v.len() == 2, "{} live", v.len());
    let yt = v.iter().find(|c| c.remote_port == 443).ok_or("no :443")?;
    kassert!(yt.name.as_deref() == Some("www.youtube.com"), "name {:?}", yt.name);
    kassert!((yt.bytes_in, yt.bytes_out) == (1000, 100), "bytes {} {}", yt.bytes_in, yt.bytes_out);
    kassert!(v.iter().any(|c| c.remote_port == 80 && c.name.is_none()), "an address never resolved has no name");
    h.close();
    drop(other);
    kassert!(mine(&s, true).is_empty(), "closed ones still live");
    let past = mine(&s, false);
    kassert!(past.len() == 2 && past.iter().all(|c| c.closed_ns.is_some()), "past {}", past.len());
    let yt = past.iter().find(|c| c.remote_port == 443).ok_or("no past :443")?;
    kassert!((yt.bytes_in, yt.bytes_out) == (1000, 100) && yt.last_ns >= yt.first_ns, "past bytes or times");
    // The v6 address of the same answer names a v6 connection.
    let h6 = open_conn(s, NetProto::Udp, false, 40003, IpAddr::V6(YT6), 443);
    kassert!(mine(&s, true).first().and_then(|c| c.name.clone()).as_deref() == Some("www.youtube.com"), "v6 name");
    drop(h6);
    forget_test_connections(&s);
    kassert!(mine(&s, false).is_empty() && !crate::dns::stored(&s, IpAddr::V4(YT)), "forgotten");
    names_not_taken(before)
}
ktest!("pm.conn.named", Safe, named_and_counted);

/// Names belong to the program that resolved them; BitsOS's own programs (id 0) per process.
pub(crate) fn names_stay_with_their_program() -> TestResult {
    let before = dns_names();
    let (a, b) = (fresh(0), fresh(0));
    note_dns_answer(&a, &build_answer("brave.com", 1, &[Rr::A("brave.com", [192, 0, 2, 20])]));
    let hb = open_conn(b, NetProto::Tcp, false, 40010, IpAddr::V4([192, 0, 2, 20]), 443);
    let ha = open_conn(a, NetProto::Tcp, false, 40011, IpAddr::V4([192, 0, 2, 20]), 443);
    kassert!(mine(&b, true)[0].name.is_none(), "another process's answer named it");
    kassert!(mine(&a, true)[0].name.as_deref() == Some("brave.com"), "own answer");
    drop((ha, hb));
    forget_test_connections(&a);
    forget_test_connections(&b);
    names_not_taken(before)
}
ktest!("pm.conn.per_program", Safe, names_stay_with_their_program);

/// PM-2c: a test forgets only the names its own pid added. Two processes of one program resolve; forgetting
/// one leaves the other's, and the table never ends up smaller than before the test (only forgets shrink it).
pub(crate) fn forget_leaves_other_names() -> TestResult {
    let before = dns_names();
    let a = fresh(P + 7);
    let b = Subject { pid: fresh(P + 7).pid, ..a };
    note_dns_answer(&a, &build_answer("a.example", 1, &[Rr::A("a.example", [192, 0, 2, 70])]));
    note_dns_answer(&b, &build_answer("b.example", 1, &[Rr::A("b.example", [192, 0, 2, 71])]));
    forget_test_connections(&a);
    let (gone, kept) = (!crate::dns::stored(&a, IpAddr::V4([192, 0, 2, 70])), crate::dns::stored(&b, IpAddr::V4([192, 0, 2, 71])));
    forget_test_connections(&b);
    kassert!(gone, "a's own name stayed");
    kassert!(kept, "a's forget took b's name (same program)");
    kassert!(!crate::dns::stored(&b, IpAddr::V4([192, 0, 2, 71])), "b's name stayed");
    names_not_taken(before)
}
ktest!("pm.conn.forget_own", Safe, forget_leaves_other_names);

/// The name table is not smaller than `before`: a test took only what it added. Inserting never shrinks it
/// (the oldest goes only at `dns_keep`, where the count stays), so this holds while real programs resolve
/// too; near `dns_keep` a test's own names may push real ones out, so the check is skipped there.
pub(crate) fn names_not_taken(before: usize) -> TestResult {
    let after = dns_names();
    kassert!(after >= before || before + 16 > dns_keep(), "the name table shrank from {before} to {after}");
    Ok(())
}

/// Hostile and odd messages: no name, no panic.
pub(crate) fn hostile_dns() -> TestResult {
    let before = dns_names();
    let good = build_answer("a.example", 1, &[Rr::A("a.example", [192, 0, 2, 1])]);
    kassert!(parse_answer(&good).is_some(), "the good one");
    let mut query = good.clone();
    query[2] &= 0x7f;
    let mut nx = good.clone();
    nx[3] |= 3;
    let mut looped = good.clone();
    looped[12] = 0xC0;
    looped[13] = 12;
    let other = build_answer("a.example", 1, &[Rr::A("b.example", [192, 0, 2, 2])]);
    let cases: [(&str, &[u8]); 6] =
        [("query", &query), ("nxdomain", &nx), ("pointer loop", &looped), ("short", &good[..20]), ("empty", &[]), ("other name", &other)];
    for (what, m) in cases {
        kassert!(parse_answer(m).is_none(), "{what} gave a name");
    }
    // A CNAME loop, and a chain longer than the bound (one link shorter still names it).
    let looped_chain = build_answer("a.example", 1, &[
        Rr::Cname("a.example", "b.example"), Rr::Cname("b.example", "a.example"), Rr::A("c.example", [192, 0, 2, 3]),
    ]);
    kassert!(parse_answer(&looped_chain).is_none(), "CNAME loop gave a name");
    let links: Vec<String> = (0..=17).map(|i| alloc::format!("n{i}.example")).collect();
    let chain = |n: usize| {
        let mut rrs: Vec<Rr<'_>> = (0..n).map(|i| Rr::Cname(if i == 0 { "q.example" } else { &links[i - 1] }, &links[i])).collect();
        rrs.push(Rr::A(&links[n - 1], [192, 0, 2, 4]));
        build_answer("q.example", 1, &rrs)
    };
    kassert!(parse_answer(&chain(16)).is_some(), "a chain of 16 links");
    kassert!(parse_answer(&chain(17)).is_none(), "a chain past the bound gave a name");
    // A pointer past the end, a forward pointer into a loop, RDLENGTH past the end, ANCOUNT 65535 with a short
    // body, two questions.
    let mut far = good.clone();
    far[12] = 0xC0;
    far[13] = 0xFF;
    let mut fwd = good.clone();
    let end = fwd.len() as u8;
    fwd[12] = 0xC0;
    fwd[13] = end;
    fwd.extend_from_slice(&[0xC0, 12]);
    let mut rdlen = good.clone();
    let n = rdlen.len();
    rdlen[n - 6] = 0xFF;
    rdlen[n - 5] = 0xFF;
    let mut many = good.clone();
    many[6] = 0xFF;
    many[7] = 0xFF;
    many.truncate(n - 2);
    let mut two = good.clone();
    two[5] = 2;
    let cases: [(&str, &[u8]); 5] = [("far pointer", &far), ("forward loop", &fwd), ("rdlength", &rdlen), ("ancount", &many), ("two questions", &two)];
    for (what, m) in cases {
        kassert!(parse_answer(m).is_none(), "{what} gave a name");
    }
    // Many addresses: at most MAX_ADDRS (32) are taken.
    let ips: Vec<[u8; 4]> = (0..100u8).map(|i| [198, 51, 100, i]).collect();
    let rrs: Vec<Rr<'_>> = ips.iter().map(|a| Rr::A("a.example", *a)).collect();
    let got = parse_answer(&build_answer("a.example", 1, &rrs)).map(|(_, v)| v.len());
    kassert!(got == Some(32), "{got:?} addresses from one answer");
    for n in 0..good.len() {
        let _ = parse_answer(&good[..n]);
    }
    let mut x: u32 = 0x1234_5678;
    let mut junk = good.clone();
    for _ in 0..2000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        let i = x as usize % junk.len();
        junk[i] = (x >> 8) as u8;
        let _ = parse_answer(&junk);
    }
    names_not_taken(before)
}
ktest!("pm.conn.dns_hostile", Safe, hostile_dns);

/// Callers of `record_conn` (MCP's server, the fetch tool) appear in the view: a Closed record ends the live one.
pub(crate) fn records_feed_the_view() -> TestResult {
    let before = dns_names();
    let s = fresh(P + 1);
    let rec = |kind, bytes_in, bytes_out| ConnRecord {
        subject: s, kind, proto: NetProto::Tcp, local_port: 8765, remote: IpAddr::V4([192, 0, 2, 30]), remote_port: 50000,
        bytes_in, bytes_out, time_ns: ksync::now_ns().unwrap_or(1),
    };
    // The same connection also held by a socket (the net stack's own entry): the Closed record leaves it.
    let held = open_conn(s, NetProto::Tcp, true, 8765, IpAddr::V4([192, 0, 2, 30]), 50000);
    record_conn(rec(ConnKind::Accepted, 0, 0));
    kassert!(mine(&s, true).len() == 2 && mine(&s, true).iter().all(|c| c.accepted), "accepted ones live");
    record_conn(rec(ConnKind::Closed, 300, 4000));
    let v = mine(&s, false);
    kassert!(v.len() == 2 && v[0].live() && !v[1].live() && (v[1].bytes_in, v[1].bytes_out) == (300, 4000), "the record's one closed with its bytes");
    drop(held);
    let mut u = rec(ConnKind::Opened, 0, 0);
    u.proto = NetProto::Udp;
    record_conn(u);
    kassert!(mine(&s, false).len() == 3 && mine(&s, true).is_empty(), "a UDP peer without a handle is a past entry");
    forget_test_connections(&s);
    kassert!(mine(&s, false).is_empty() && !recent_connections(usize::MAX).iter().any(|r| r.subject.pid == s.pid), "forgotten");
    names_not_taken(before)
}
ktest!("pm.conn.records", Safe, records_feed_the_view);

/// Filters: live only, by program, by text (name, address, program name), BitsOS's own or not.
pub(crate) fn filters() -> TestResult {
    let before = dns_names();
    let s = fresh(P + 2);
    note_dns_answer(&s, &build_answer("www.google.com", 1, &[Rr::A("www.google.com", [192, 0, 2, 40])]));
    let g = open_conn(s, NetProto::Tcp, false, 40020, IpAddr::V4([192, 0, 2, 40]), 443);
    let q = open_conn(s, NetProto::Udp, false, 40021, IpAddr::V4([192, 0, 2, 41]), 443);
    drop(q);
    let by = |f: ConnFilter<'_>| connections(&ConnFilter { pid: Some(s.pid), ..f }, usize::MAX).len();
    kassert!(by(ConnFilter::default()) == 2, "both");
    kassert!(by(ConnFilter { live_only: true, ..ConnFilter::default() }) == 1, "live only");
    kassert!(by(ConnFilter { text: Some("GOOGLE"), ..ConnFilter::default() }) == 1, "by name");
    kassert!(by(ConnFilter { text: Some("192.0.2.41:443"), ..ConnFilter::default() }) == 1, "by address");
    kassert!(by(ConnFilter { program: Some(ProgramId(P + 3)), ..ConnFilter::default() }) == 0, "other program");
    kassert!(by(ConnFilter { bitsos: Some(true), ..ConnFilter::default() }) == 0, "not BitsOS's own");
    let mut v = mine(&s, true);
    v[0].set_program_name("brave");
    kassert!(by(ConnFilter { text: Some("brave"), live_only: true, ..ConnFilter::default() }) == 1, "by program name");
    // PM-2b: Chromium's helpers exec /proc/self/exe: the name is the executable's, the comm `exe` matches too.
    v[0].set_program("brave", "exe");
    kassert!(mine(&s, true)[0].comm.as_deref() == Some("exe"), "comm kept");
    kassert!(by(ConnFilter { text: Some("EXE"), ..ConnFilter::default() }) == 1, "by comm");
    kassert!(by(ConnFilter { text: Some("brave"), ..ConnFilter::default() }) == 1, "by executable name");
    v[0].set_program("brave", "brave");
    kassert!(mine(&s, true)[0].comm.is_none() && by(ConnFilter { text: Some("exe"), ..ConnFilter::default() }) == 0, "same comm not kept");
    v[0].set_program("brave", "exe");
    v[0].set_program_name("chrome");
    kassert!(mine(&s, true)[0].comm.is_none() && v[0].comm.is_none(), "renaming drops the old comm");
    kassert!(by(ConnFilter { text: Some("exe"), ..ConnFilter::default() }) == 0, "old comm no longer matches");
    kassert!(connections(&ConnFilter { pid: Some(s.pid), ..ConnFilter::default() }, 1).len() == 1, "max");
    let mut names = String::new();
    for c in mine(&s, false) {
        names.push_str(c.name.as_deref().unwrap_or("-"));
    }
    kassert!(names == "www.google.com-", "live first: {names}");
    drop(g);
    forget_test_connections(&s);
    names_not_taken(before)
}
ktest!("pm.conn.filters", Safe, filters);

/// The kernel's own sockets never name anything: an answer for the kernel subject (pid 0) is not stored.
pub(crate) fn kernel_answers_ignored() -> TestResult {
    let ip = [192, 0, 2, 70];
    let ans = build_answer("kernel.example", 1, &[Rr::A("kernel.example", ip)]);
    note_dns_answer(&Subject::KERNEL, &ans);
    kassert!(!crate::dns::stored(&Subject::KERNEL, IpAddr::V4(ip)), "the kernel's answer was stored");
    let sys = Subject { program: ProgramId::SYSTEM, pid: 0, uid: 0 };
    note_dns_answer(&sys, &ans);
    kassert!(!crate::dns::stored(&sys, IpAddr::V4(ip)), "a pid 0 answer was stored");
    // Through a socket's peers too, also after the kernel socket asked that server.
    let k = UdpPeers::new(Subject::KERNEL, 68);
    k.sent(IpAddr::V4([192, 0, 2, 53]), 53, 30);
    k.received(IpAddr::V4([192, 0, 2, 53]), 53, ans.len(), Some(&ans));
    let stored = crate::dns::stored(&Subject::KERNEL, IpAddr::V4(ip));
    // PM-2b: the socket's peer row is the real kernel's (pid 0): forget only that peer, not the kernel's history.
    drop(k);
    crate::view::forget_test_peer(&Subject::KERNEL, 68, IpAddr::V4([192, 0, 2, 53]), 53);
    kassert!(!stored, "a kernel socket's answer was stored");
    let left = connections(&ConnFilter { pid: Some(0), text: Some("192.0.2.53:53"), ..ConnFilter::default() }, usize::MAX);
    kassert!(left.iter().all(|c| c.local_port != 68), "the test's kernel row was left behind");
    Ok(())
}
ktest!("pm.conn.kernel_dns", Safe, kernel_answers_ignored);

/// A socket's UDP peers: bytes per peer, names only from a server it asked, more peers than kept share one entry.
pub(crate) fn udp_peers() -> TestResult {
    let before = dns_names();
    let s = fresh(P + 4);
    let (ns, other) = (IpAddr::V4([192, 0, 2, 53]), IpAddr::V4([192, 0, 2, 66]));
    let ip = [192, 0, 2, 80];
    let ans = build_answer("bank.example", 1, &[Rr::A("bank.example", ip)]);
    {
        let u = UdpPeers::new(s, 40100);
        // An answer from a server never asked, then from another host than the one asked: no name.
        u.received(ns, 53, ans.len(), Some(&ans));
        kassert!(!crate::dns::stored(&s, IpAddr::V4(ip)), "an answer nobody asked for named it");
        let s2 = fresh(P + 4);
        let u2 = UdpPeers::new(s2, 40101);
        u2.sent(ns, 53, 30);
        u2.received(other, 53, ans.len(), Some(&ans));
        kassert!(!crate::dns::stored(&s2, IpAddr::V4(ip)), "another host's answer named it");
        u2.received(ns, 53, ans.len(), Some(&ans));
        kassert!(crate::dns::stored(&s2, IpAddr::V4(ip)), "the asked server's answer");
        // A peek counts nothing.
        u2.received(ns, 53, 0, None);
        let v = mine(&s2, true);
        let e = v.iter().find(|c| c.remote == ns).ok_or("no server entry")?;
        kassert!((e.bytes_in, e.bytes_out) == (ans.len() as u64, 30) && !e.accepted, "server bytes {} {}", e.bytes_in, e.bytes_out);
        kassert!(v.iter().any(|c| c.remote == other && c.accepted && c.bytes_in == ans.len() as u64), "the other host's entry");
        drop(u2);
        forget_test_connections(&s2);
        // More peers than kept: one entry for the rest, with all their bytes.
        let keep = udp_peer_keep();
        // The server above is one of the kept peers, so four of these share the entry.
        for i in 0..keep + 3 {
            u.received(IpAddr::V4([10, (i >> 16) as u8, (i >> 8) as u8, i as u8]), 9, 10, None);
        }
        kassert!(u.peers() == keep, "{} peers kept", u.peers());
        let v = mine(&s, true);
        let more = v.iter().find(|c| c.remote_port == 0).ok_or("no (more peers) entry")?;
        kassert!(more.bytes_in == 40 && more.name.as_deref() == Some("(more peers)"), "more {}", more.bytes_in);
        kassert!(v.len() == keep + 1, "{} entries", v.len());
    }
    kassert!(mine(&s, true).is_empty(), "the peers end with the socket");
    forget_test_connections(&s);
    names_not_taken(before)
}
ktest!("pm.conn.udp_peers", Safe, udp_peers);

/// A recorded connection whose Closed record never comes ends as lost after `record_live_ns`. QemuOnly: it
/// moves the clock of the records forward, which would end real recorded connections too.
pub(crate) fn lost_records_end() -> TestResult {
    let before = dns_names();
    let s = fresh(P + 5);
    let t0 = ksync::now_ns().unwrap_or(1).max(1);
    let rec = |port, t| ConnRecord {
        subject: s, kind: ConnKind::Opened, proto: NetProto::Tcp, local_port: port, remote: IpAddr::V4([192, 0, 2, 90]), remote_port: 443,
        bytes_in: 0, bytes_out: 0, time_ns: t,
    };
    record_conn(rec(40200, t0));
    kassert!(mine(&s, true).len() == 1, "recorded one live");
    record_conn(rec(40201, t0 + record_live_ns() + 1));
    let v = mine(&s, false);
    kassert!(v.iter().any(|c| c.local_port == 40200 && !c.live()), "the old one is not ended");
    kassert!(v.iter().any(|c| c.local_port == 40201 && c.live()), "the new one is not live");
    let mut c = rec(40201, t0 + record_live_ns() + 2);
    c.kind = ConnKind::Closed;
    record_conn(c);
    forget_test_connections(&s);
    names_not_taken(before)
}
ktest!("pm.conn.lost", QemuOnly, lost_records_end);
