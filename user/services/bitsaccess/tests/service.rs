//! DB2: bitsaccess on the host: the kernel's lines, the store's days and first-time marks, retention, the
//! socket's answers and who gets them, the settings.

use bitsaccess::kernel;
use bitsaccess::model::{self, Entry};
use bitsaccess::query;
use bitsaccess::serve::{self, State};
use bitsaccess::settings::{self, Settings};
use bitsaccess::store::{self, Store};
use mcpreg::Json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const DAY: u64 = 86_400_000;
/// 2026-11-14 12:00 UTC.
const NOW: u64 = 1_794_657_600_000;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitsaccess-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// What the kernel hands out for one program run (vfs::access::drain's format).
const BATCH: &str = r#"{"p":300,"start":5000000000,"uid":1000,"exe":"/apps/brave/brave"}
{"p":301,"start":6000000000,"uid":0,"exe":"/bin/bitspkgd"}
{"pid":300,"start":5000000000,"k":"read","dir":"/data/home/1000/Briefe","n":4,"t0":9000000000,"t1":9500000000}
{"pid":300,"start":5000000000,"k":"create","dir":"/data/home/1000/Downloads","name":"bild.png","n":1,"t0":9100000000,"t1":9100000000}
{"pid":300,"start":5000000000,"k":"device","path":"/dev/snd/pcmC0D0c","w":false,"n":1,"t0":9200000000,"t1":9200000000}
{"pid":300,"start":5000000000,"k":"device","path":"/dev/dri/renderD128","w":true,"n":3,"t0":9200000000,"t1":9300000000}
{"pid":300,"start":5000000000,"k":"connect","proto":"tcp","addr":"93.184.216.34","port":443,"n":2,"t0":9300000000,"t1":9400000000}
{"pid":300,"start":5000000000,"k":"netclose","proto":"tcp","addr":"93.184.216.34","port":443,"tx":1200,"rx":56000,"n":1,"t0":9900000000,"t1":9900000000}
{"pid":301,"start":6000000000,"k":"connect","proto":"tcp","addr":"192.168.100.1","port":80,"n":1,"t0":9300000000,"t1":9300000000}
{"pid":300,"start":5000000000,"k":"caps","caps":4096,"n":1,"t0":9400000000,"t1":9400000000}
{"s":1,"on":true,"lost":7,"cut":0,"now":10000000000}
"#;

fn roots() -> Vec<String> {
    vec!["/data/home/1000/Briefe".into()]
}

#[test]
fn kernel_lines_become_entries_with_wall_time_and_program() {
    assert!(kernel::complete(BATCH) && !kernel::complete("{\"p\":1}\n"));
    let b = kernel::parse(BATCH, NOW);
    assert!(b.complete && b.on);
    assert_eq!(b.lost, 7);
    assert_eq!(b.entries.len(), 8);
    let read = &b.entries[0];
    assert_eq!((read.exe.as_str(), read.kind.as_str(), read.what.as_str(), read.n, read.uid), ("/apps/brave/brave", "read", "/data/home/1000/Briefe", 4, 1000));
    // 1 s before the status line's `now` is 1 s before the wall clock of the read.
    assert_eq!((read.t0, read.t1), (NOW - 1000, NOW - 500));
    let close = b.entries.iter().find(|e| e.kind == "netclose").unwrap();
    assert_eq!((close.what.as_str(), close.port, close.tx, close.rx), ("93.184.216.34", 443, 1200, 56000));
    assert!(b.entries.iter().any(|e| e.kind == "device" && e.write && e.what == "/dev/dri/renderD128"));
    assert_eq!(b.entries.iter().find(|e| e.kind == "caps").unwrap().what, "4096");
}

#[test]
fn names_on_the_page() {
    assert_eq!(model::device_class("/dev/snd/pcmC0D0c"), "mikrofon");
    assert_eq!(model::device_class("/dev/snd/pcmC1D3p"), "ton");
    assert_eq!(model::device_class("/dev/video0"), "kamera");
    assert_eq!(model::device_class("/dev/input/event4"), "eingabe");
    assert_eq!(model::device_class("/dev/nvme0n1p2"), "laufwerk");
    assert_eq!(model::device_class("/dev/dri/renderD128"), "grafik");
    assert_eq!(model::device_class("/dev/bus/usb/001/004"), "usb");
    assert_eq!(model::device_class("/dev/bitsos/events"), "andere");
    assert_eq!(model::private_root("/data/home/1000/Briefe/2026", &roots()), Some("/data/home/1000/Briefe"));
    assert_eq!(model::private_root("/data/home/1000/Briefe-alt", &roots()), None);
    assert!(model::is_system("/bin/bitspkgd", 1000) && model::is_system("/apps/x", 0) && !model::is_system("/apps/brave/brave", 1000));
    assert_eq!(model::host_words("127.0.0.1"), "dieser Rechner");
    assert_eq!(model::host_words("192.168.100.1"), "192.168.100.1 (Heimnetz)");
    assert_eq!(model::port_words("tcp", 443), "Port 443, sicheres Web");
    assert_eq!(model::port_words("tcp", 8443), "Port 8443");
}

#[test]
fn first_batch_learns_later_new_things_are_marked_and_days_survive_a_restart() {
    let dir = tmp("store");
    let mut s = Store::open(&dir);
    s.add(kernel::parse(BATCH, NOW).entries, &roots(), NOW);
    assert!(s.since(0).all(|e| !e.new), "the first batch only learns");
    // Later: the microphone again (known), a camera and a new host (new), a private folder again (known).
    let later = |kind: &str, what: &str| Entry { t0: NOW + 10, t1: NOW + 20, exe: "/apps/brave/brave".into(), pid: 300, uid: 1000, kind: kind.into(), what: what.into(), n: 1, ..Entry::default() };
    s.add(vec![later("device", "/dev/snd/pcmC0D0c"), later("device", "/dev/video0"), later("connect", "203.0.113.9"), later("read", "/data/home/1000/Briefe/Steuer")], &roots(), NOW + 30);
    let new: Vec<String> = s.recent_since(NOW + 10).filter(|e| e.new).map(|e| e.what.clone()).collect();
    assert_eq!(new, vec!["/dev/video0".to_string(), "203.0.113.9".to_string()]);
    s.flush().unwrap();
    let again = Store::open(&dir);
    assert_eq!(again.since(0).count(), s.since(0).count());
    let mic: Vec<&Entry> = again.since(0).filter(|e| e.what == "/dev/snd/pcmC0D0c").collect();
    assert_eq!(mic.len(), 1, "a day keeps one line per thing");
    assert_eq!(mic[0].n, 2);
    assert!(again.since(0).any(|e| e.new && e.what == "/dev/video0"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn retention_by_days_then_by_size_keeps_today() {
    let dir = tmp("retain");
    let mut s = Store::open(&dir);
    let e = |t: u64| Entry { t0: t, t1: t, exe: "/apps/a".into(), kind: "read".into(), what: format!("/data/{t}"), n: 1, ..Entry::default() };
    s.add((0..40).map(|d| e(NOW - d * DAY)).collect(), &[], NOW);
    s.flush().unwrap();
    let gone = s.retain(NOW, 30, u64::MAX);
    assert_eq!(gone.len(), 9, "{gone:?}");
    assert_eq!(s.days.len(), 31);
    let gone = s.retain(NOW, 30, 1);
    assert_eq!(s.days.keys().collect::<Vec<_>>(), vec![&store::day_of(NOW)], "{gone:?}");
    assert_eq!(std::fs::read_dir(dir.join("days")).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn summary_groups_per_program_with_the_page_words() {
    let dir = tmp("summary");
    let mut s = Store::open(&dir);
    s.add(kernel::parse(BATCH, NOW).entries, &roots(), NOW);
    let j = query::summary(&s, NOW - DAY, None, &roots(), NOW);
    let progs = j.get("programs").and_then(Json::as_array).unwrap();
    assert_eq!(progs.len(), 2);
    // Brave's newest touch (the close at NOW - 100) is newer than bitspkgd's.
    let b = &progs[0];
    assert_eq!(b.get("name").and_then(Json::as_str), Some("brave"));
    assert_eq!(b.get("system").and_then(Json::as_bool), Some(false));
    let files = b.get("files").and_then(Json::as_array).unwrap();
    let briefe = files.iter().find(|f| f.get("folder").and_then(Json::as_str) == Some("/data/home/1000/Briefe")).unwrap();
    assert_eq!((briefe.get("private").and_then(Json::as_bool), briefe.get("read").and_then(Json::as_u64)), (Some(true), Some(4)));
    let net = b.get("net").and_then(Json::as_array).unwrap();
    assert_eq!(net.len(), 1);
    assert_eq!((net[0].get("connect").and_then(Json::as_u64), net[0].get("rx").and_then(Json::as_u64)), (Some(2), Some(56000)));
    let devs: Vec<&str> = b.get("devices").and_then(Json::as_array).unwrap().iter().filter_map(|d| d.get("label").and_then(Json::as_str)).collect();
    assert_eq!(devs, vec!["Mikrofon", "Grafik"]);
    assert_eq!(b.get("rights").and_then(Json::as_array).unwrap().len(), 1);
    assert_eq!(progs[1].get("system").and_then(Json::as_bool), Some(true));
    // One program by its file name; events newest first.
    let one = query::summary(&s, NOW - DAY, Some("bitspkgd"), &roots(), NOW);
    assert_eq!(one.get("programs").and_then(Json::as_array).unwrap().len(), 1);
    let ev = query::events(&s, NOW - 60_000, Some("brave"), Some("net"), 10, NOW);
    let ev = ev.get("events").and_then(Json::as_array).unwrap();
    assert_eq!(ev.iter().filter_map(|e| e.get("kind").and_then(Json::as_str)).collect::<Vec<_>>(), vec!["netclose", "connect"]);
    let _ = std::fs::remove_dir_all(dir);
}

fn shared(dir: &std::path::Path) -> serve::Shared {
    let mut store = Store::open(dir);
    store.add(kernel::parse(BATCH, bitsaccess::now_ms()).entries, &roots(), bitsaccess::now_ms());
    Arc::new(Mutex::new(State { store, settings: Settings { private_roots: roots(), ..Settings::default() }, known: true, reload: false }))
}

fn ask(state: &serve::Shared, uid: Option<u32>, req: &str) -> String {
    let (a, b) = UnixStream::pair().unwrap();
    let st = state.clone();
    let t = std::thread::spawn(move || serve::handle(b, &st, uid));
    let mut w = a.try_clone().unwrap();
    writeln!(w, "{req}").unwrap();
    w.shutdown(std::net::Shutdown::Write).unwrap();
    let mut line = String::new();
    BufReader::new(a).read_line(&mut line).unwrap();
    t.join().unwrap();
    line
}

#[test]
fn the_socket_answers_root_and_the_session_only() {
    let dir = tmp("sock");
    let st = shared(&dir);
    let ok = ask(&st, Some(1000), r#"{"id":7,"op":"summary","args":{"since_s":3600}}"#);
    let j = Json::parse(ok.trim()).unwrap();
    assert_eq!(j.get("id").and_then(Json::as_u64), Some(7));
    assert_eq!(j.get("result").and_then(|r| r.get("programs")).and_then(Json::as_array).map(|a| a.len()), Some(2));
    let ev = Json::parse(ask(&st, Some(0), r#"{"id":"x","op":"events","args":{"since_s":3600,"kind":"device","max":1}}"#).trim()).unwrap();
    let r = ev.get("result").unwrap();
    assert_eq!((r.get("events").and_then(Json::as_array).map(|a| a.len()), r.get("more").and_then(Json::as_u64)), (Some(1), Some(1)));
    for who in [Some(1001), Some(65534), None] {
        let no = ask(&st, who, r#"{"id":1,"op":"summary","args":{}}"#);
        assert!(no.contains("\"error\"") && !no.contains("programs"), "{who:?}: {no}");
    }
    assert!(ask(&st, Some(0), r#"{"id":2,"op":"delete"}"#).contains("unbekannte Anfrage"));
    let tool = serve::tool_answer(&st, &Json::obj(vec![("op", Json::str("summary")), ("app", Json::str("brave"))]));
    assert!(format!("{tool:?}").contains("brave"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn settings_register_as_settingsd_reads_them_and_ignore_drops_programs() {
    let j = settingsd::json::parse(&settings::register_request()).unwrap();
    let Some(settingsd::json::Json::Arr(keys)) = j.get("keys") else { panic!("no keys") };
    assert_eq!(keys.len(), 4);
    for k in keys {
        let d = settingsd::key::KeyDef::from_json(k, settings::OWNER).unwrap_or_else(|e| panic!("{e}: {}", k.to_text()));
        assert!(d.name.starts_with("access."));
    }
    let mut s = Settings::default();
    assert_eq!((s.enabled, s.keep_days, s.max_mb), (true, 30, 64));
    s.set("access.keep_days", &settingsd::json::Json::Int(9999));
    s.set("access.ignore", &settingsd::json::Json::Str("brave; /bin/bitspkgd".into()));
    s.set("access.enabled", &settingsd::json::Json::Bool(false));
    assert_eq!((s.keep_days, s.enabled), (365, false));
    let kept = serve::keep(kernel::parse(BATCH, NOW).entries, &s, 9);
    assert!(kept.is_empty(), "{kept:?}");
    let kept = serve::keep(kernel::parse(BATCH, NOW).entries, &Settings::default(), 300);
    assert!(kept.iter().all(|e| e.pid == 301), "its own pid's entries stay out");
    let (d, why) = settings::load("/nonexistent/settings.sock");
    assert_eq!(d, Settings::default());
    assert!(why.is_some());
}

#[test]
fn a_log_kept_in_memory_joins_the_disk_once_data_is_there() {
    let dir = tmp("late");
    let mut first = Store::open(&dir);
    first.add(kernel::parse(BATCH, NOW - DAY).entries, &roots(), NOW - DAY);
    first.flush().unwrap();
    let mut s = Store::in_memory(&dir);
    s.add(kernel::parse(BATCH, NOW).entries, &roots(), NOW);
    s.forget_before(NOW - DAY / 2);
    s.load_disk();
    assert_eq!(s.days.len(), 2, "yesterday from the disk, today from memory");
    s.flush().unwrap();
    assert_eq!(Store::open(&dir).since(0).count(), s.since(0).count());
    let _ = std::fs::remove_dir_all(dir);
}

/// The kernel's node as the reader sees it: one batch to read, the switch words written.
struct FakeNode {
    text: std::io::Cursor<Vec<u8>>,
    written: Vec<u8>,
    reads: usize,
}

impl FakeNode {
    fn new() -> FakeNode {
        FakeNode { text: std::io::Cursor::new(BATCH.as_bytes().to_vec()), written: Vec::new(), reads: 0 }
    }
}

impl std::io::Read for FakeNode {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        self.text.read(buf)
    }
}

impl Write for FakeNode {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn nothing_is_read_or_kept_before_settingsd_said_whether_the_log_is_on() {
    let dir = tmp("known");
    let state: serve::Shared = Arc::new(Mutex::new(State { store: Store::in_memory(&dir), ..State::default() }));
    let mut node = FakeNode::new();
    let mut was_on = None;
    // settingsd has not answered yet: the defaults say "on", but nothing is read and the kernel is left alone.
    assert!(!serve::round(&state, &mut node, 1, &mut was_on, std::time::Duration::from_secs(5)).unwrap());
    assert_eq!((node.reads, node.written.len()), (0, 0));
    // It answers: Georg switched the log off. The kernel is switched off, nothing is read or stored.
    {
        let mut s = state.lock().unwrap();
        s.settings.enabled = false;
        s.known = true;
    }
    assert!(!serve::round(&state, &mut node, 1, &mut was_on, std::time::Duration::from_secs(6)).unwrap());
    assert_eq!((node.reads, node.written.as_slice()), (0, &b"off\n"[..]));
    let s = state.lock().unwrap();
    assert_eq!((s.store.since(0).count(), s.store.recent.len(), s.store.on), (0, 0, false));
    drop(s);
    // Without an answer for SETTINGS_WAIT the defaults hold: on, and the batch is kept.
    let state: serve::Shared = Arc::new(Mutex::new(State { store: Store::in_memory(&dir), ..State::default() }));
    let (mut node, mut was_on) = (FakeNode::new(), None);
    assert!(serve::round(&state, &mut node, 1, &mut was_on, serve::SETTINGS_WAIT).unwrap());
    assert_eq!(node.written.as_slice(), &b"on\n"[..]);
    assert_eq!(state.lock().unwrap().store.recent.len(), 8);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn many_hosts_do_not_keep_a_program_new_and_old_marks_go() {
    let dir = tmp("hosts");
    let mut s = Store::open(&dir);
    s.add(kernel::parse(BATCH, NOW).entries, &roots(), NOW);
    let marks = s.marks();
    let host = |i: u64| Entry { t0: NOW + i, t1: NOW + i, exe: "/apps/brave/brave".into(), pid: 300, uid: 1000, kind: "connect".into(), what: format!("203.0.113.{i}"), proto: "tcp".into(), port: 443, n: 1, ..Entry::default() };
    s.add((1..=40).map(host).collect(), &roots(), NOW + 100);
    let new = s.recent_since(NOW + 1).filter(|e| e.new).count();
    // Brave already knew one host (from the first batch): nine more are new, the rest is neither marked nor kept.
    assert_eq!(new, store::NEW_HOSTS_MAX - 1);
    assert_eq!(s.marks(), marks + store::NEW_HOSTS_MAX - 1);
    s.flush().unwrap();
    assert_eq!(Store::open(&dir).marks(), s.marks(), "the marks survive a restart");
    // 31 days later every mark not touched since is forgotten.
    s.retain(NOW + 31 * DAY, 30, u64::MAX);
    assert_eq!(s.marks(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn starts_are_counted_and_only_quiet_shipped_programs_count_as_bitsos() {
    let dir = tmp("system");
    let mut s = Store::open(&dir);
    s.add(kernel::parse(BATCH, NOW).entries, &roots(), NOW);
    let e = |exe: &str, uid: u32, kind: &str, what: &str| Entry { t0: NOW + 5, t1: NOW + 5, exe: exe.into(), pid: 400, uid, kind: kind.into(), what: what.into(), n: 1, ..Entry::default() };
    s.add(
        vec![
            e("/bin/sh", 1000, "exec", "/bin/sh"),
            e("/bin/sh", 1000, "exec", "/bin/sh"),
            e("/home/anna/werkzeug", 0, "exec", "/home/anna/werkzeug"),
            e("/bin/bitsmon", 0, "caps", "4096"),
        ],
        &roots(),
        NOW + 10,
    );
    let j = query::summary(&s, NOW - DAY, None, &roots(), NOW + 10);
    let progs = j.get("programs").and_then(Json::as_array).unwrap();
    let of = |name: &str| progs.iter().find(|p| p.get("name").and_then(Json::as_str) == Some(name)).unwrap();
    let sys = |name: &str| of(name).get("system").and_then(Json::as_bool);
    assert_eq!(of("sh").get("started").and_then(Json::as_u64), Some(2));
    assert_eq!(sys("sh"), Some(true));
    assert_eq!(sys("bitspkgd"), Some(true));
    assert_eq!(sys("werkzeug"), Some(false), "run as root but not shipped with BitsOS");
    assert_eq!(sys("bitsmon"), Some(false), "a rights entry is shown on its own");
    let _ = std::fs::remove_dir_all(dir);
}
