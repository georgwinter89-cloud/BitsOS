//! The reader's side of the access log: the rings' records in time order, each attributed to its program image
//! (pid and start time: an exec starts a new image, a fork's child runs its parent's until it execs), merged per
//! image, kind and thing (a folder, a device node, a peer), and written as one JSON line each. Reads are merged
//! per folder; other file kinds keep the last file name as an example. A read hands out at most `MAX_ENTRIES`
//! merged entries; records beyond are counted (`cut`).
//!
//! Lines (one JSON object each, in this order):
//! - `{"p":PID,"start":NS,"uid":UID,"exe":"/usr/bin/x"}` a program image the entries below name (with
//!   `"tree":N` when its process is in assistant tree N, EINS-C);
//! - `{"pid":PID,"start":NS,"k":KIND,...,"n":COUNT,"t0":NS,"t1":NS}` an entry: files `"dir"` (+ `"name"`),
//!   `device` `"path"` and `"w"`, network `"proto"`, `"addr"`, `"port"` (`netclose` + `"tx"`, `"rx"`), `exec`
//!   and `setid` `"path"`, `caps` `"caps"`, `ptrace` `"target"`;
//! - `{"s":1,"on":BOOL,"lost":N,"cut":N,"now":NS}` last: records lost in full rings since boot, cut by this read.
//!
//! Times are ns since boot (the service turns them into wall time).
//!
//! A path gone before the reader came (records hold paths weakly) is named by its folder when that still lives,
//! else `GONE`: still counted, never dropped. The rings are emptied one after the other, so a record can reach a
//! later read than a newer one of another CPU: an exec or fork older than the image already known for that pid
//! does not replace it.

use super::{F_DIR, F_WRITE, Kind, NetAddr, Obj, Rec, Seen};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

/// The thing of a record whose path (and folder) went before the reader came.
pub const GONE: &[u8] = b"(nicht mehr da)";

/// The most merged entries one read hands out.
pub const MAX_ENTRIES: usize = 8192;

/// Each live pid's current program image (exe, start), carried from read to read.
static KNOWN: ksync::Mutex<BTreeMap<u32, (Vec<u8>, u64)>> = ksync::Mutex::named("vfs.access.known", BTreeMap::new());

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub n: u64,
    pub t0: u64,
    pub t1: u64,
    pub tx: u64,
    pub rx: u64,
    pub write: bool,
    /// Files: the last name in the folder (not for reads).
    pub name: Vec<u8>,
}

/// (pid, image start, kind, thing).
pub type Key = (u32, u64, Kind, Vec<u8>);

#[derive(Default)]
pub struct Merged {
    /// (pid, start) -> (uid, exe).
    pub progs: BTreeMap<(u32, u64), (u32, Vec<u8>)>,
    pub entries: BTreeMap<Key, Entry>,
    pub cut: u64,
}

/// Records a ktest took out of the rings that belong to other programs: the next read gets them first.
static PENDING: ksync::Mutex<Vec<Rec>> = ksync::Mutex::named("vfs.access.pending", Vec::new());
/// Ktests holding the log: reads hand out only the status line meanwhile.
static HOLDS: AtomicU32 = AtomicU32::new(0);

/// Drops what the reader remembers and what a ktest kept back (recording switched off).
pub fn forget() {
    KNOWN.lock().clear();
    drop(core::mem::take(&mut *PENDING.lock()));
}

/// A ktest's sole use of the rings on a live system (bitsaccess reads every 0.5 s): while it lives, reads of the
/// node wait with the records; what the test takes out that is not its own goes back to the next read.
pub struct Hold(());

pub fn hold() -> Hold {
    HOLDS.fetch_add(1, Ordering::AcqRel);
    Hold(())
}

impl Hold {
    /// Every waiting record `mine` picks; the others are kept for the next read.
    pub fn take(&self, mine: impl Fn(&Rec) -> bool) -> Vec<Rec> {
        let (m, others): (Vec<Rec>, Vec<Rec>) = super::ring::take_all().into_iter().partition(|r| mine(r));
        PENDING.lock().extend(others);
        m
    }
    /// `recs` merged with the reader's program table and written as a read would.
    pub fn text(&self, recs: Vec<Rec>) -> String {
        let m = merge(recs, &mut KNOWN.lock(), &super::proc_info, false);
        render(&m, super::on(), super::dropped(), ksync::now_ns().unwrap_or(0))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        HOLDS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn path_bytes(p: &crate::mount_ns::Path) -> Vec<u8> {
    crate::mount_ns::path_to_top(p)
}

/// A kept path as text: the path itself, or `GONE`.
fn seen_bytes(s: &Seen) -> Vec<u8> {
    s.path().map_or_else(|| GONE.to_vec(), |p| path_bytes(&p))
}

fn split_dir(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    match p.iter().rposition(|&b| b == b'/') {
        Some(0) => (b"/".to_vec(), p[1..].to_vec()),
        Some(i) => (p[..i].to_vec(), p[i + 1..].to_vec()),
        None => (p.to_vec(), Vec::new()),
    }
}

pub fn addr_text(a: &NetAddr) -> String {
    let mut s = String::new();
    let v4 = if !a.v6 { Some(&a.addr[..4]) } else if a.addr[..10] == [0; 10] && a.addr[10..12] == [0xff, 0xff] { Some(&a.addr[12..]) } else { None };
    match v4 {
        Some(b) => {
            let _ = write!(s, "{}.{}.{}.{}", b[0], b[1], b[2], b[3]);
        }
        None => {
            for i in 0..8 {
                let _ = write!(s, "{}{:x}", if i > 0 { ":" } else { "" }, u16::from_be_bytes([a.addr[2 * i], a.addr[2 * i + 1]]));
            }
        }
    }
    s
}

fn proto_word(p: u8) -> &'static str {
    match p {
        6 => "tcp",
        17 => "udp",
        _ => "ip",
    }
}

/// The thing a record is kept under, and the file name it names (files only).
fn thing(r: &Rec) -> (Vec<u8>, Vec<u8>) {
    match (&r.obj, r.kind) {
        (Obj::Path(p), Kind::Read | Kind::Write | Kind::Create | Kind::Unlink | Kind::Rename) => {
            let named = |n: Vec<u8>| if r.kind == Kind::Read { Vec::new() } else { n };
            if r.flags & F_DIR != 0 {
                (seen_bytes(p), Vec::new())
            } else if let Some(full) = p.path().map(|x| path_bytes(&x)) {
                let (d, n) = split_dir(&full);
                (d, named(n))
            } else {
                // Removed before the reader came: its folder and the name it had then.
                let d = p.folder().map_or_else(|| GONE.to_vec(), |x| path_bytes(&x));
                (d, named(p.name.as_deref().unwrap_or_default().to_vec()))
            }
        }
        (Obj::Path(p), _) => (seen_bytes(p), Vec::new()),
        (Obj::Net(a), _) => {
            let mut s = String::from(proto_word(a.proto));
            let _ = write!(s, " {} {}", addr_text(a), a.port);
            (s.into_bytes(), Vec::new())
        }
        (Obj::Num(n), _) => {
            let mut s = String::new();
            let _ = write!(s, "{n}");
            (s.into_bytes(), Vec::new())
        }
    }
}

type Image = (Vec<u8>, u64);

/// The image `pid` runs at `t`: the table's, or the live one when the table has none, or when an exec or fork
/// record was lost (`checked`: the live answers of this merge) and the live image started after the table's and
/// before `t`.
fn image_at(pid: u32, t: u64, known: &mut BTreeMap<u32, Image>, info: &dyn Fn(u32) -> Option<Image>, checked: Option<&mut BTreeMap<u32, Option<Image>>>) -> Image {
    if let Some(checked) = checked
        && let Some(table) = known.get(&pid).map(|k| k.1)
        && let Some(live) = checked.entry(pid).or_insert_with(|| info(pid)).as_ref()
        && live.1 > table
        && t >= live.1
    {
        known.insert(pid, live.clone());
    }
    known.entry(pid).or_insert_with(|| info(pid).unwrap_or_default()).clone()
}

/// Merges `recs` (any order). `info` answers a pid's live program (exe, start); `known` is the table carried
/// between reads, pruned to live pids at the end. `identity_lost`: an exec or fork record was lost, so the table
/// is checked against the live programs.
pub fn merge(mut recs: Vec<Rec>, known: &mut BTreeMap<u32, Image>, info: &dyn Fn(u32) -> Option<Image>, identity_lost: bool) -> Merged {
    recs.sort_by_key(|r| r.t0);
    let mut m = Merged::default();
    let mut checked = identity_lost.then(BTreeMap::new);
    for r in recs {
        // A time of 0 (no clock yet: early boot, host tests) cannot be ordered.
        let newer = |pid: u32, known: &BTreeMap<u32, Image>| r.t0 > 0 && known.get(&pid).is_some_and(|k| k.1 > r.t0);
        let (exe, start) = match (r.kind, &r.obj) {
            (Kind::Exec, Obj::Path(p)) => {
                let img = (seen_bytes(p), r.t0);
                if !newer(r.pid, known) {
                    known.insert(r.pid, img.clone());
                }
                img
            }
            (Kind::Fork, Obj::Num(child)) => {
                let child = *child as u32;
                if !newer(child, known) {
                    let img = image_at(r.pid, r.t0, known, info, checked.as_mut());
                    known.insert(child, img);
                }
                continue;
            }
            _ => image_at(r.pid, r.t0, known, info, checked.as_mut()),
        };
        let (obj, name) = thing(&r);
        let key = (r.pid, start, r.kind, obj);
        if !m.entries.contains_key(&key) && m.entries.len() >= MAX_ENTRIES {
            m.cut += r.n as u64;
            continue;
        }
        m.progs.entry((r.pid, start)).or_insert((r.uid, exe));
        let e = m.entries.entry(key).or_insert_with(|| Entry { t0: r.t0, ..Entry::default() });
        e.n += r.n as u64;
        e.t0 = e.t0.min(r.t0);
        e.t1 = e.t1.max(r.t1);
        e.tx += r.tx;
        e.rx += r.rx;
        e.write |= r.flags & F_WRITE != 0;
        if !name.is_empty() {
            e.name = name;
        }
    }
    known.retain(|pid, _| info(*pid).is_some());
    m
}

/// `b` as a JSON string (bytes that are not UTF-8 become U+FFFD).
pub fn json_str(out: &mut String, b: &[u8]) {
    out.push('"');
    for c in String::from_utf8_lossy(b).chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// The lines of one read.
pub fn render(m: &Merged, on: bool, lost: u64, now: u64) -> String {
    let mut out = String::new();
    for ((pid, start), (uid, exe)) in &m.progs {
        let _ = write!(out, "{{\"p\":{pid},\"start\":{start},\"uid\":{uid},\"exe\":");
        json_str(&mut out, exe);
        // EINS-C (K5): a program of the assistant's tree says so.
        match super::tree_of(*pid) {
            0 => out.push_str("}\n"),
            t => {
                let _ = writeln!(out, ",\"tree\":{t}}}");
            }
        }
    }
    for ((pid, start, kind, obj), e) in &m.entries {
        let _ = write!(out, "{{\"pid\":{pid},\"start\":{start},\"k\":\"{}\"", kind.word());
        match kind {
            Kind::Read | Kind::Write | Kind::Create | Kind::Unlink | Kind::Rename => {
                out.push_str(",\"dir\":");
                json_str(&mut out, obj);
                if !e.name.is_empty() {
                    out.push_str(",\"name\":");
                    json_str(&mut out, &e.name);
                }
            }
            Kind::Device => {
                out.push_str(",\"path\":");
                json_str(&mut out, obj);
                let _ = write!(out, ",\"w\":{}", e.write);
            }
            Kind::Connect | Kind::Listen | Kind::Accept | Kind::NetClose => {
                let t = String::from_utf8_lossy(obj);
                let mut w = t.split(' ');
                let (p, a, port) = (w.next().unwrap_or(""), w.next().unwrap_or(""), w.next().unwrap_or("0"));
                let _ = write!(out, ",\"proto\":\"{p}\",\"addr\":\"{a}\",\"port\":{port}");
                if *kind == Kind::NetClose {
                    let _ = write!(out, ",\"tx\":{},\"rx\":{}", e.tx, e.rx);
                }
            }
            Kind::Exec | Kind::SetId => {
                out.push_str(",\"path\":");
                json_str(&mut out, obj);
            }
            Kind::CapRaise => {
                let _ = write!(out, ",\"caps\":{}", String::from_utf8_lossy(obj));
            }
            Kind::Ptrace => {
                let _ = write!(out, ",\"target\":{}", String::from_utf8_lossy(obj));
            }
            Kind::Fork => {}
        }
        let _ = writeln!(out, ",\"n\":{},\"t0\":{},\"t1\":{}}}", e.n, e.t0, e.t1);
    }
    let _ = writeln!(out, "{{\"s\":1,\"on\":{on},\"lost\":{lost},\"cut\":{},\"now\":{now}}}", m.cut);
    out
}

/// One read's text: every waiting record, merged (only the status line while a ktest holds the log).
pub fn take_text() -> String {
    let now = || ksync::now_ns().unwrap_or(0);
    if HOLDS.load(Ordering::Acquire) > 0 {
        return render(&Merged::default(), super::on(), super::dropped(), now());
    }
    let mut recs = core::mem::take(&mut *PENDING.lock());
    recs.append(&mut super::ring::take_all());
    let lost = super::ring::take_identity_lost();
    let m = {
        let mut known = KNOWN.lock();
        merge(recs, &mut known, &super::proc_info, lost)
    };
    render(&m, super::on(), super::dropped(), now())
}
