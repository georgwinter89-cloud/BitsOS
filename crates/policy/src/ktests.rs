//! PM-1 ktests: each access class through `check`, ask answered and timed out, the cache after a rule change,
//! the cost of a check. Safe ones are rerunnable: each test uses its own program id (no real program has it)
//! and removes its rules when it ends, also on failure.

use crate::rules::{dirs, modes};
use crate::*;
use alloc::string::ToString;
use ktest::{kassert, ktest, TestResult};

/// Removes the test program's rules when dropped.
struct Rules(ProgramId);
impl Rules {
    fn set(p: u32, r: RuleSet) -> Rules {
        set_rules(ProgramId(p), Some(r));
        Rules(ProgramId(p))
    }
}
impl Drop for Rules {
    fn drop(&mut self) {
        set_rules(self.0, None);
    }
}

fn subj(p: u32) -> Subject {
    Subject { program: ProgramId(p), pid: 0x7fff_5000 + (p & 0xfff), uid: 1000 }
}

fn denies(s: &Subject, a: &Access<'_>) -> bool {
    check(s, a).is_err()
}

const P: u32 = 0x504D_0000;

fn defaults() -> TestResult {
    let s = subj(P + 1);
    if rules_of(s.program) != RuleSet::allow_all() {
        return ktest::skip("default rules for foreign programs are installed (permd)");
    }
    let accesses = [
        Access::Path { path: "/pm-ktest/x", write: true, create: true },
        Access::NetConnect { proto: NetProto::Tcp, addr: IpAddr::V4([10, 0, 2, 2]), port: 443 },
        Access::Device { class: DeviceClass::Microphone, name: "pcmC0D0c" },
        Access::Signal { target_pid: 1, signal: 9 },
        Access::McpAction { tool: "app.pm.ktest" },
        Access::Scheduling { grant: "compositor" },
    ];
    for a in &accesses {
        let before = counts()[a.class() as usize].1;
        kassert!(check(&s, a).is_ok(), "default refused {a:?}");
        kassert!(counts()[a.class() as usize].1 > before, "{a:?} not counted");
    }
    kassert!(check(&Subject { program: ProgramId::SYSTEM, pid: 1, uid: 0 }, &accesses[0]).is_ok(), "BitsOS's own programs allow");
    Ok(())
}
ktest!("pm.default.allow", Safe, defaults);

fn path_class() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/pm-ktest".to_string(), modes: modes::WRITE | modes::CREATE, decision: Decision::Deny });
    r.paths.push(PathRule { prefix: "/pm-ktest/ok".to_string(), modes: modes::ALL, decision: Decision::Allow });
    let _g = Rules::set(P + 2, r);
    let s = subj(P + 2);
    kassert!(!allows_any(&s, AccessClass::Path), "path rules exist, the fast answer must say no");
    kassert!(allows_any(&s, AccessClass::Net));
    let before = denied()[0].1;
    kassert!(denies(&s, &Access::Path { path: "/pm-ktest/a", write: true, create: false }));
    kassert!(denies(&s, &Access::Path { path: "/pm-ktest/a", write: false, create: true }));
    kassert!(!denies(&s, &Access::Path { path: "/pm-ktest/a", write: false, create: false }), "read not covered");
    kassert!(!denies(&s, &Access::Path { path: "/pm-ktest/ok/b", write: true, create: true }), "longer prefix wins");
    kassert!(!denies(&s, &Access::Path { path: "/pm-ktestx", write: true, create: false }), "component boundary");
    kassert!(denied()[0].1 >= before + 2);
    let e = check(&s, &Access::Path { path: "/pm-ktest/a", write: true, create: false });
    kassert!(e.as_ref().is_err_and(|e| e.reason.contains("/pm-ktest/a")), "reason: {e:?}");
    Ok(())
}
ktest!("pm.class.path", Safe, path_class);

fn net_class() -> TestResult {
    let mut r = RuleSet::with_defaults([Decision::Allow, Decision::Deny, Decision::Allow, Decision::Allow, Decision::Allow, Decision::Allow]);
    r.net.push(NetRule { dirs: dirs::CONNECT, proto: Some(NetProto::Tcp), addr: Some((IpAddr::V4([10, 0, 2, 0]), 24)), ports: (443, 443), decision: Decision::Allow });
    r.net.push(NetRule { dirs: dirs::BIND, proto: Some(NetProto::Udp), addr: None, ports: (5353, 5353), decision: Decision::Allow });
    let _g = Rules::set(P + 3, r);
    let s = subj(P + 3);
    kassert!(!denies(&s, &Access::NetConnect { proto: NetProto::Tcp, addr: IpAddr::V4([10, 0, 2, 2]), port: 443 }));
    kassert!(denies(&s, &Access::NetConnect { proto: NetProto::Tcp, addr: IpAddr::V4([10, 0, 3, 2]), port: 443 }));
    kassert!(denies(&s, &Access::NetConnect { proto: NetProto::Udp, addr: IpAddr::V4([10, 0, 2, 2]), port: 443 }));
    kassert!(!denies(&s, &Access::NetBind { proto: NetProto::Udp, port: 5353 }));
    kassert!(denies(&s, &Access::NetBind { proto: NetProto::Tcp, port: 5353 }));
    kassert!(denies(&s, &Access::NetListen { port: 80 }));
    Ok(())
}
ktest!("pm.class.net", Safe, net_class);

fn device_class() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.devices.push(DeviceRule { class: DeviceClass::Microphone, name: None, decision: Decision::Deny });
    let _g = Rules::set(P + 4, r);
    let s = subj(P + 4);
    kassert!(denies(&s, &Access::Device { class: DeviceClass::Microphone, name: "pcmC0D0c" }));
    kassert!(!denies(&s, &Access::Device { class: DeviceClass::Audio, name: "pcmC0D0p" }));
    Ok(())
}
ktest!("pm.class.device", Safe, device_class);

fn signal_class() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.signals.push(SignalRule { signal: Some(9), target_pid: None, decision: Decision::Deny });
    let _g = Rules::set(P + 5, r);
    let s = subj(P + 5);
    kassert!(denies(&s, &Access::Signal { target_pid: 77, signal: 9 }));
    kassert!(!denies(&s, &Access::Signal { target_pid: 77, signal: 15 }));
    Ok(())
}
ktest!("pm.class.signal", Safe, signal_class);

fn action_and_scheduling_class() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.actions.push(NameRule { pattern: "app.pm.*".to_string(), decision: Decision::Deny });
    r.scheduling.push(NameRule { pattern: "compositor".to_string(), decision: Decision::Deny });
    let _g = Rules::set(P + 6, r);
    let s = subj(P + 6);
    kassert!(denies(&s, &Access::McpAction { tool: "app.pm.delete" }));
    kassert!(!denies(&s, &Access::McpAction { tool: "app.other.delete" }));
    kassert!(denies(&s, &Access::Scheduling { grant: "compositor" }));
    Ok(())
}
ktest!("pm.class.action", Safe, action_and_scheduling_class);

/// Answers the first request of `program` from a second thread (as permd will).
fn answer_from_thread(program: ProgramId, d: Decision, always: bool) -> Result<sched::JoinHandle, alloc::string::String> {
    sched::spawn("pm.ktest.answer", sched::Class::Normal, move || {
        for _ in 0..4000 {
            if let Some(a) = pending_asks().into_iter().find(|a| a.subject.program == program) {
                answer(a.id, d, always);
                return;
            }
            sched::sleep_ns(1_000_000);
        }
    })
    .map_err(|e| alloc::format!("spawn: {e:?}"))
}

fn ask_answered() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.devices.push(DeviceRule { class: DeviceClass::Camera, name: None, decision: Decision::Ask });
    r.ask_timeout_ns = Some(5_000_000_000);
    let _g = Rules::set(P + 7, r);
    let s = subj(P + 7);
    let cam = Access::Device { class: DeviceClass::Camera, name: "video0" };
    let h = answer_from_thread(s.program, Decision::Allow, false)?;
    kassert!(check(&s, &cam).is_ok(), "answered allow");
    h.join();
    let h = answer_from_thread(s.program, Decision::Deny, true)?;
    let e = check(&s, &cam);
    h.join();
    kassert!(e.as_ref().is_err_and(|e| e.reason.contains("Regel: nie")), "answered deny: {e:?}");
    // "always": the learned rule decides now, nothing is asked (no answerer runs, the deadline is 5 s).
    let t0 = ksync::now_ns().unwrap_or(0);
    kassert!(denies(&s, &cam));
    kassert!(ksync::now_ns().unwrap_or(0) - t0 < 1_000_000_000, "asked again after an always answer");
    kassert!(pending_asks().iter().all(|a| a.subject.program != s.program));
    Ok(())
}
ktest!("pm.ask.answer", Safe, ask_answered);

fn ask_timeout() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/pm-ask".to_string(), modes: modes::ALL, decision: Decision::Ask });
    r.ask_timeout_ns = Some(50_000_000);
    let _g = Rules::set(P + 8, r.clone());
    let s = subj(P + 8);
    let a = Access::Path { path: "/pm-ask/f", write: false, create: false };
    let t0 = ksync::now_ns().unwrap_or(0);
    kassert!(check(&s, &a).is_ok(), "timeout -> class default allow");
    kassert!(ksync::now_ns().unwrap_or(0) - t0 >= 50_000_000, "returned before the deadline");
    r.defaults[AccessClass::Path as usize] = Decision::Ask;
    set_rules(s.program, Some(r));
    let e = check(&s, &a);
    kassert!(e.as_ref().is_err_and(|e| e.reason.contains("keine Antwort")), "timeout with default ask -> deny: {e:?}");
    kassert!(pending_asks().iter().all(|a| a.subject.program != s.program), "request left behind");
    Ok(())
}
ktest!("pm.ask.timeout", Safe, ask_timeout);

/// A caller that cannot sleep (here: preemption off, as under a spin lock) gets the timeout decision at once.
fn ask_cannot_wait() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/pm-nowait".to_string(), modes: modes::ALL, decision: Decision::Ask });
    r.ask_timeout_ns = Some(5_000_000_000);
    let _g = Rules::set(P + 20, r.clone());
    let s = subj(P + 20);
    let a = Access::Path { path: "/pm-nowait/f", write: false, create: false };
    let t0 = ksync::now_ns().unwrap_or(0);
    let ok = {
        let _p = ksync::PreemptGuard::new();
        check(&s, &a).is_ok()
    };
    kassert!(ok, "class default allow at once");
    r.defaults[AccessClass::Path as usize] = Decision::Ask;
    set_rules(s.program, Some(r));
    let refused = {
        let _p = ksync::PreemptGuard::new();
        check(&s, &a).is_err()
    };
    kassert!(refused, "default ask -> deny at once");
    kassert!(ksync::now_ns().unwrap_or(0) - t0 < 1_000_000_000, "waited although it could not sleep");
    kassert!(pending_asks().iter().all(|x| x.subject.program != s.program), "queued a request it cannot wait for");
    Ok(())
}
ktest!("pm.ask.cannot_wait", Safe, ask_cannot_wait);

/// A dot-dot path never slips past a deny rule (callers should pass canonical paths; this is the backstop).
fn non_canonical() -> TestResult {
    let mut r = RuleSet::allow_all();
    r.paths.push(PathRule { prefix: "/pm-secret".to_string(), modes: modes::ALL, decision: Decision::Deny });
    let _g = Rules::set(P + 21, r);
    let s = subj(P + 21);
    for p in ["/pm-secret/x", "/tmp/../pm-secret/x", "//pm-secret/x", "pm-secret/x", ""] {
        kassert!(denies(&s, &Access::Path { path: p, write: false, create: false }), "{p:?} allowed");
    }
    kassert!(!denies(&s, &Access::Path { path: "/tmp/x", write: false, create: false }));
    Ok(())
}
ktest!("pm.class.path_canonical", Safe, non_canonical);

fn cache_invalidation() -> TestResult {
    let p = ProgramId(P + 9);
    let s = subj(P + 9);
    let c = Cache::new();
    let mic = Access::Device { class: DeviceClass::Microphone, name: "pcmC0D0c" };
    kassert!(check_cached(&c, &s, &mic).is_ok());
    let mut r = RuleSet::allow_all();
    r.defaults[AccessClass::Device as usize] = Decision::Deny;
    let g = Rules::set(p.0, r);
    kassert!(c.unconditional(p, AccessClass::Device) == Some(Decision::Deny), "cache kept the old answer");
    kassert!(check_cached(&c, &s, &mic).is_err());
    drop(g);
    kassert!(check_cached(&c, &s, &mic).is_ok(), "deny kept after the rules went");
    kassert!(c.unconditional(p, AccessClass::Device) == Some(Decision::Allow), "cache kept the deny");
    kassert!(c.generation() == generation_tag());
    Ok(())
}
ktest!("pm.cache.invalidate", Safe, cache_invalidation);

/// The cost of one check (reported, never judged in QEMU): fast path (no rule of the class anywhere, one
/// atomic load), the fast answer for vfs, the table path (read lock, lookup, three path rules), another
/// program while one has rules, and the per-process cache.
fn cost() -> TestResult {
    const N: u64 = 100_000;
    let a = Access::Path { path: "/home/georg/.config/BraveSoftware/Brave-Browser/Default/Preferences", write: false, create: false };
    let time = |f: &dyn Fn()| {
        let t0 = ksync::now_ns().unwrap_or(0);
        for _ in 0..N {
            f();
        }
        (ksync::now_ns().unwrap_or(0) - t0) / N
    };
    let s = subj(P + 10);
    let fast_ok = table::all_allow(AccessClass::Path);
    let fast = time(&|| {
        let _ = core::hint::black_box(check(&s, &a));
    });
    let any = time(&|| {
        let _ = core::hint::black_box(allows_any(&s, AccessClass::Path));
    });
    let mut r = RuleSet::allow_all();
    for p in ["/home/georg/Dokumente", "/etc", "/home/georg/.ssh"] {
        r.paths.push(PathRule { prefix: p.to_string(), modes: modes::ALL, decision: Decision::Deny });
    }
    let _g = Rules::set(P + 11, r);
    let ruled = subj(P + 11);
    let table_ns = time(&|| {
        let _ = core::hint::black_box(check(&ruled, &a));
    });
    let other = time(&|| {
        let _ = core::hint::black_box(allows_any(&s, AccessClass::Path));
    });
    let c = Cache::new();
    let cached = time(&|| {
        let _ = core::hint::black_box(check_cached(&c, &s, &a));
    });
    klog::event!(Info, "pm", "check_cost", fast_path = fast_ok, fast_ns = fast, allows_any_ns = any, table_ns = table_ns, other_program_ns = other, cached_ns = cached);
    kassert!(check(&ruled, &a).is_ok());
    Ok(())
}
ktest!("pm.cost.check", Safe, cost);

/// QemuOnly: it pushes the real connection history out of the ring.
fn conn_ring() -> TestResult {
    let s = subj(P + 12);
    let keep = conn_keep();
    for port in 0..(keep as u32 + 10) {
        record_conn(ConnRecord { subject: s, kind: ConnKind::Opened, proto: NetProto::Udp, local_port: 1, remote: IpAddr::V4([10, 0, 2, 3]), remote_port: port as u16, bytes_in: 0, bytes_out: 0, time_ns: 0 });
    }
    let r = recent_connections(keep + 100);
    kassert!(r.len() == keep, "kept {} of {keep}", r.len());
    kassert!(r[0].remote_port == (keep as u32 + 9) as u16, "newest first");
    Ok(())
}
ktest!("pm.conn.ring", QemuOnly, conn_ring);
