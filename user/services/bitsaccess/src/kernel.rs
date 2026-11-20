//! The kernel's lines (`/dev/bitsos/access`, vfs::access::drain) turned into entries: program paths from the
//! program lines, kernel times (ns since boot) into wall time by the batch's last line (`now`).

use crate::model::Entry;
use mcpreg::Json;
use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq)]
pub struct Batch {
    pub entries: Vec<Entry>,
    /// The batch ended with the kernel's status line.
    pub complete: bool,
    pub on: bool,
    /// Records the kernel lost in full rings since boot, and cut by this read.
    pub lost: u64,
    pub cut: u64,
}

/// Whether `text` holds a whole batch (its status line came).
pub fn complete(text: &str) -> bool {
    text.lines().any(|l| l.starts_with("{\"s\":"))
}

/// One batch. `wall_now_ms`: the wall clock when it was read (the status line's `now` is the same moment).
pub fn parse(text: &str, wall_now_ms: u64) -> Batch {
    let mut b = Batch::default();
    let mut progs: BTreeMap<(u64, u64), (u32, String)> = BTreeMap::new();
    let mut raw: Vec<Json> = Vec::new();
    let mut now_ns = 0u64;
    for line in text.lines() {
        let Ok(j) = Json::parse(line) else { continue };
        let u = |k: &str| j.get(k).and_then(Json::as_u64);
        if let Some(p) = u("p") {
            let exe = j.get("exe").and_then(Json::as_str).unwrap_or("").to_string();
            progs.insert((p, u("start").unwrap_or(0)), (u("uid").unwrap_or(0) as u32, exe));
        } else if u("s").is_some() {
            b.complete = true;
            b.on = j.get("on").and_then(Json::as_bool).unwrap_or(false);
            b.lost = u("lost").unwrap_or(0);
            b.cut = u("cut").unwrap_or(0);
            now_ns = u("now").unwrap_or(0);
        } else if u("pid").is_some() {
            raw.push(j);
        }
    }
    if now_ns == 0 {
        now_ns = raw.iter().filter_map(|j| j.get("t1").and_then(Json::as_u64)).max().unwrap_or(0);
    }
    let wall = |t: u64| wall_now_ms.saturating_sub(now_ns.saturating_sub(t) / 1_000_000);
    for j in raw {
        let s = |k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        let u = |k: &str| j.get(k).and_then(Json::as_u64).unwrap_or(0);
        let (pid, start) = (u("pid"), u("start"));
        let (uid, exe) = progs.get(&(pid, start)).cloned().unwrap_or_default();
        let kind = s("k");
        let what = match kind.as_str() {
            "read" | "write" | "create" | "unlink" | "rename" => s("dir"),
            "device" | "exec" | "setid" => s("path"),
            "connect" | "listen" | "accept" | "netclose" => s("addr"),
            "caps" => u("caps").to_string(),
            "ptrace" => u("target").to_string(),
            _ => continue,
        };
        b.entries.push(Entry {
            t0: wall(u("t0")),
            t1: wall(u("t1")),
            exe,
            pid: pid as u32,
            uid,
            kind,
            what,
            name: s("name"),
            proto: s("proto"),
            port: u("port") as u16,
            write: j.get("w").and_then(Json::as_bool).unwrap_or(false),
            n: u("n").max(1),
            tx: u("tx"),
            rx: u("rx"),
            new: false,
        });
    }
    b
}
