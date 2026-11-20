//! The answers of the socket (C5: ops `summary` {since_s, app?} and `events` {since_s, app?, kind?, max}) and of the
//! MCP tool `access_log`. `summary`: per program, newest first, how often it was started, its folders, hosts,
//! devices and rights in the words the page uses; `events`: the entries themselves, newest first (the last hour as
//! they came, older ones merged per day). An exec entry belongs to the program it started (the kernel names a
//! pid's new program from its exec on), so `started` counts a program's own starts.
//!
//! `system` (the page's "BitsOS selbst" group, one line each) holds only what is shipped with BitsOS and has
//! nothing to point out: a program elsewhere, even run as root, or one with a rights entry or something new, is
//! shown on its own.

use crate::model::{self, Entry};
use crate::store::{RECENT_MS, Store};
use mcpreg::Json;
use std::collections::BTreeMap;

/// What one program shows at most per part (the rest is counted).
pub const PART_MAX: usize = 40;
pub const EVENTS_MAX: usize = 2000;

fn num(v: u64) -> Json {
    Json::Num(v as f64)
}

#[derive(Default)]
struct Prog {
    system: bool,
    started: u64,
    first: u64,
    last: u64,
    pids: Vec<u32>,
    new: bool,
    /// folder -> (read, write, create, delete, rename, last, new, example)
    files: BTreeMap<String, (u64, u64, u64, u64, u64, u64, bool, String)>,
    /// (addr, proto, port) -> (connect, accept, listen, tx, rx, last, new)
    net: BTreeMap<(String, String, u16), (u64, u64, u64, u64, u64, u64, bool)>,
    /// class -> (paths, count, last, new, write)
    devices: BTreeMap<String, (Vec<String>, u64, u64, bool, bool)>,
    /// (kind, what) -> (count, last)
    rights: BTreeMap<(String, String), (u64, u64)>,
}

fn matches_app(e: &Entry, app: Option<&str>) -> bool {
    app.is_none_or(|a| e.exe == a || model::program_name(&e.exe) == a)
}

/// The summary since `since` (unix ms), optionally for one program (its path or file name).
pub fn summary(s: &Store, since: u64, app: Option<&str>, roots: &[String], now: u64) -> Json {
    let mut progs: BTreeMap<String, Prog> = BTreeMap::new();
    for e in s.since(since).filter(|e| matches_app(e, app)) {
        let p = progs.entry(e.exe.clone()).or_insert_with(|| Prog { system: model::is_system_path(&e.exe), first: e.t0, ..Prog::default() });
        p.first = p.first.min(e.t0);
        p.last = p.last.max(e.t1);
        p.new |= e.new;
        if !p.pids.contains(&e.pid) && p.pids.len() < 8 {
            p.pids.push(e.pid);
        }
        if e.is_file() {
            let f = p.files.entry(e.what.clone()).or_default();
            match e.kind.as_str() {
                "read" => f.0 += e.n,
                "write" => f.1 += e.n,
                "create" => f.2 += e.n,
                "unlink" => f.3 += e.n,
                _ => f.4 += e.n,
            }
            f.5 = f.5.max(e.t1);
            f.6 |= e.new;
            if !e.name.is_empty() {
                f.7 = e.name.clone();
            }
        } else if e.is_net() {
            let k = p.net.entry((e.what.clone(), e.proto.clone(), e.port)).or_default();
            match e.kind.as_str() {
                "connect" => k.0 += e.n,
                "accept" => k.1 += e.n,
                "listen" => k.2 += e.n,
                _ => {}
            }
            k.3 += e.tx;
            k.4 += e.rx;
            k.5 = k.5.max(e.t1);
            k.6 |= e.new;
        } else if e.kind == "device" {
            let d = p.devices.entry(model::device_class(&e.what).to_string()).or_default();
            if !d.0.contains(&e.what) {
                d.0.push(e.what.clone());
            }
            d.1 += e.n;
            d.2 = d.2.max(e.t1);
            d.3 |= e.new;
            d.4 |= e.write;
        } else if e.is_right() {
            let r = p.rights.entry((e.kind.clone(), e.what.clone())).or_default();
            r.0 += e.n;
            r.1 = r.1.max(e.t1);
        } else if e.kind == "exec" {
            p.started += e.n;
        }
    }
    for p in progs.values_mut() {
        p.system &= p.rights.is_empty() && !p.new;
    }
    let mut list: Vec<(String, Prog)> = progs.into_iter().collect();
    list.sort_by(|a, b| b.1.last.cmp(&a.1.last).then(a.0.cmp(&b.0)));
    let programs = list.into_iter().map(|(exe, p)| prog_json(&exe, p, roots)).collect();
    Json::obj(vec![("since", num(since)), ("now", num(now)), ("on", Json::Bool(s.on)), ("lost", num(s.lost)), ("oldest", num(s.oldest().unwrap_or(0))), ("programs", Json::Arr(programs))])
}

fn newest_first<K, V>(m: BTreeMap<K, V>, last: impl Fn(&V) -> u64) -> (Vec<(K, V)>, usize) {
    let mut v: Vec<(K, V)> = m.into_iter().collect();
    v.sort_by_key(|(_, x)| std::cmp::Reverse(last(x)));
    let more = v.len().saturating_sub(PART_MAX);
    v.truncate(PART_MAX);
    (v, more)
}

fn prog_json(exe: &str, p: Prog, roots: &[String]) -> Json {
    let (files, files_more) = newest_first(p.files, |f| f.5);
    let files = files
        .into_iter()
        .map(|(folder, (r, w, c, d, m, last, new, ex))| {
            let root = model::private_root(&folder, roots);
            let mut v = vec![("folder", Json::str(&folder)), ("private", Json::Bool(root.is_some())), ("read", num(r)), ("write", num(w)), ("create", num(c)), ("delete", num(d)), ("rename", num(m)), ("last", num(last)), ("new", Json::Bool(new))];
            if !ex.is_empty() {
                v.push(("example", Json::str(&ex)));
            }
            Json::obj(v)
        })
        .collect();
    let (net, net_more) = newest_first(p.net, |n| n.5);
    let net = net
        .into_iter()
        .map(|((addr, proto, port), (c, a, l, tx, rx, last, new))| {
            Json::obj(vec![
                ("host", Json::str(&addr)),
                ("words", Json::str(&model::host_words(&addr))),
                ("proto", Json::str(&proto)),
                ("port", num(port as u64)),
                ("port_words", Json::str(&model::port_words(&proto, port))),
                ("connect", num(c)),
                ("accept", num(a)),
                ("listen", num(l)),
                ("tx", num(tx)),
                ("rx", num(rx)),
                ("last", num(last)),
                ("new", Json::Bool(new)),
            ])
        })
        .collect();
    let mut devices: Vec<(String, (Vec<String>, u64, u64, bool, bool))> = p.devices.into_iter().collect();
    devices.sort_by_key(|(c, _)| model::DEVICE_CLASSES.iter().position(|x| x.0 == c));
    let devices = devices
        .into_iter()
        .map(|(class, (paths, n, last, new, write))| {
            Json::obj(vec![
                ("class", Json::str(&class)),
                ("label", Json::str(model::device_name(&class))),
                ("paths", Json::Arr(paths.iter().map(|x| Json::str(x)).collect())),
                ("count", num(n)),
                ("last", num(last)),
                ("new", Json::Bool(new)),
                ("write", Json::Bool(write)),
            ])
        })
        .collect();
    let rights = p.rights.into_iter().map(|((kind, what), (n, last))| Json::obj(vec![("kind", Json::str(&kind)), ("what", Json::str(&what)), ("count", num(n)), ("last", num(last))])).collect();
    Json::obj(vec![
        ("app", Json::str(exe)),
        ("name", Json::str(model::program_name(exe))),
        ("system", Json::Bool(p.system)),
        ("started", num(p.started)),
        ("pids", Json::Arr(p.pids.iter().map(|x| num(*x as u64)).collect())),
        ("first", num(p.first)),
        ("last", num(p.last)),
        ("new", Json::Bool(p.new)),
        ("files", Json::Arr(files)),
        ("files_more", num(files_more as u64)),
        ("net", Json::Arr(net)),
        ("net_more", num(net_more as u64)),
        ("devices", Json::Arr(devices)),
        ("rights", Json::Arr(rights)),
    ])
}

/// The entries since `since`, newest first, at most `max`: the raw ones of the last hour when `since` lies within
/// it, else the day lines.
pub fn events(s: &Store, since: u64, app: Option<&str>, kind: Option<&str>, max: usize, now: u64) -> Json {
    let pick = |e: &&Entry| matches_app(e, app) && kind.is_none_or(|k| e.kind == k || (k == "files" && e.is_file()) || (k == "net" && e.is_net()) || (k == "rights" && e.is_right()));
    let mut v: Vec<&Entry> = if since + RECENT_MS >= now { s.recent_since(since).filter(pick).collect() } else { s.since(since).filter(pick).collect() };
    v.sort_by(|a, b| b.t1.cmp(&a.t1));
    let more = v.len().saturating_sub(max);
    v.truncate(max.min(EVENTS_MAX));
    Json::obj(vec![("since", num(since)), ("now", num(now)), ("more", num(more as u64)), ("events", Json::Arr(v.into_iter().map(Entry::to_json).collect()))])
}

/// One request `{"id","op","args"}` of the socket: its answer line (with the request's id).
pub fn answer(s: &Store, req: &str, roots: &[String], now: u64) -> String {
    let Ok(j) = Json::parse(req.trim()) else {
        return Json::obj(vec![("error", Json::str("keine Anfrage in JSON"))]).to_string();
    };
    let id = j.get("id").cloned().unwrap_or(Json::Null);
    let args = j.get("args").cloned().unwrap_or(Json::obj(vec![]));
    let since_s = args.get("since_s").and_then(Json::as_u64).unwrap_or(86_400);
    let since = now.saturating_sub(since_s.saturating_mul(1000));
    let app = args.get("app").and_then(Json::as_str);
    let result = match j.get("op").and_then(Json::as_str) {
        Some("summary") => summary(s, since, app, roots, now),
        Some("events") => {
            let max = args.get("max").and_then(Json::as_u64).unwrap_or(200).clamp(1, EVENTS_MAX as u64) as usize;
            events(s, since, app, args.get("kind").and_then(Json::as_str), max, now)
        }
        Some(op) => return Json::obj(vec![("id", id), ("error", Json::str(&format!("unbekannte Anfrage {op}")))]).to_string(),
        None => return Json::obj(vec![("id", id), ("error", Json::str("die Anfrage nennt kein op"))]).to_string(),
    };
    Json::obj(vec![("id", id), ("ok", Json::Bool(true)), ("result", result)]).to_string()
}
