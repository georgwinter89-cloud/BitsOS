//! Host tests of the global log (ring, readers, levels, sinks). They share klog's statics, so each test takes
//! `serial()` first. On the host every thread is its own "CPU" (`lock::cpu_tag`), so threads contend for the
//! ring lock as CPUs do. `serial()` plays klogd (`set_drainer(true)`); the early phase is tested on its own.

extern crate std;
use super::*;
use std::string::ToString;
use std::sync::{Mutex, MutexGuard};

static TEST_LOCK: Mutex<()> = Mutex::new(());
/// The ring right after the first `heap_ready`, before any test filled it.
static FIRST: std::sync::OnceLock<Vec<Record>> = std::sync::OnceLock::new();

/// The test lock, with rate limits off (the tests log thousands of records of one event on purpose; the
/// limiter has its own tests in `trace_tests`, which take `serial_limited`).
fn serial() -> (MutexGuard<'static, ()>, ratelimit::Suspend) {
    let g = serial_limited();
    (g, ratelimit::suspend())
}

pub(crate) fn serial_limited() -> MutexGuard<'static, ()> {
    let g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_serial(false);
    heap_ready();
    set_drainer(true);
    FIRST.get_or_init(|| read_since(0, 100));
    g
}

struct Collect {
    lines: Mutex<Vec<String>>,
    emergency: Mutex<Vec<String>>,
}
impl Collect {
    const fn new() -> Self {
        Collect { lines: Mutex::new(Vec::new()), emergency: Mutex::new(Vec::new()) }
    }
    fn take(&self) -> Vec<String> {
        core::mem::take(&mut *self.lines.lock().unwrap())
    }
}
impl Sink for Collect {
    fn write_line(&self, _l: Level, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }
    fn write_emergency(&self, _l: Level, line: &str) {
        self.emergency.lock().unwrap().push(line.to_string());
    }
}

fn named<'a>(recs: &'a [Record], name: &str) -> Option<&'a Record> {
    recs.iter().find(|r| r.name == name)
}

#[test]
fn events_render_fields_and_message() {
    let _g = serial();
    let start = next_seq();
    let name = String::from("igc0");
    event!(Info, "net", "link.up", ifname = name, speed_mbps = 1000u32, addr = Hex(0xfee0_0000));
    event!(Warn, "net", "dhcp.retry", attempt = 3u8; "no offer after {} s", 4);
    event!(Debug, "net", "hidden");
    let recs = read_since(start, 10);
    let lines: Vec<_> = recs.iter().map(|r| r.render()).collect();
    assert!(lines[0].ends_with("I net link.up ifname=igc0 speed_mbps=1000 addr=0xfee00000"), "{}", lines[0]);
    assert!(lines[1].ends_with("W net dhcp.retry attempt=3 | no offer after 4 s"), "{}", lines[1]);
    assert_eq!(recs.len(), 2, "debug is below the default level");
}

#[test]
fn early_records_move_into_the_heap_ring() {
    let _g = serial();
    let all = FIRST.get().unwrap();
    let early = named(all, "ring.early").expect("ring.early moved over");
    let ready = named(all, "ring.ready").expect("ring.ready kept");
    assert!(early.seq == 0 && early.seq < ready.seq, "{all:?}");
    let info = ring_info();
    assert!(!info.early && info.bytes >= RING_MIN, "{info:?}");
}

#[test]
fn try_event_under_a_held_ring_lock_never_waits() {
    let _g = serial();
    static EM: Collect = Collect::new();
    let id = add_deferred_sink("em-test", &EM, false);
    let start = next_seq();
    let lost = LOST.get();
    let stored = __with_ring_held(|| {
        let s = try_event!(Error, "rbtest", "nmi.path", cpu = 3u32; "stalled");
        // A normal event on the CPU that holds the ring (an interrupt) must not wait for itself either.
        event!(Warn, "rbtest", "irq.path");
        s
    });
    assert!(!stored);
    assert_eq!(LOST.get(), lost + 2);
    assert!(EM.emergency.lock().unwrap().iter().any(|l| l.contains("rbtest nmi.path cpu=3 | stalled")));
    assert!(try_event!(Error, "rbtest", "nmi.free"), "a free ring takes try_event");
    let page = read(start, 100, &Filter::ALL);
    assert_eq!(page.missed(), 2, "{:?}", page.gaps);
    assert_eq!(page.gaps[0], Gap { before: start + 2, count: 2 });
    assert!(named(&page.records, "nmi.free").is_some());
    remove_sink(id);
}

#[test]
fn filters_by_level_subsystem_and_name() {
    let _g = serial();
    let start = next_seq();
    event!(Error, "rbf.a", "x.one");
    event!(Warn, "rbf.a.b", "x.two");
    event!(Info, "rbf.ab", "y.three");
    event!(Info, "rbf.c", "x.four");
    let names = |f: Filter<'_>| read(start, 100, &f).records.iter().map(|r| r.name).collect::<Vec<_>>();
    assert_eq!(names(Filter { level: Some(Level::Warn), ..Filter::ALL }), ["x.one", "x.two"]);
    assert_eq!(names(Filter { subsystem: Some("rbf.a"), ..Filter::ALL }), ["x.one", "x.two"]);
    assert_eq!(names(Filter { name: Some("x."), ..Filter::ALL }), ["x.one", "x.two", "x.four"]);
    let tail = read_tail(2, &Filter { name: Some("x."), ..Filter::ALL });
    assert_eq!(tail.records.iter().map(|r| r.name).collect::<Vec<_>>(), ["x.two", "x.four"]);
    let p = read(start, 1, &Filter::ALL);
    assert_eq!((p.records.len(), p.next), (1, start + 1), "max and the follow-up sequence");
}

#[test]
fn wrap_reports_the_missed_records_to_readers_and_deferred_sinks() {
    let _g = serial();
    static SLOW: Collect = Collect::new();
    let id = add_deferred_sink("slow-test", &SLOW, false);
    resize(RING_MIN).unwrap();
    let start = next_seq();
    for i in 0..5000u32 {
        event!(Info, "rbwrap", "fill", i = i; "some text to fill the ring faster");
    }
    let page = read(start, usize::MAX, &Filter::ALL);
    assert!(page.missed() > 0 && page.gaps[0].before == page.records[0].seq, "{:?}", page.gaps);
    assert_eq!(page.records.len() as u64 + page.missed(), 5000);
    let mut lines = Vec::new();
    while pending() {
        assert!(drain_deferred(1000) > 0);
        lines.extend(SLOW.take());
    }
    assert!(lines[0].starts_with("[... ") && lines[0].ends_with(" records lost]"), "{}", lines[0]);
    assert!(lines.last().unwrap().contains("i=4999"));
    remove_sink(id);
    resize(RING_BYTES).unwrap();
}

#[test]
fn deferred_sinks_are_never_called_by_the_emitter() {
    let _g = serial();
    static D: Collect = Collect::new();
    static I: Collect = Collect::new();
    let d = add_deferred_sink("defer-test", &D, false);
    add_sink(&I);
    event!(Warn, "rbsink", "both");
    assert!(D.take().is_empty(), "deferred sink ran in the emitter");
    assert!(I.take().iter().any(|l| l.ends_with("W rbsink both")));
    drain_deferred(100);
    assert!(D.take().iter().any(|l| l.ends_with("W rbsink both")));
    assert_eq!(drain_deferred(100), 0, "nothing new");
    remove_sink(d);
}

#[test]
fn levels_per_subsystem() {
    let _g = serial();
    let start = next_seq();
    set_levels("info,rblv=debug").unwrap();
    assert_eq!(min_level(), Level::Debug);
    event!(Debug, "rblv.sub", "kept");
    event!(Debug, "rbother", "dropped");
    assert_eq!(set_levels("info,net=loud"), Err(String::from("net=loud")));
    set_levels("info").unwrap();
    assert_eq!(min_level(), Level::Info);
    event!(Debug, "rblv", "dropped.too");
    let recs = read(start, 100, &Filter::ALL).records;
    let names: Vec<_> = recs.iter().filter(|r| r.subsystem.starts_with("rb")).map(|r| r.name).collect();
    assert_eq!(names, ["kept"]);
    let mut cfg = std::collections::BTreeMap::new();
    cfg.insert("loglevel", "warn,rbcfg=trace");
    configure(&|k| cfg.get(k).map(|v| v.to_string()));
    assert_eq!(levels(), "warn,rbcfg=trace");
    set_levels("info").unwrap();
}

#[test]
fn reader_from_the_future_starts_over() {
    let _g = serial();
    let p = read(next_seq() + 1000, 10, &Filter::ALL);
    assert!(p.ahead && p.records.is_empty() && p.next == next_seq());
}

#[test]
fn long_lines_are_cut_and_marked() {
    let _g = serial();
    let start = next_seq();
    let long: String = "x".repeat(4000);
    event!(Info, "rbcut", "long", v = long);
    let r = &read(start, 1, &Filter::ALL).records[0];
    assert!(r.text.ends_with(" [cut]") && r.render().len() <= LINE_BYTES, "{}", r.text.len());
}

#[test]
fn sizes() {
    assert_eq!(parse_size("4M"), Some(4 << 20));
    assert_eq!(parse_size("512 KiB"), Some(512 << 10));
    assert_eq!(parse_size("1g"), Some(1 << 30));
    assert_eq!(parse_size("65536"), Some(65536));
    assert_eq!(parse_size("4X"), None);
    assert_eq!(parse_size("99999999999999999999G"), None);
    assert_eq!(ring_bytes_for_memory(24 << 30), 4 << 20);
    assert_eq!(ring_bytes_for_memory(32 << 30) % (64 << 10), 0);
    assert_eq!(ring_bytes_for_memory(2 << 30), 1 << 20);
    assert_eq!(ring_bytes_for_memory(1 << 40), 16 << 20);
}

#[test]
fn text_from_outside_cannot_break_the_line_or_fake_a_field() {
    let _g = serial();
    let start = next_seq();
    let ssid = "home\n[    9.000000] E net fake.record";
    let host = "a=b";
    let esc = "\u{1b}[2J";
    event!(Warn, "rbesc", "join", ssid = ssid, host = host, esc = esc, plain = "igc0"; "peer said {}", "x\r\ny");
    let r = &read(start, 1, &Filter::ALL).records[0];
    let line = r.render();
    assert!(!line.contains('\n') && !line.contains('\r') && !line.contains('\u{1b}'), "{line}");
    assert!(line.contains(r#"ssid="home\n[    9.000000] E net fake.record""#), "{line}");
    assert!(line.contains(r#"host="a=b""#) && line.contains(r#"esc="\u{1b}[2J""#), "{line}");
    assert!(line.contains(" plain=igc0 ") && line.ends_with(r"| peer said x\r\ny"), "{line}");
}

#[test]
fn threads_logging_at_once_get_unique_sequence_numbers() {
    let _g = serial();
    const THREADS: u64 = 8;
    const EACH: u64 = 2000;
    resize(RING_BYTES).unwrap();
    let start = next_seq();
    let (lost0, rec0) = (LOST.get(), RECORDS.get());
    let hs: Vec<_> = (0..THREADS)
        .map(|t| std::thread::spawn(move || {
            for i in 0..EACH {
                event!(Info, "rbmt", "e", t = t, i = i);
            }
        }))
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(next_seq() - start, THREADS * EACH);
    let (lost, stored) = (LOST.get() - lost0, RECORDS.get() - rec0);
    assert_eq!(lost + stored, THREADS * EACH, "every event either stored or counted as lost");
    let page = read(start, usize::MAX, &Filter { subsystem: Some("rbmt"), ..Filter::ALL });
    let seqs: Vec<u64> = page.records.iter().map(|r| r.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "sorted, no duplicates");
    assert_eq!(seqs.len() as u64 + page.missed(), THREADS * EACH);
    // Per thread, its own events come in its own order.
    for t in 0..THREADS {
        let mine: Vec<u64> = page
            .records
            .iter()
            .filter(|r| r.text.starts_with(&std::format!("t={t} ")))
            .map(|r| r.text.rsplit('=').next().unwrap().parse().unwrap())
            .collect();
        assert!(mine.windows(2).all(|w| w[0] < w[1]), "thread {t} out of order");
    }
}

#[test]
fn a_held_ring_makes_another_cpu_wait_bounded_then_drop_the_record() {
    let _g = serial();
    let start = next_seq();
    let lost = LOST.get();
    let t0 = std::time::Instant::now();
    __with_ring_held(|| {
        // Another "CPU" finds the ring taken by a holder that does not let go: it gives up after its bound.
        std::thread::spawn(|| event!(Warn, "rbheld", "waited")).join().unwrap();
    });
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "bounded");
    assert_eq!(LOST.get(), lost + 1);
    let page = read(start, 10, &Filter::ALL);
    assert_eq!(page.gaps.first().map(|g| g.before), Some(start + 1), "{:?}", page.gaps);
    // A reader on another CPU gets `busy` instead of hanging.
    let busy = __with_ring_held(|| std::thread::spawn(|| read(0, 10, &Filter::ALL)).join().unwrap());
    assert!(busy.busy && busy.records.is_empty() && busy.next == 0, "{busy:?}");
    assert!(!read(start, 10, &Filter::ALL).busy);
}

#[test]
fn before_klogd_the_emitter_feeds_deferred_sinks_in_order() {
    let _g = serial();
    static EARLY: Collect = Collect::new();
    event!(Warn, "rbearly", "before.sink");
    set_drainer(false);
    let id = add_deferred_sink("early-test", &EARLY, true);
    let replayed = EARLY.take();
    assert!(replayed.iter().any(|l| l.ends_with("W rbearly before.sink")), "replay at registration");
    // INT-A1: under parallel tests the shared ring may have wrapped; then the replay starts with the gap marker.
    assert!(replayed[0].contains("klog ring.early") || replayed[0].contains("records lost"), "replay starts at the oldest record: {}", replayed[0]);
    event!(Warn, "rbearly", "inline");
    assert!(EARLY.take().last().is_some_and(|l| l.ends_with("W rbearly inline")), "fed by the emitter");
    assert_eq!(drain_deferred(100), 0, "nothing twice");
    set_drainer(true);
    event!(Warn, "rbearly", "deferred");
    assert!(EARLY.take().is_empty(), "with a drainer the emitter leaves it alone");
    drain_deferred(100);
    let lines = EARLY.take();
    assert!(lines.iter().any(|l| l.ends_with("W rbearly deferred")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.ends_with("W rbearly inline")), "no line twice: {lines:?}");
    remove_sink(id);
}

struct Mirror(Mutex<Vec<String>>);
impl Sink for Mirror {
    fn write_line(&self, _l: Level, line: &str) {
        self.0.lock().unwrap().push(line.to_string());
    }
    fn lost(&self, records: u64) {
        self.0.lock().unwrap().push(std::format!("<lost {records}>"));
    }
}

#[test]
fn a_sink_can_mark_lost_records_its_own_way() {
    let _g = serial();
    static M: Mirror = Mirror(Mutex::new(Vec::new()));
    let id = add_deferred_sink("mirror-test", &M, false);
    __with_ring_held(|| try_event!(Error, "rbmirror", "lost"));
    event!(Warn, "rbmirror", "after");
    drain_deferred(100);
    let got: Vec<String> = core::mem::take(&mut *M.0.lock().unwrap()).into_iter().filter(|l| !l.contains("sink.added")).collect();
    assert_eq!(got.first().map(String::as_str), Some("<lost 1>"), "{got:?}");
    assert!(got.get(1).is_some_and(|l| l.ends_with("W rbmirror after")), "{got:?}");
    remove_sink(id);
}

#[test]
fn resize_in_chunks_keeps_every_record_in_order() {
    let _g = serial();
    resize(RING_BYTES).unwrap();
    let start = next_seq();
    for i in 0..3000u32 {
        event!(Info, "rbresize", "fill", i = i; "text that wraps the store {}", i);
    }
    let before = read(start, usize::MAX, &Filter::ALL);
    resize(RING_BYTES * 2).unwrap();
    let after = read(start, usize::MAX, &Filter::ALL);
    assert_eq!(before.records.len(), 3000);
    assert_eq!(after.records.len(), 3000, "{:?}", after.gaps);
    assert!(before.records.iter().zip(&after.records).all(|(a, b)| a.seq == b.seq && a.text == b.text));
    assert_eq!(ring_info().bytes, ring_size(RING_BYTES * 2));
    resize(RING_BYTES).unwrap();
}

#[test]
fn the_crash_tail_has_the_newest_whole_lines_oldest_first() {
    let _g = serial();
    event!(Warn, "rbtail", "first", n = 1u32);
    event!(Warn, "rbtail", "second", n = 2u32; "with a message");
    event!(Warn, "rbtail", "third");
    let mut big = [0u8; 4096];
    let n = try_tail_text(&mut big).expect("ring free");
    let s = core::str::from_utf8(&big[..n]).unwrap();
    assert!(s.ends_with("W rbtail third\n"), "newest last, no trailing space without text: {s}");
    let a = s.find("W rbtail first n=1\n").expect("first");
    let b = s.find("W rbtail second n=2 | with a message\n").expect("second");
    assert!(a < b);
    assert!(s.starts_with('['), "whole lines only");
    // A small buffer keeps only whole lines from the end; nothing fits in nothing.
    let mut small = [0u8; 40];
    let n = try_tail_text(&mut small).expect("ring free");
    assert!(n == 0 || (small[0] == b'[' && small[n - 1] == b'\n'));
    assert_eq!(try_tail_text(&mut []), Some(0));
    // Only what came before a sequence number (the panic path's start).
    let cut = next_seq();
    event!(Warn, "rbtail", "after");
    let n = try_tail_text_before(&mut big, cut).expect("ring free");
    let s = core::str::from_utf8(&big[..n]).unwrap();
    assert!(s.ends_with("W rbtail third\n") && !s.contains("rbtail after"), "{s}");
    // The panic path never waits for a busy ring.
    assert_eq!(__with_ring_held(|| std::thread::spawn(|| try_tail_text(&mut [0u8; 64])).join().unwrap()), None);
}

/// RB-klog: before klogd, emitters on many CPUs feed a deferred sink from the ring, in order and each record
/// once. The old per-emitter feeding let a later record overtake an earlier one: the earlier one was dropped and
/// a false `[... 1 records lost]` came instead (the Mini-PC console right after boot).
#[test]
fn before_klogd_many_cpus_feed_a_deferred_sink_in_order_without_false_losses() {
    let _g = serial();
    static STORM: Collect = Collect::new();
    resize(RING_BYTES).unwrap();
    set_drainer(false);
    let id = add_deferred_sink("storm-test", &STORM, false);
    let start = next_seq();
    const THREADS: u64 = 8;
    const EACH: u64 = 500;
    let hs: Vec<_> = (0..THREADS)
        .map(|t| std::thread::spawn(move || {
            for i in 0..EACH {
                event!(Info, "rbstorm", "e", t = t, i = i);
            }
        }))
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // What the last feeder left (its rounds are bounded) goes out in the same order.
    drain_deferred(usize::MAX);
    set_drainer(true);
    remove_sink(id);
    let all = STORM.take();
    assert!(!all.iter().any(|l| l.contains("records lost")), "{all:?}");
    let lines: Vec<String> = all.into_iter().filter(|l| l.contains("rbstorm")).collect();
    let ring = read(start, usize::MAX, &Filter { subsystem: Some("rbstorm"), ..Filter::ALL });
    assert_eq!(ring.missed(), 0, "nothing lost below the ring size: {:?}", ring.gaps);
    assert_eq!(ring.records.len() as u64, THREADS * EACH);
    let want: Vec<String> = ring.records.iter().map(|r| r.render()).collect();
    assert_eq!(lines.len(), want.len(), "every record once");
    assert!(lines == want, "in sequence order");
}

/// The reader copies small batches: a narrow filter over many records and a small `max` still give every match
/// once, in order, across batch and scan borders, with `next` right after the last one taken.
#[test]
fn batched_reads_keep_order_across_borders() {
    let _g = serial();
    resize(RING_BYTES).unwrap();
    let start = next_seq();
    for i in 0..3000u32 {
        if i % 97 == 0 {
            event!(Warn, "rbbatch", "hit", i = i);
        } else {
            event!(Info, "rbbatch", "miss", i = i; "{}", "x".repeat((i % 700) as usize));
        }
    }
    let f = Filter { name: Some("hit"), ..Filter::ALL };
    let mut got = Vec::new();
    let mut since = start;
    loop {
        let p = read(since, 3, &f);
        assert!(!p.busy);
        if p.records.is_empty() {
            break;
        }
        got.extend(p.records.iter().map(|r| r.text.clone()));
        since = p.next;
    }
    let want: Vec<String> = (0..3000u32).filter(|i| i % 97 == 0).map(|i| std::format!("i={i}")).collect();
    assert_eq!(got, want);
    let all = read(start, usize::MAX, &Filter { subsystem: Some("rbbatch"), ..Filter::ALL });
    assert_eq!(all.records.len(), 3000);
    for (i, r) in all.records.iter().enumerate() {
        let n = if r.name == "miss" { i % 700 } else { 0 };
        assert!(r.text.starts_with(&std::format!("i={i}")) && r.text.matches('x').count() == n, "text whole: {}", r.text);
    }
    let tail = read_tail(40, &Filter { subsystem: Some("rbbatch"), ..Filter::ALL });
    assert_eq!(tail.records.len(), 40);
    assert!(tail.records.windows(2).all(|w| w[0].seq < w[1].seq) && tail.records[39].text.starts_with("i=2999"));
}

/// Serial is printed from the ring by one CPU at a time: after a storm from many threads nothing is left for
/// serial, and turning serial on later does not dump the old records.
#[test]
fn the_serial_printer_catches_up_after_a_storm() {
    let _g = serial();
    serial::__reset();
    set_serial(true);
    set_drainer(false);
    let hs: Vec<_> = (0..8u32)
        .map(|t| std::thread::spawn(move || {
            for i in 0..300u32 {
                event!(Info, "rbserial", "e", t = t, i = i);
            }
        }))
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    // Before klogd every printer prints until nothing is left.
    event!(Info, "rbserial", "last");
    assert!(!serial_pending(), "the printer left records behind");
    // With klogd an emitter prints a bounded share; `flush_serial` (klogd) takes the rest.
    set_drainer(true);
    event!(Info, "rbserial", "with.klogd");
    flush_serial(usize::MAX);
    assert!(!serial_pending());
    set_serial(false);
    event!(Info, "rbserial", "off");
    assert!(!serial_pending(), "serial off: nothing pending");
    set_serial(true);
    assert!(!serial_pending(), "records from while it was off are not printed later");
    set_serial(false);
}

/// The panic path's catch-up of serial stops at its time budget (each line may pay hal's whole LINE wait when a
/// stopped CPU holds it), says so in one lock-free record, and leaves the rest pending; without a clock only the
/// line count bounds it.
#[test]
fn the_emergency_serial_flush_keeps_its_time_budget() {
    let _g = serial();
    serial::__reset();
    set_serial(true);
    let seq0 = next_seq();
    // Stored and printed as usual; then serial owes them all again, as after a printer stopped by the panic NMI.
    for i in 0..50u32 {
        event!(Info, "rbemerg", "e", i = i);
    }
    serial::__rewind(seq0);
    // A clock that moves 7 ms each time it is read: lines at 0, 7 and 14 ms, then the 20 ms budget is spent.
    static T: AtomicU64 = AtomicU64::new(0);
    T.store(0, Ordering::SeqCst);
    fn clock() -> u64 {
        T.fetch_add(7_000_000, Ordering::SeqCst) + 7_000_000
    }
    let n = serial::flush_emergency_within(256, 20_000_000, clock);
    assert_eq!(n, 3, "lines printed within the budget");
    assert!(serial_pending(), "the rest is still owed to serial");
    let recs = read_since(seq0, usize::MAX);
    let cut = named(&recs, "serial.emergency_cut").expect("the cut is logged");
    assert!(cut.text.contains("printed=3") && cut.text.contains("left=47"), "{}", cut.text);
    // No clock (before `time` installs one): only the line count; the direct cut record is not printed again.
    assert_eq!(serial::flush_emergency_within(256, 20_000_000, || 0), 47);
    assert!(!serial_pending());
    // The line count still bounds it.
    for i in 0..5u32 {
        event!(Info, "rbemerg", "f", i = i);
    }
    serial::__rewind(next_seq() - 5);
    assert_eq!(serial::flush_emergency_within(2, 20_000_000, || 0), 2);
    assert_eq!(serial::flush_emergency_within(256, 20_000_000, || 0), 3);
    set_serial(false);
}

/// The ring lock keeps its longest wait and its timeouts (the `log` tool and the -smp 16 ktest read them).
#[test]
fn lock_stats_see_waits_and_timeouts() {
    let _g = serial();
    install_clock(|| {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64 + 1
    });
    reset_lock_stats();
    __with_ring_held(|| std::thread::spawn(|| event!(Warn, "rbstats", "dropped")).join().unwrap());
    let s = lock_stats();
    assert!(s.timeouts >= 1, "{s:?}");
    let busy = std::sync::Arc::new(AtomicBool::new(false));
    let b2 = busy.clone();
    let h = std::thread::spawn(move || {
        __with_ring_held(|| {
            b2.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(2));
        })
    });
    while !busy.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    event!(Warn, "rbstats", "waited");
    h.join().unwrap();
    let s = lock_stats();
    assert!(s.wait_max_ns > 0 && s.hold_max_ns >= 1_000_000, "{s:?}");
    assert_eq!(ring_info().lock.timeouts, lock_stats().timeouts);
    reset_lock_stats();
    assert_eq!(lock_stats(), LockStats::default());
}
