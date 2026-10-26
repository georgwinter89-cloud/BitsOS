//! Host tests: rule matching, the table's fast path, ask with answer and timeout, the cache, the ring.
//! Tests that change the table hold `SERIAL` (the table and the mask are global to the test binary).

use super::*;
use crate::rules::{dirs, modes};
use alloc::string::ToString;
use alloc::vec;
use std::sync::Mutex;

extern crate std;

pub(crate) static SERIAL: Mutex<()> = Mutex::new(());

fn subj(p: u32) -> Subject {
    Subject { program: ProgramId(p), pid: 100 + p, uid: 1000 }
}

fn path(p: &str, write: bool, create: bool) -> Access<'_> {
    Access::Path { path: p, write, create }
}

fn tcp(a: [u8; 4], port: u16) -> Access<'static> {
    Access::NetConnect { proto: NetProto::Tcp, addr: IpAddr::V4(a), port }
}

#[test]
fn default_allows_and_counts() {
    klog::set_serial(false);
    let _g = SERIAL.lock();
    let s = subj(5);
    let before = counts()[1].1;
    assert!(check(&s, &Access::Device { class: DeviceClass::Microphone, name: "hda" }).is_ok());
    assert!(check(&s, &tcp([1, 1, 1, 1], 443)).is_ok());
    assert!(counts()[1].1 > before);
    assert!(allows_any(&s, AccessClass::Path));
    assert!(check(&Subject::KERNEL, &path("/x", true, true)).is_ok());
}

#[test]
fn path_rules_longest_prefix_and_modes() {
    let mut r = RuleSet::allow_all();
    r.paths = vec![
        PathRule { prefix: "/home/georg".to_string(), modes: modes::ALL, decision: Decision::Deny },
        PathRule { prefix: "/home/georg/Downloads/".to_string(), modes: modes::ALL, decision: Decision::Allow },
        PathRule { prefix: "/etc".to_string(), modes: modes::WRITE | modes::CREATE, decision: Decision::Deny },
    ];
    assert_eq!(r.decide(&path("/home/georg", false, false)), Decision::Deny);
    assert_eq!(r.decide(&path("/home/georg/a", false, false)), Decision::Deny);
    assert_eq!(r.decide(&path("/home/georgx", false, false)), Decision::Allow, "component boundary");
    assert_eq!(r.decide(&path("/home/georg/Downloads/f", true, true)), Decision::Allow, "longer prefix wins");
    assert_eq!(r.decide(&path("/etc/passwd", false, false)), Decision::Allow, "reading is not covered");
    assert_eq!(r.decide(&path("/etc/passwd", true, false)), Decision::Deny);
    assert_eq!(r.decide(&path("/etc/new", false, true)), Decision::Deny);
    assert_eq!(r.unconditional(AccessClass::Path), None);
    assert_eq!(r.unconditional(AccessClass::Net), Some(Decision::Allow));
    // Same prefix: the strictest wins.
    r.paths.push(PathRule { prefix: "/home/georg/Downloads".to_string(), modes: modes::READ, decision: Decision::Ask });
    assert_eq!(r.decide(&path("/home/georg/Downloads/f", false, false)), Decision::Ask);
    // "/" covers everything.
    let mut all = RuleSet::allow_all();
    all.paths.push(PathRule { prefix: "/".to_string(), modes: modes::READ, decision: Decision::Deny });
    assert_eq!(all.decide(&path("/a/b", false, false)), Decision::Deny);
    assert_eq!(all.decide(&path("/a/b", true, false)), Decision::Allow);
}

#[test]
fn net_rules_specific_wins() {
    let mut r = RuleSet::with_defaults([Decision::Allow, Decision::Deny, Decision::Allow, Decision::Allow, Decision::Allow, Decision::Allow]);
    r.net = vec![
        NetRule { dirs: dirs::CONNECT, proto: Some(NetProto::Tcp), addr: Some((IpAddr::V4([192, 168, 100, 0]), 24)), ports: (0, 65535), decision: Decision::Allow },
        NetRule { dirs: dirs::CONNECT, proto: None, addr: Some((IpAddr::V4([192, 168, 100, 1]), 32)), ports: (53, 53), decision: Decision::Deny },
        NetRule { dirs: dirs::BIND | dirs::LISTEN, proto: None, addr: None, ports: (8000, 8999), decision: Decision::Ask },
    ];
    assert_eq!(r.decide(&tcp([192, 168, 100, 20], 443)), Decision::Allow);
    assert_eq!(r.decide(&tcp([192, 168, 100, 1], 53)), Decision::Deny, "narrower address wins");
    assert_eq!(r.decide(&tcp([192, 168, 179, 1], 443)), Decision::Deny, "class default");
    assert_eq!(r.decide(&Access::NetBind { proto: NetProto::Udp, port: 8080 }), Decision::Ask);
    assert_eq!(r.decide(&Access::NetListen { port: 8999 }), Decision::Ask);
    assert_eq!(r.decide(&Access::NetListen { port: 9000 }), Decision::Deny);
    let v6 = Access::NetConnect { proto: NetProto::Tcp, addr: IpAddr::V6([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]), port: 1 };
    assert_eq!(r.decide(&v6), Decision::Deny, "a v4 rule never covers v6");
    r.net.push(NetRule { dirs: dirs::CONNECT, proto: None, addr: Some((IpAddr::V6([0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), 10)), ports: (0, 65535), decision: Decision::Allow });
    assert_eq!(r.decide(&v6), Decision::Allow);
}

#[test]
fn device_signal_action_scheduling_rules() {
    let mut r = RuleSet::allow_all();
    r.devices = vec![
        DeviceRule { class: DeviceClass::Microphone, name: None, decision: Decision::Ask },
        DeviceRule { class: DeviceClass::Microphone, name: Some("pcmC0D0c".to_string()), decision: Decision::Deny },
    ];
    r.signals = vec![SignalRule { signal: None, target_pid: Some(1), decision: Decision::Deny }, SignalRule { signal: Some(9), target_pid: None, decision: Decision::Ask }];
    r.actions = vec![NameRule { pattern: "app.brave.*".to_string(), decision: Decision::Ask }, NameRule { pattern: "app.brave.open".to_string(), decision: Decision::Allow }];
    r.scheduling = vec![NameRule { pattern: "*".to_string(), decision: Decision::Deny }];
    assert_eq!(r.decide(&Access::Device { class: DeviceClass::Microphone, name: "usb0" }), Decision::Ask);
    assert_eq!(r.decide(&Access::Device { class: DeviceClass::Microphone, name: "pcmC0D0c" }), Decision::Deny);
    assert_eq!(r.decide(&Access::Device { class: DeviceClass::Audio, name: "pcmC0D0p" }), Decision::Allow);
    assert_eq!(r.decide(&Access::Signal { target_pid: 1, signal: 15 }), Decision::Deny);
    assert_eq!(r.decide(&Access::Signal { target_pid: 7, signal: 9 }), Decision::Ask);
    assert_eq!(r.decide(&Access::Signal { target_pid: 7, signal: 15 }), Decision::Allow);
    assert_eq!(r.decide(&Access::McpAction { tool: "app.brave.open" }), Decision::Allow, "exact name wins");
    assert_eq!(r.decide(&Access::McpAction { tool: "app.brave.delete" }), Decision::Ask);
    assert_eq!(r.decide(&Access::McpAction { tool: "app.other.x" }), Decision::Allow);
    assert_eq!(r.decide(&Access::Scheduling { grant: "compositor" }), Decision::Deny);
}

#[test]
fn table_fast_path_and_refusal() {
    let _g = SERIAL.lock();
    let p = ProgramId(9001);
    let s = subj(9001);
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/secret".to_string(), modes: modes::ALL, decision: Decision::Deny });
    let g0 = generation();
    set_rules(p, Some(r));
    assert!(generation() > g0);
    assert!(!allows_any(&s, AccessClass::Path), "path rules exist: the caller must build the path");
    assert!(allows_any(&s, AccessClass::Net));
    assert!(allows_any(&subj(9002), AccessClass::Path), "other programs keep the fast answer");
    let before = denied()[0].1;
    let e = check(&s, &path("/secret/x", false, false)).unwrap_err();
    assert!(e.reason.contains("/secret/x") && e.reason.contains("Programm 9001"), "{}", e.reason);
    assert_eq!(denied()[0].1, before + 1);
    assert!(check(&s, &path("/open/x", false, false)).is_ok());
    // SYSTEM keeps allow-all even under a strict default for foreign programs.
    set_default_rules(Some(RuleSet::with_defaults([Decision::Deny; CLASSES])));
    assert!(check(&Subject { program: ProgramId::SYSTEM, pid: 5, uid: 0 }, &path("/x", true, true)).is_ok());
    assert!(check(&subj(9003), &path("/x", false, false)).is_err());
    set_default_rules(None);
    set_rules(p, None);
    assert!(check(&s, &path("/secret/x", false, false)).is_ok());
}

#[test]
fn cache_follows_rule_changes() {
    let _g = SERIAL.lock();
    let p = ProgramId(9010);
    let s = subj(9010);
    let c = Cache::new();
    assert_eq!(c.unconditional(p, AccessClass::Device), Some(Decision::Allow));
    let mut r = RuleSet::allow_all();
    r.defaults[AccessClass::Device as usize] = Decision::Deny;
    set_rules(p, Some(r));
    assert_eq!(c.unconditional(p, AccessClass::Device), Some(Decision::Deny), "new generation refills");
    assert_eq!(c.generation(), generation_tag());
    assert!(check_cached(&c, &s, &Access::Device { class: DeviceClass::Camera, name: "v0" }).is_err());
    set_rules(p, None);
    assert!(check_cached(&c, &s, &Access::Device { class: DeviceClass::Camera, name: "v0" }).is_ok());
}

#[test]
fn ask_answered_and_remembered() {
    let _g = SERIAL.lock();
    let p = ProgramId(9020);
    let s = subj(9020);
    let mut r = RuleSet::allow_all();
    r.devices.push(DeviceRule { class: DeviceClass::Camera, name: None, decision: Decision::Ask });
    r.ask_timeout_ns = Some(20_000_000_000);
    set_rules(p, Some(r));
    let answerer = std::thread::spawn(move || {
        for _ in 0..20_000 {
            if let Some(a) = pending_asks().into_iter().find(|a| a.subject.program == p) {
                assert!(a.what.contains("Camera"), "{}", a.what);
                return answer(a.id, Decision::Allow, true);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    });
    assert!(check(&s, &Access::Device { class: DeviceClass::Camera, name: "video0" }).is_ok());
    assert!(answerer.join().unwrap());
    // "always": the next access is decided by the learned rule, nothing is asked.
    assert_eq!(learned_rules(p).matched(&Access::Device { class: DeviceClass::Camera, name: "video0" }), Some(Decision::Allow));
    assert!(check(&s, &Access::Device { class: DeviceClass::Camera, name: "video0" }).is_ok());
    assert!(pending_asks().iter().all(|a| a.subject.program != p));
    assert!(!answer(u64::MAX, Decision::Allow, false), "unknown request");
    set_rules(p, None);
}

#[test]
fn ask_times_out_to_class_default() {
    let _g = SERIAL.lock();
    let p = ProgramId(9030);
    let s = subj(9030);
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/q".to_string(), modes: modes::ALL, decision: Decision::Ask });
    r.ask_timeout_ns = Some(30_000_000);
    set_rules(p, Some(r.clone()));
    assert!(check(&s, &path("/q/a", false, false)).is_ok(), "class default allow");
    r.defaults[AccessClass::Path as usize] = Decision::Ask;
    set_rules(p, Some(r));
    let e = check(&s, &path("/q/a", false, false)).unwrap_err();
    assert!(e.reason.contains("keine Antwort"), "{}", e.reason);
    assert!(pending_asks().iter().all(|a| a.subject.program != p), "timed-out request removed");
    set_rules(p, None);
}

#[test]
fn connection_ring_keeps_newest() {
    let _g = SERIAL.lock();
    let s = subj(5);
    let old = conn_keep();
    set_conn_keep(3);
    for port in 1..=5u16 {
        record_conn(ConnRecord { subject: s, kind: ConnKind::Opened, proto: NetProto::Tcp, local_port: 40000, remote: IpAddr::V4([1, 1, 1, 1]), remote_port: port, bytes_in: 0, bytes_out: 0, time_ns: 0 });
    }
    let r = recent_connections(10);
    assert_eq!(r.iter().map(|c| c.remote_port).collect::<std::vec::Vec<_>>(), [5, 4, 3]);
    assert_eq!(recent_connections(1)[0].remote_port, 5);
    set_conn_keep(old);
}

#[test]
fn describe_in_words() {
    assert_eq!(describe(&tcp([10, 0, 0, 1], 80)), "Verbindung Tcp zu 10.0.0.1 Port 80");
    assert_eq!(describe(&path("/a", true, false)), "Datei /a schreiben");
    let _ = (dirs::ALL, modes::ALL);
}

// ---- fixer round (PM-1 checker findings) ------------------------------------------------------------------

#[test]
fn non_canonical_paths_never_slip_past_a_rule() {
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/secret".to_string(), modes: modes::ALL, decision: Decision::Deny });
    r.paths.push(PathRule { prefix: "/open".to_string(), modes: modes::ALL, decision: Decision::Allow });
    for p in ["/secret/../etc/x", "//secret", "/secret/./x", "/open/../secret/x", "", "secret/x", "./secret", "/open//x", "/x/.."] {
        assert_eq!(r.decide(&path(p, false, false)), Decision::Deny, "{p:?}");
        assert_eq!(r.matched(&path(p, false, false)), None, "{p:?}");
    }
    for p in ["/", "/etc/x", "/open/x", "/secretx", "/etc/"] {
        assert!(rules::canonical(p), "{p:?}");
    }
    assert_eq!(r.decide(&path("/etc/x", false, false)), Decision::Allow);
    // Only allow rules and an allow default: a non-canonical path is still allowed (nothing to slip past).
    let mut a = RuleSet::allow_all();
    a.paths.push(PathRule { prefix: "/open".to_string(), modes: modes::READ, decision: Decision::Allow });
    assert_eq!(a.decide(&path("../x", false, false)), Decision::Allow);
    // A rule for another mode does not make a read stricter.
    let mut w = RuleSet::allow_all();
    w.paths.push(PathRule { prefix: "/etc".to_string(), modes: modes::WRITE, decision: Decision::Deny });
    assert_eq!(w.decide(&path("/etc/../x", false, false)), Decision::Allow);
    assert_eq!(w.decide(&path("/etc/../x", true, false)), Decision::Deny);
    // An empty prefix covers every canonical path.
    let mut e = RuleSet::allow_all();
    e.paths.push(PathRule { prefix: String::new(), modes: modes::READ, decision: Decision::Ask });
    assert_eq!(e.decide(&path("/a", false, false)), Decision::Ask);
    // Through the table: check() refuses the dot-dot form.
    let _g = SERIAL.lock();
    set_rules(ProgramId(9040), Some(r));
    assert!(check(&subj(9040), &path("/open/../secret/k", false, false)).is_err());
    assert!(check(&subj(9040), &path("/open/k", false, false)).is_ok());
    set_rules(ProgramId(9040), None);
}

#[test]
fn hostile_net_rules_have_a_defined_result() {
    let v4 = |a: [u8; 4], port| tcp(a, port);
    let v6 = Access::NetConnect { proto: NetProto::Udp, addr: IpAddr::V6([0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]), port: 53 };
    let rule = |addr, ports, proto, d| NetRule { dirs: dirs::ALL, proto, addr, ports, decision: d };
    let mut r = RuleSet::allow_all();
    r.net = vec![
        rule(Some((IpAddr::V4([10, 0, 0, 1]), 200)), (0, 65535), Some(NetProto::Tcp), Decision::Deny),
        rule(Some((IpAddr::V6([0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7]), 255)), (0, 65535), None, Decision::Deny),
        rule(None, (9000, 10), None, Decision::Deny),
    ];
    assert_eq!(r.decide(&v4([10, 0, 0, 1], 1)), Decision::Deny, "/200 counts as /32");
    assert_eq!(r.decide(&v4([10, 0, 0, 2], 1)), Decision::Allow);
    assert_eq!(r.decide(&v6), Decision::Deny, "/255 counts as /128, proto None covers UDP");
    assert_eq!(r.decide(&v4([1, 2, 3, 4], 9500)), Decision::Allow, "an inverted port range matches nothing");
    assert_eq!(r.decide(&Access::NetBind { proto: NetProto::Udp, port: 9000 }), Decision::Allow);
    // /0 covers every address of its family only.
    let mut z = RuleSet::allow_all();
    z.net = vec![rule(Some((IpAddr::V4([0; 4]), 0)), (0, 65535), None, Decision::Ask)];
    assert_eq!(z.decide(&v4([200, 1, 1, 1], 80)), Decision::Ask);
    assert_eq!(z.decide(&v6), Decision::Allow, "a v4 /0 does not cover v6");
    z.net.push(rule(Some((IpAddr::V6([0; 16]), 0)), (53, 53), Some(NetProto::Udp), Decision::Deny));
    assert_eq!(z.decide(&v6), Decision::Deny);
}

fn wait_pending(p: ProgramId, n: usize) -> std::vec::Vec<AskInfo> {
    for _ in 0..20_000 {
        let v: std::vec::Vec<_> = pending_asks().into_iter().filter(|a| a.subject.program == p).collect();
        if v.len() >= n && !v.is_empty() {
            return v;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    std::vec::Vec::new()
}

#[test]
fn identical_asks_share_one_request() {
    let _g = SERIAL.lock();
    let p = ProgramId(9050);
    let mut r = RuleSet::allow_all();
    r.devices.push(DeviceRule { class: DeviceClass::Camera, name: None, decision: Decision::Ask });
    r.ask_timeout_ns = Some(20_000_000_000);
    set_rules(p, Some(r));
    let waiters: std::vec::Vec<_> = (0..3)
        .map(|i| std::thread::spawn(move || check(&Subject { program: ProgramId(9050), pid: 200 + i, uid: 1000 }, &Access::Device { class: DeviceClass::Camera, name: "video0" }).is_ok()))
        .collect();
    // Wait until all three are parked on the one request (they cannot be told apart from outside, so give
    // the late ones time to join it).
    let v = wait_pending(p, 1);
    assert_eq!(v.len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(pending_asks().iter().filter(|a| a.subject.program == p).count(), 1, "one prompt for three waiters");
    assert!(answer(v[0].id, Decision::Allow, true));
    for w in waiters {
        assert!(w.join().unwrap(), "every waiter released by the one answer");
    }
    assert!(!answer(v[0].id, Decision::Deny, false), "a second answer finds nothing");
    assert_eq!(learned_rules(p).devices.len(), 1, "one learned rule");
    set_rules(p, None);
    assert!(learned_rules(p).devices.is_empty(), "set_rules replaces the learned rules too");
}

#[test]
fn late_answer_after_timeout_is_refused() {
    let _g = SERIAL.lock();
    let p = ProgramId(9060);
    let mut r = RuleSet::allow_all();
    r.signals.push(SignalRule { signal: Some(9), target_pid: None, decision: Decision::Ask });
    r.defaults[AccessClass::Signal as usize] = Decision::Deny;
    r.ask_timeout_ns = Some(200_000_000);
    set_rules(p, Some(r));
    let ids = std::sync::Arc::new(std::sync::Mutex::new(std::vec::Vec::new()));
    let seen = ids.clone();
    let spy = std::thread::spawn(move || {
        let v = wait_pending(ProgramId(9060), 1);
        seen.lock().unwrap().extend(v.iter().map(|a| a.id));
    });
    assert!(check(&subj(9060), &Access::Signal { target_pid: 3, signal: 9 }).is_err(), "timeout -> class default deny");
    spy.join().unwrap();
    let id = ids.lock().unwrap().first().copied().expect("the ask was queued");
    assert!(!answer(id, Decision::Allow, true), "answer after the deadline");
    assert!(learned_rules(p).signals.is_empty(), "a refused answer learns nothing");
    set_rules(p, None);
}

#[test]
fn learned_rule_keeps_following_the_default() {
    let _g = SERIAL.lock();
    let p = ProgramId(9070);
    let mut d = RuleSet::allow_all();
    d.actions.push(NameRule { pattern: "app.x.*".to_string(), decision: Decision::Ask });
    d.ask_timeout_ns = Some(20_000_000_000);
    set_default_rules(Some(d));
    let answerer = std::thread::spawn(move || {
        let v = wait_pending(ProgramId(9070), 1);
        v.first().is_some_and(|a| answer(a.id, Decision::Allow, true))
    });
    assert!(check(&subj(9070), &Access::McpAction { tool: "app.x.run" }).is_ok());
    assert!(answerer.join().unwrap());
    // The default changes afterwards: the program still follows it, except for what Georg answered.
    let mut strict = RuleSet::allow_all();
    strict.defaults[AccessClass::Device as usize] = Decision::Deny;
    strict.actions.push(NameRule { pattern: "app.x.*".to_string(), decision: Decision::Deny });
    set_default_rules(Some(strict));
    assert!(check(&subj(9070), &Access::Device { class: DeviceClass::Gpu, name: "card0" }).is_err(), "new default applies");
    assert!(check(&subj(9070), &Access::McpAction { tool: "app.x.run" }).is_ok(), "the learned answer stays");
    assert!(check(&subj(9070), &Access::McpAction { tool: "app.x.other" }).is_err());
    assert!(!allows_any(&subj(9070), AccessClass::McpAction));
    set_default_rules(None);
    set_rules(p, None);
    assert!(allows_any(&subj(9070), AccessClass::McpAction));
}

#[test]
fn cache_shared_by_threads_ends_consistent() {
    let _g = SERIAL.lock();
    let p = ProgramId(9080);
    let c = std::sync::Arc::new(Cache::new());
    let stop = std::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let readers: std::vec::Vec<_> = (0..4)
        .map(|_| {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = c.unconditional(ProgramId(9080), AccessClass::Device);
                }
            })
        })
        .collect();
    for i in 0..200 {
        let mut r = RuleSet::allow_all();
        r.defaults[AccessClass::Device as usize] = if i % 2 == 0 { Decision::Deny } else { Decision::Allow };
        set_rules(p, Some(r));
    }
    let mut r = RuleSet::allow_all();
    r.defaults[AccessClass::Device as usize] = Decision::Deny;
    set_rules(p, Some(r));
    for _ in 0..1000 {
        assert_eq!(c.unconditional(p, AccessClass::Device), Some(Decision::Deny), "a stale allow survived");
    }
    stop.store(true, Ordering::Relaxed);
    for t in readers {
        t.join().unwrap();
    }
    assert_eq!(c.unconditional(p, AccessClass::Device), Some(Decision::Deny));
    set_rules(p, None);
}

#[test]
fn connection_ring_grows_and_trims() {
    let _g = SERIAL.lock();
    let s = subj(6);
    let old = conn_keep();
    set_conn_keep(50);
    for port in 1..=120u16 {
        record_conn(ConnRecord { subject: s, kind: ConnKind::Opened, proto: NetProto::Udp, local_port: 1, remote: IpAddr::V4([2, 2, 2, 2]), remote_port: port, bytes_in: 0, bytes_out: 0, time_ns: 0 });
    }
    let r = recent_connections(1000);
    assert_eq!(r.len(), 50);
    assert_eq!(r.first().map(|c| c.remote_port), Some(120));
    assert_eq!(r.last().map(|c| c.remote_port), Some(71));
    set_conn_keep(old);
}
