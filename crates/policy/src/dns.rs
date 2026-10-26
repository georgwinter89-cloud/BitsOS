//! Names for the connection view (PM-2, B3): the names a program resolved itself, taken from the DNS answers
//! it receives on its own UDP sockets (the net stack hands every datagram from port 53 on a program socket to
//! `note_dns_answer`; the kernel's own sockets are never looked at). Nothing is logged per answer and nothing
//! is resolved here: a connection shows the name only if its own program asked for it.
//!
//! A name is remembered per program (its ProgramId; BitsOS's own programs, which all share ProgramId 0, per
//! process) and address. How many are kept is a setting (`set_dns_keep`), the oldest goes first.
//! The answer is parsed by RFC 1035 §4.1: header, the first question's name (the name the program asked for),
//! A/AAAA records of that name or of a CNAME chain from it. Hostile input has bounds that real answers never
//! reach (they are guards against loops and floods, not counts of things): compression pointers are followed at
//! most `MAX_JUMPS` times per name, a CNAME chain has at most `MAX_CHAIN` links, at most `MAX_RECORDS` answer
//! records are looked at and at most `MAX_ADDRS` addresses taken from one answer. So one datagram costs work
//! linear in its size, and the names table changes by at most `MAX_ADDRS` entries per answer.

use crate::{IpAddr, Subject};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use ksync::SpinLock;

/// Pointers followed for one name (RFC 1035 names are at most 255 bytes, so a real one needs far fewer).
const MAX_JUMPS: usize = 64;
/// CNAME links followed from the question's name (resolvers give up far earlier; RFC 1034 §3.6.2: chains are short).
const MAX_CHAIN: usize = 16;
/// Answer records looked at (a real answer has a few dozen at most).
const MAX_RECORDS: usize = 256;
/// Addresses taken from one answer.
const MAX_ADDRS: usize = 32;
const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

fn u16_at(m: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*m.get(at)?, *m.get(at + 1)?]))
}

/// The name at `pos` (lower case; bytes other than letters, digits, '-' and '_' become '?') and the position
/// after it in the record.
fn read_name(m: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut after = None;
    let mut jumps = 0;
    loop {
        let len = *m.get(pos)? as usize;
        match len & 0xC0 {
            0xC0 => {
                after.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > MAX_JUMPS {
                    return None;
                }
                pos = ((len & 0x3F) << 8) | *m.get(pos + 1)? as usize;
            }
            0 if len == 0 => return Some((out, after.unwrap_or(pos + 1))),
            0 => {
                let label = m.get(pos + 1..pos + 1 + len)?;
                if !out.is_empty() {
                    out.push('.');
                }
                for &b in label {
                    let ok = b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
                    out.push(if ok { b.to_ascii_lowercase() as char } else { '?' });
                }
                if out.len() > 255 {
                    return None;
                }
                pos += 1 + len;
            }
            _ => return None,
        }
    }
}

/// A DNS answer: the name asked for and the addresses given for it (directly or through CNAMEs). None for a
/// query, an error answer, an answer without addresses or a malformed message.
pub fn parse_answer(m: &[u8]) -> Option<(String, Vec<IpAddr>)> {
    let flags = u16_at(m, 2)?;
    // QR set, opcode QUERY, RCODE NOERROR.
    if flags & 0x8000 == 0 || (flags >> 11) & 0xF != 0 || flags & 0xF != 0 {
        return None;
    }
    // One question (RFC 9619: QDCOUNT is one for QUERY; nothing else names an answer unambiguously).
    let (qd, an) = (u16_at(m, 4)?, u16_at(m, 6)?);
    if qd != 1 || an == 0 {
        return None;
    }
    let (qname, mut pos) = read_name(m, 12)?;
    pos += 4;
    if qname.is_empty() {
        return None;
    }
    let mut cnames: Vec<(String, String)> = Vec::new();
    let mut addrs: Vec<(String, IpAddr)> = Vec::new();
    for _ in 0..(an as usize).min(MAX_RECORDS) {
        let (owner, p) = read_name(m, pos)?;
        let (ty, class, len) = (u16_at(m, p)?, u16_at(m, p + 2)?, u16_at(m, p + 8)? as usize);
        let data = m.get(p + 10..p + 10 + len)?;
        pos = p + 10 + len;
        if class != CLASS_IN {
            continue;
        }
        match (ty, len) {
            (TYPE_A, 4) => addrs.push((owner, IpAddr::V4([data[0], data[1], data[2], data[3]]))),
            (TYPE_AAAA, 16) => {
                let mut a = [0u8; 16];
                a.copy_from_slice(data);
                addrs.push((owner, IpAddr::V6(a)));
            }
            (TYPE_CNAME, _) => cnames.push((owner, read_name(m, p + 10)?.0)),
            _ => {}
        }
    }
    // The names that stand for qname: qname and the CNAME chain from it, one link per round (one scan of the
    // CNAMEs each), ending at a name without a CNAME, at a loop or after MAX_CHAIN links.
    let mut chain: Vec<&str> = alloc::vec![qname.as_str()];
    while chain.len() <= MAX_CHAIN {
        let at = chain[chain.len() - 1];
        match cnames.iter().find(|(o, _)| o == at) {
            Some((_, t)) if !chain.contains(&t.as_str()) => chain.push(t),
            _ => break,
        }
    }
    let ips: Vec<IpAddr> =
        addrs.iter().filter(|(o, _)| chain.contains(&o.as_str())).map(|(_, a)| *a).take(MAX_ADDRS).collect();
    if ips.is_empty() {
        return None;
    }
    Some((qname, ips))
}

/// Whose names these are: a program by its id, BitsOS's own programs (id 0) by process.
pub(crate) fn owner(s: &Subject) -> u64 {
    if s.program.0 != 0 { (1 << 32) | s.program.0 as u64 } else { s.pid as u64 }
}

pub(crate) fn ip_key(a: IpAddr) -> [u8; 17] {
    let mut k = [0u8; 17];
    match a {
        IpAddr::V4(b) => k[13..].copy_from_slice(&b),
        IpAddr::V6(b) => {
            k[0] = 6;
            k[1..].copy_from_slice(&b);
        }
    }
    k
}

/// The remembered names; `order` holds the same keys oldest first, so the oldest goes in O(log n). With each
/// name the pid whose socket got it (tests forget only what their own pids added, PM-2c).
pub(crate) struct Names {
    map: BTreeMap<(u64, [u8; 17]), (Arc<str>, u64, u32)>,
    order: BTreeMap<u64, (u64, [u8; 17])>,
    seq: u64,
}

impl Names {
    pub(crate) const fn new() -> Names {
        Names { map: BTreeMap::new(), order: BTreeMap::new(), seq: 0 }
    }

    pub(crate) fn insert(&mut self, who: u64, pid: u32, ip: IpAddr, name: &Arc<str>, keep: usize) {
        self.seq += 1;
        let key = (who, ip_key(ip));
        if let Some((_, old, _)) = self.map.insert(key, (name.clone(), self.seq, pid)) {
            self.order.remove(&old);
        }
        self.order.insert(self.seq, key);
        while self.map.len() > keep.max(1) {
            let Some((_, k)) = self.order.pop_first() else { break };
            self.map.remove(&k);
        }
    }

    /// Forgets the names of `who` that `pid` added (rerunnable tests clean up after themselves): never the ones
    /// another process of the same program resolved (pm.conn.self_exec runs as ProgramId 1, Brave's on the
    /// Mini-PC, and took all its names with it before PM-2c).
    pub(crate) fn forget(&mut self, who: u64, pid: u32) {
        let gone: Vec<u64> =
            self.map.range((who, [0; 17])..=(who, [0xff; 17])).filter(|(_, v)| v.2 == pid).map(|(_, v)| v.1).collect();
        for seq in gone {
            if let Some(k) = self.order.remove(&seq) {
                self.map.remove(&k);
            }
        }
    }

    pub(crate) fn get(&self, who: u64, ip: IpAddr) -> Option<Arc<str>> {
        self.map.get(&(who, ip_key(ip))).map(|v| v.0.clone())
    }

    pub(crate) fn len(&self) -> usize {
        debug_assert_eq!(self.map.len(), self.order.len());
        self.map.len()
    }
}

static NAMES: SpinLock<Names> = SpinLock::named("policy.dns", Names::new());
static KEEP: AtomicUsize = AtomicUsize::new(4096);

pub fn set_dns_keep(n: usize) {
    KEEP.store(n.max(1), Ordering::Relaxed);
}

pub fn dns_keep() -> usize {
    KEEP.load(Ordering::Relaxed)
}

/// Names remembered now (all programs).
pub fn dns_names() -> usize {
    NAMES.lock().len()
}

/// The net hook: `msg` arrived on `subject`'s socket from UDP port 53. A query, a malformed message or the
/// kernel's own socket changes nothing.
pub fn note_dns_answer(subject: &Subject, msg: &[u8]) {
    if subject.pid == 0 {
        return;
    }
    let Some((qname, ips)) = parse_answer(msg) else { return };
    let name: Arc<str> = Arc::from(qname.as_str());
    let (who, keep) = (owner(subject), dns_keep());
    let mut n = NAMES.lock();
    for ip in ips {
        n.insert(who, subject.pid, ip, &name, keep);
    }
}

/// The name `subject`'s program resolved for `ip`, if it did.
pub(crate) fn name_of(subject: &Subject, ip: IpAddr) -> Option<Arc<str>> {
    if subject.pid == 0 {
        return None;
    }
    NAMES.lock().get(owner(subject), ip)
}

/// Forgets the names `subject`'s process resolved (rerunnable tests: `forget_test_connections`).
pub(crate) fn forget(subject: &Subject) {
    NAMES.lock().forget(owner(subject), subject.pid);
}

/// Whether a name of `subject`'s program for `ip` is stored (tests: also for the kernel, which `name_of` skips).
pub(crate) fn stored(subject: &Subject, ip: IpAddr) -> bool {
    NAMES.lock().get(owner(subject), ip).is_some()
}
