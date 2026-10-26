//! PM-2 host tests: the ktests' cases, the parser, bounds of both stores, address text, threads.

use super::*;
use crate::ktests_view::*;
use crate::tests::SERIAL;

extern crate std;

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn ktest_cases_on_the_host() {
    let _g = serial();
    named_and_counted().unwrap();
    names_stay_with_their_program().unwrap();
    forget_leaves_other_names().unwrap();
    hostile_dns().unwrap();
    records_feed_the_view().unwrap();
    filters().unwrap();
    kernel_answers_ignored().unwrap();
    udp_peers().unwrap();
    lost_records_end().unwrap();
}

/// PM-2b (w11 Mini-PC C1): the kernel_dns ktest left a pid 0 row per run; reruns now leave neither rows nor records.
#[test]
fn kernel_dns_reruns_leave_nothing() {
    let _g = serial();
    let mine = |l: u16, r: IpAddr, rp: u16| l == 68 && r == IpAddr::V4([192, 0, 2, 53]) && rp == 53;
    for _ in 0..2 {
        kernel_answers_ignored().unwrap();
    }
    let rows = connections(&ConnFilter { pid: Some(0), ..ConnFilter::default() }, usize::MAX);
    assert!(!rows.iter().any(|c| mine(c.local_port, c.remote, c.remote_port)));
    assert!(!recent_connections(usize::MAX).iter().any(|r| r.subject.pid == 0 && mine(r.local_port, r.remote, r.remote_port)));
}

#[test]
fn parse_cname_chain_and_both_families() {
    let m = build_answer("WWW.Example.com", 28, &[
        Rr::Cname("www.example.com", "edge.example.net"),
        Rr::Cname("edge.example.net", "a1.cdn.example.org"),
        Rr::Aaaa("a1.cdn.example.org", YT6),
        Rr::A("a1.cdn.example.org", YT),
        Rr::A("unrelated.example", [192, 0, 2, 77]),
    ]);
    let (name, ips) = parse_answer(&m).unwrap();
    assert_eq!(name, "www.example.com");
    assert_eq!(ips, [IpAddr::V6(YT6), IpAddr::V4(YT)]);
}

#[test]
fn odd_label_bytes_are_shown_safely() {
    let mut m = build_answer("ab.example", 1, &[Rr::A("ab.example", [192, 0, 2, 5])]);
    m[13] = b'\n';
    m[14] = 0xff;
    let (name, _) = parse_answer(&m).unwrap();
    assert_eq!(name, "??.example");
}

#[test]
fn ip_text() {
    let t = |a: IpAddr| std::format!("{a}");
    assert_eq!(t(IpAddr::V4([192, 168, 100, 1])), "192.168.100.1");
    assert_eq!(t(IpAddr::V6(YT6)), "2001:db8::10");
    assert_eq!(t(IpAddr::V6([0; 16])), "::");
    let mut lo = [0u8; 16];
    lo[15] = 1;
    assert_eq!(t(IpAddr::V6(lo)), "::1");
    let mut m = [0u8; 16];
    m[10] = 0xff;
    m[11] = 0xff;
    m[12..].copy_from_slice(&[10, 0, 2, 3]);
    assert_eq!(t(IpAddr::V6(m)), "::ffff:10.0.2.3");
    let one: [u8; 16] = [0x20, 0x01, 0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1];
    assert_eq!(t(IpAddr::V6(one)), "2001:0:1::1:0:1");
}

#[test]
fn past_connections_follow_conn_keep_live_ones_stay() {
    let _g = serial();
    let old = conn_keep();
    let s = fresh(0x504D_0030);
    let live: std::vec::Vec<ConnHandle> =
        (0..20).map(|i| open_conn(s, NetProto::Tcp, false, 41000 + i, IpAddr::V4([192, 0, 2, 50]), 443)).collect();
    set_conn_keep(5);
    for i in 0..30u16 {
        drop(open_conn(s, NetProto::Tcp, false, 42000 + i, IpAddr::V4([192, 0, 2, 51]), 443));
    }
    let (_, past) = connection_counts();
    assert_eq!(past, 5);
    assert_eq!(mine(&s, true).len(), 20);
    let ports: std::vec::Vec<u16> = mine(&s, false).iter().filter(|c| !c.live()).map(|c| c.local_port).collect();
    assert_eq!(ports, [42029, 42028, 42027, 42026, 42025]);
    drop(live);
    set_conn_keep(3);
    assert_eq!(connection_counts().1, 3);
    set_conn_keep(old);
}

#[test]
fn dns_names_follow_dns_keep() {
    let mut n = dns::Names::new();
    let name: alloc::sync::Arc<str> = alloc::sync::Arc::from("x.example");
    for i in 0..10u8 {
        n.insert(7, 1, IpAddr::V4([192, 0, 2, i]), &name, 4);
    }
    assert_eq!(n.len(), 4);
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 5])).is_none());
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 9])).is_some());
    assert!(n.get(8, IpAddr::V4([192, 0, 2, 9])).is_none());
}

#[test]
fn threads_open_and_close_consistently() {
    let _g = serial();
    let s = fresh(0x504D_0031);
    let ts: std::vec::Vec<_> = (0..8u16)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..200u16 {
                    let h = open_conn(s, NetProto::Tcp, false, t * 1000 + i, IpAddr::V4([192, 0, 2, 60]), 443);
                    h.add_in(3);
                    if i % 2 == 0 {
                        h.close();
                    }
                }
            })
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    assert!(mine(&s, true).is_empty());
    let v = mine(&s, false);
    assert!(v.iter().all(|c| c.bytes_in == 3 && c.closed_ns.is_some()));
}

#[test]
fn names_evict_oldest_and_refresh_on_reinsert() {
    let mut n = dns::Names::new();
    let name: alloc::sync::Arc<str> = alloc::sync::Arc::from("x.example");
    for i in 0..4u8 {
        n.insert(7, 1, IpAddr::V4([192, 0, 2, i]), &name, 4);
    }
    // .0 answered again: now the newest, so .1 goes first.
    n.insert(7, 1, IpAddr::V4([192, 0, 2, 0]), &name, 4);
    n.insert(7, 1, IpAddr::V4([192, 0, 2, 9]), &name, 4);
    assert_eq!(n.len(), 4);
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 0])).is_some());
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 1])).is_none());
    n.forget(7, 2);
    assert_eq!(n.len(), 4, "another pid's forget took names");
    n.forget(7, 1);
    assert_eq!(n.len(), 0);
}

/// PM-2c: a test's forget takes only the names its own pid added, not the ones other processes of the same
/// program resolved (85 of Brave's names went with pm.conn.self_exec, which runs as ProgramId 1).
#[test]
fn forget_takes_only_the_pids_own_names() {
    let mut n = dns::Names::new();
    let name: alloc::sync::Arc<str> = alloc::sync::Arc::from("x.example");
    n.insert(7, 100, IpAddr::V4([192, 0, 2, 1]), &name, 16);
    n.insert(7, 200, IpAddr::V4([192, 0, 2, 2]), &name, 16);
    n.insert(8, 100, IpAddr::V4([192, 0, 2, 3]), &name, 16);
    // .3 of program 8 answered again for pid 200: now pid 200's.
    n.insert(8, 200, IpAddr::V4([192, 0, 2, 3]), &name, 16);
    n.forget(7, 100);
    n.forget(8, 100);
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 1])).is_none());
    assert!(n.get(7, IpAddr::V4([192, 0, 2, 2])).is_some());
    assert!(n.get(8, IpAddr::V4([192, 0, 2, 3])).is_some());
    assert_eq!(n.len(), 2);
}

#[test]
fn a_flood_of_cnames_is_cheap() {
    // About 4600 CNAMEs in one datagram, one after another: bounded work, no name.
    let names: std::vec::Vec<std::string::String> = (0..4600).map(|i| std::format!("c{i}.x")).collect();
    let mut rrs: std::vec::Vec<Rr<'_>> = (0..names.len() - 1).map(|i| Rr::Cname(&names[i], &names[i + 1])).collect();
    rrs.push(Rr::A(&names[names.len() - 1], [192, 0, 2, 1]));
    let m = build_answer("c0.x", 1, &rrs);
    let t = std::time::Instant::now();
    assert!(parse_answer(&m).is_none());
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
}

/// Processes the host tests pretend run (pid -> program name, comm); `Owners` answers from it.
static RUNNING: std::sync::Mutex<std::vec::Vec<(u32, &'static str, &'static str, u64)>> = std::sync::Mutex::new(std::vec::Vec::new());

struct FakeOwners;

impl Owners for FakeOwners {
    fn names(&self, s: &Subject, at_ns: u64) -> Option<(alloc::sync::Arc<str>, alloc::sync::Arc<str>)> {
        let r = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        r.iter().find(|p| p.0 == s.pid && p.3 <= at_ns).map(|p| (alloc::sync::Arc::from(p.1), alloc::sync::Arc::from(p.2)))
    }
}

static FAKE: FakeOwners = FakeOwners;

/// PM-2c (w12 Mini-PC step 5): the owner's names are stored when the connection opens, so a close after the
/// process ended still names it (9 of 10 of Brave's `conn.closed` had `comm=- exe=-`), and so does the view.
#[test]
fn owner_named_at_open_survives_its_end() {
    let _g = serial();
    set_owners(&FAKE);
    let (s, t, k) = (fresh(0x504D_0032), fresh(0x504D_0032), fresh(0x504D_0032));
    RUNNING.lock().unwrap().extend([(s.pid, "brave", "exe", 0), (t.pid, "curl", "curl", 0)]);
    let h = open_conn(s, NetProto::Tcp, false, 41001, IpAddr::V4([192, 0, 2, 80]), 443);
    let same = open_conn(t, NetProto::Udp, false, 41002, IpAddr::V4([192, 0, 2, 81]), 443);
    let unknown = open_conn(k, NetProto::Tcp, false, 41003, IpAddr::V4([192, 0, 2, 82]), 443);
    let kernel = open_conn(Subject::KERNEL, NetProto::Tcp, false, 41004, IpAddr::V4([192, 0, 2, 83]), 443);
    // The processes end before their connections do.
    RUNNING.lock().unwrap().retain(|p| p.0 != s.pid && p.0 != t.pid);
    drop((h, same, unknown));
    let v = mine(&s, false);
    assert_eq!((v[0].program_name.as_deref(), v[0].comm.as_deref()), (Some("brave"), Some("exe")));
    let v = mine(&t, false);
    assert_eq!((v[0].program_name.as_deref(), v[0].comm.as_deref()), (Some("curl"), None), "a comm equal to the name is not kept twice");
    let v = mine(&k, false);
    assert!(v[0].program_name.is_none() && v[0].comm.is_none(), "an unknown pid has no name");
    let kv = connections(&ConnFilter { pid: Some(0), live_only: true, ..ConnFilter::default() }, usize::MAX);
    assert!(kv.iter().filter(|c| c.local_port == 41004).all(|c| c.program_name.is_none()), "the kernel is not asked");
    drop(kernel);
    crate::view::forget_test_peer(&Subject::KERNEL, 41004, IpAddr::V4([192, 0, 2, 83]), 443);
    for x in [s, t, k] {
        forget_test_connections(&x);
    }
}

/// PM-2c fixer: a late `record_conn` record (its time before the pid's process started) is not named after a
/// process that reused the pid since; a record from after the start is.
#[test]
fn late_record_skips_a_reused_pid() {
    let _g = serial();
    set_owners(&FAKE);
    let s = fresh(0x504D_0033);
    RUNNING.lock().unwrap().push((s.pid, "newer", "newer", 1_000));
    let rec = |kind, port, time_ns| ConnRecord { subject: s, kind, proto: NetProto::Tcp, local_port: port, remote: IpAddr::V4([192, 0, 2, 84]), remote_port: 443, bytes_in: 0, bytes_out: 0, time_ns };
    record_conn(rec(ConnKind::Closed, 41005, 500));
    record_conn(rec(ConnKind::Opened, 41006, 2_000));
    RUNNING.lock().unwrap().retain(|p| p.0 != s.pid);
    let v = mine(&s, false);
    let port = |p: u16| v.iter().find(|c| c.local_port == p).map(|c| c.program_name.clone());
    assert_eq!(port(41005), Some(None), "a record older than the process is not its");
    assert_eq!(port(41006), Some(Some(alloc::sync::Arc::from("newer"))));
    record_conn(rec(ConnKind::Closed, 41006, 3_000));
    forget_test_connections(&s);
}
