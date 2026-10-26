//! Host tests of RB-7a: trace rings, keys, rate limits, field filters.

extern crate std;
use super::*;
use crate::tests::serial_limited;
use core::sync::atomic::AtomicU64;
use std::sync::Arc;
use trace::{Kind, Limits, Ring};

fn host_clock() {
    install_clock(|| {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64 + 1
    });
}

#[test]
fn a_ring_wraps_and_keeps_the_newest_entries() {
    let r = Ring::new(3, 8);
    for i in 0..20u64 {
        r.push(100 + i, Kind::Mark, 7, 0, i, i * 2);
    }
    assert_eq!(r.span(), (12, 20));
    assert_eq!(r.overwritten(), 12);
    assert!(r.get(5).is_none(), "overwritten entries are not shown");
    let mut seen = Vec::new();
    assert_eq!(r.scan(false, |e| { seen.push(e.a); true }), 0);
    assert_eq!(seen, (12..20).collect::<Vec<_>>());
    let e = r.get(19).unwrap();
    assert_eq!((e.cpu, e.pos, e.time_ns, e.kind, e.thread, e.b), (3, 19, 119, Kind::Mark, 7, 38));
    let mut newest = Vec::new();
    r.scan(true, |e| { newest.push(e.a); newest.len() < 3 });
    assert_eq!(newest, [19, 18, 17]);
}

#[test]
fn writers_on_many_threads_never_show_a_torn_entry() {
    let r = Arc::new(Ring::new(0, 64));
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (r, stop) = (r.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut checked = 0u64;
            while !stop.load(Ordering::Relaxed) {
                r.scan(false, |e| {
                    assert_eq!(e.b, e.a ^ (e.thread << 40), "torn entry {e:?}");
                    assert_eq!(e.time_ns, e.a);
                    checked += 1;
                    true
                });
            }
            checked
        })
    };
    let writers: Vec<_> = (0..4u64)
        .map(|t| {
            let r = r.clone();
            std::thread::spawn(move || {
                for i in 0..50_000u64 {
                    r.push(i, Kind::Mark, t, 0, i, i ^ (t << 40));
                }
            })
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    assert!(reader.join().unwrap() > 0);
    assert_eq!(r.span().1, 200_000);
}

#[test]
fn gcra_lets_a_burst_through_then_the_rate() {
    let tat = AtomicU64::new(0);
    // burst 5, one per 10 ns
    let pass = |now| ratelimit::admit_at(&tat, now, 10, 40);
    assert_eq!((0..8).filter(|_| pass(1000)).count(), 5);
    assert!(pass(1010));
    assert!(!pass(1010));
    assert_eq!((0..8).filter(|_| pass(2000)).count(), 5, "a quiet period refills the burst");
}

#[test]
fn ratelimit_settings_parse() {
    assert_eq!(ratelimit::parse("1000/100"), Some((1000, 100)));
    assert_eq!(ratelimit::parse(" 20 / 5/s"), Some((20, 5)));
    assert_eq!(ratelimit::parse("off"), Some((ratelimit::DEFAULT_BURST, 0)));
    assert_eq!(ratelimit::parse("0/5"), None);
    assert_eq!(ratelimit::parse("x"), None);
}

#[test]
fn a_storm_of_one_event_is_cut_to_the_burst_and_reported() {
    let _g = serial_limited();
    host_clock();
    let before = ratelimit::get();
    ratelimit::set(5, 1);
    let start = next_seq();
    for i in 0..50u32 {
        event!(Warn, "rbrl", "flood", i = i);
    }
    event!(Warn, "rbrl", "other");
    let kept = read(start, usize::MAX, &Filter { subsystem: Some("rbrl"), ..Filter::ALL });
    let floods = kept.records.iter().filter(|r| r.name == "flood").count();
    assert!((5..=6).contains(&floods), "{floods} kept");
    assert!(kept.records.iter().any(|r| r.name == "other"), "another event has its own budget");
    assert_eq!(kept.missed(), 0, "suppressed records leave no gap");
    let s0 = next_seq();
    assert!(ratelimit::sweep() >= 44);
    let rep = read(s0, usize::MAX, &Filter { subsystem: Some("klog"), name: Some("ratelimit."), ..Filter::ALL });
    let line = rep.records.iter().find(|r| r.text.contains("subsystem=rbrl")).expect("summary").render();
    assert!(line.contains("event=flood count=4"), "{line}");
    assert_eq!(ratelimit::sweep(), 0, "counts are cleared");
    ratelimit::set(before.0, before.1);
}

#[test]
fn keys_are_stable_and_name_themselves() {
    let (a, _) = keys::lookup("rbk", "one");
    let (b, _) = keys::lookup("rbk", "two");
    assert_ne!(a, b);
    assert_eq!(keys::lookup("rbk", "one").0, a);
    assert_eq!(keys::get(a).and_then(|k| k.names()), Some(("rbk", "one")));
    let (used, cap) = keys::usage();
    assert!(used >= 2 && cap >= 512);
}

#[test]
fn field_filters() {
    let text = r#"dev=eth0 n=42 addr=0x1000 name="a b" | message x=1"#;
    let m = |spec: &str| FieldFilter::parse(spec).unwrap().matches(text);
    assert!(m("dev=eth0"));
    assert!(m("n=42 n>41 n<=42 n>=0x2a"));
    assert!(!m("n>42"));
    assert!(m("addr=4096") && m("addr>=0x1000"));
    assert!(m("name"));
    assert!(m("dev!=eth1") && m("missing!=1"));
    assert!(!m("x=1"), "the message is not a field");
    assert!(!m("missing"));
    assert!(m(""));
    assert_eq!(FieldFilter::parse("n>abc"), Err(String::from("n>abc")));
    assert_eq!(FieldFilter::parse("=3"), Err(String::from("=3")));
    assert_eq!(FieldFilter::parse("a!b"), Err(String::from("a!b")));
    assert!(FieldFilter::parse("b=").is_err());
    assert_eq!(fields::value_of("| msg a=1", "a"), None);
    assert_eq!(fields::value_of(r#"s="x \" y" t=2"#, "t"), Some("2"));
}

#[test]
fn hooks_record_into_this_cpus_ring() {
    let _g = serial_limited();
    host_clock();
    trace::init(1 << 30);
    let r = trace::ring(0).expect("ring of CPU 0");
    assert!(r.capacity() >= (64 << 10) as u64 / trace::ENTRY_BYTES as u64 / 2);
    let lim = trace::limits();
    trace::set_limits(Limits { syscall_ns: 1, faults_per_window: 3, ..lim });
    // The host clock starts near 0 at its first call; a start time of 0 means "not measured".
    while now() < 10_000 {
        std::hint::spin_loop();
    }
    let (_, before) = r.span();
    trace::mark("rbtr", "hit", 1, 2);
    trace::syscall_done(202, now().saturating_sub(5000), -11, false);
    trace::syscall_done(7, now().saturating_sub(5000), 0, true);
    for i in 0..5 {
        trace::page_fault(0x1000 * i);
    }
    trace::lock_wait("rb.lock", 0x40, 1_000_000); // no TSC rate yet: not recorded
    trace::set_limits(lim);
    let mut got = Vec::new();
    r.scan(false, |e| {
        if e.pos >= before {
            got.push(*e);
        }
        true
    });
    let mark = got.iter().find(|e| e.kind == Kind::Mark).expect("mark");
    assert_eq!((mark.subsystem(), mark.name(), mark.level()), ("rbtr", "hit", Level::Debug));
    assert!(mark.text().starts_with("a=1 b=2 cpu=0 thread="), "{}", mark.text());
    let sys: Vec<_> = got.iter().filter(|e| e.kind == Kind::Syscall).collect();
    assert_eq!(sys.len(), 1, "a blocked call is not slow");
    assert!(sys[0].text().starts_with("nr=202 ret=-11 us="), "{}", sys[0].text());
    assert_eq!(got.iter().filter(|e| e.kind == Kind::Fault).count(), 1, "one entry per burst window");
    assert!(!got.iter().any(|e| e.kind == Kind::LockWait));
}

#[test]
fn trace_kinds_parse() {
    assert_eq!(trace::parse_kinds("all"), Ok(u32::MAX));
    assert_eq!(trace::parse_kinds("off"), Ok(0));
    assert_eq!(trace::parse_kinds("irq, fault"), Ok(0b10010));
    assert_eq!(trace::parse_kinds("irq,bogus"), Err(String::from("bogus")));
    assert_eq!(trace::bytes_for_memory(32 << 30, 16), 512 << 10);
    assert_eq!(trace::bytes_for_memory(1 << 30, 64), 64 << 10);
}

/// A writer stalled between reserving and publishing while the ring wraps past it: neither it nor the later
/// writer of the same slot can publish a mix of both (checker finding, RB-7a fix).
#[test]
fn a_slot_has_one_writer_at_a_time() {
    let w = AtomicU64::new(0);
    let stamp = |pos: u64| (pos + 1) << 1;
    assert!(trace::claim(&w, stamp(0)), "a fresh slot");
    assert!(!trace::claim(&w, stamp(8)), "the wrapping writer finds it in writing and drops its entry");
    w.store(stamp(0), core::sync::atomic::Ordering::Release);
    assert!(trace::claim(&w, stamp(8)), "published: the newer entry may take it");
    w.store(stamp(8), core::sync::atomic::Ordering::Release);
    assert!(!trace::claim(&w, stamp(0)), "the stalled writer comes back to a newer entry and drops its own");
    assert!(!trace::claim(&w, stamp(8)), "never the same entry twice");
    // A whole ring of two slots written over many times stays readable.
    let r = Ring::new(0, 2);
    for i in 0..10u64 {
        assert!(r.push(i, Kind::Mark, 0, 0, i, !i));
    }
    assert_eq!(r.get(9).map(|e| (e.a, e.b)), Some((9, !9)));
}

/// Switches are off by default (they would push the rare entries out of the ring); the rest is on.
#[test]
fn switch_is_off_by_default() {
    let d = trace::DEFAULT_KINDS;
    assert_eq!(d & 1, 0);
    assert_eq!(trace::parse_kinds("irq,syscall,lock,fault,mark").map(|m| m & d == m), Ok(true));
}

/// More distinct keys than the tables hold, before klogd grows them: every lookup answers (the shared overflow
/// slot when nothing is free, named without any initialisation), nothing panics; after `grow` new keys find
/// slots in the new table, and the sweep reports suppressed records from both tables and the overflow slot.
#[test]
fn keys_overflow_then_grow() {
    let _g = serial_limited();
    static TEXT: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz";
    let c = keys::Chain::leaked(16);
    let key = |i: usize| (&TEXT[i..i + 1], "n");
    let mut overflowed = 0;
    for i in 0..64 {
        let (sub, name) = key(i);
        let (id, k) = c.lookup(sub, name);
        if id == keys::OVERFLOW_ID {
            overflowed += 1;
            assert!(core::ptr::eq(k, &keys::OVERFLOW));
            assert_eq!(k.names(), Some(("klog", "keys.overflow")));
        } else {
            assert_eq!(k.names(), Some((sub, name)));
            assert!(core::ptr::eq(c.get(id).unwrap(), k));
        }
    }
    let (used, cap) = c.usage();
    assert_eq!((used, cap), (16, 16), "the first table fills, nothing beyond it");
    assert_eq!(overflowed, 48);
    assert_eq!(c.grow(), Some((16, 48)), "a table of twice the size");
    assert_eq!(c.grow(), None, "16 of 48 used: not half full");
    let mut placed = 0;
    for i in 16..40 {
        let (sub, name) = key(i);
        let (id, k) = c.lookup(sub, name);
        if id != keys::OVERFLOW_ID {
            placed += 1;
            assert!(id >= 16 && core::ptr::eq(c.get(id).unwrap(), k));
            k.suppressed.store(2, core::sync::atomic::Ordering::Relaxed);
        }
    }
    assert!(placed >= 16, "{placed} of 24 new keys placed in the new table");
    let (first, _) = c.lookup(key(0).0, key(0).1);
    c.get(first).unwrap().suppressed.store(5, core::sync::atomic::Ordering::Relaxed);
    let over_before = keys::OVERFLOW.suppressed.swap(3, core::sync::atomic::Ordering::Relaxed);
    let mut total = 0;
    let mut slots = 0;
    c.for_each(|k| {
        slots += 1;
        total += ratelimit::report(k);
    });
    assert_eq!(slots, 16 + placed + 1, "claimed slots of both tables, then the overflow slot");
    assert_eq!(total, 5 + 2 * placed as u64 + 3);
    let mut again = 0;
    c.for_each(|k| again += ratelimit::report(k));
    assert_eq!(again, 0, "reported once");
    keys::OVERFLOW.suppressed.fetch_add(over_before, core::sync::atomic::Ordering::Relaxed);
}

/// CONFIG keys of RB-7a reach the limits, the kinds and the rate limiter.
#[test]
fn config_keys_take_effect() {
    let _g = serial_limited();
    let (lim, kinds, rl) = (trace::limits(), trace::kinds(), ratelimit::get());
    let get = |k: &str| -> Option<String> {
        Some(String::from(match k {
            "trace_syscall_ms" => "5",
            "trace_lock_us" => "7",
            "trace_irq_rate" => "900",
            "trace_faults" => "11",
            "trace" => "irq,fault",
            "log_ratelimit" => "50/5",
            _ => return None,
        }))
    };
    config::configure(&get);
    let got = (trace::limits(), trace::kinds(), ratelimit::get());
    trace::set_limits(lim);
    trace::set_kinds(&kinds).unwrap();
    ratelimit::set(rl.0, rl.1);
    assert_eq!(got.0, Limits { syscall_ns: 5_000_000, lock_wait_ns: 7_000, irq_per_s: 900, faults_per_window: 11 });
    assert_eq!(got.1, "irq,fault");
    assert_eq!(got.2, (50, 5));
}
