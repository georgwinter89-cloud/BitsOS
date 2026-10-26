//! The connection view (PM-2, B3): per connection the program, the peer, the name the program itself resolved
//! (`dns`), the bytes each way and the first and last time; live ones and past ones.
//!
//! The net stack opens a connection with `open` and keeps the `ConnHandle` in its socket: it counts bytes with
//! one relaxed add per read or write (the cost the stream's own counters had), and the connection ends when the
//! handle is closed or dropped. Callers of the older `record_conn` (event records only) feed the view too:
//! a Closed record ends the live one with the same addresses made from a record.
//!
//! Live connections are as many as there are sockets; past ones are kept up to `conn_keep` (a setting), the
//! oldest go first. Both lists grow with memory allocated before the lock is taken, so the net path never waits
//! on the allocator while holding `policy.view` (a leaf lock). The last time of a live connection is when the
//! view last saw its bytes move (exact at the close): an exact one would cost a clock read per read or write.
//! Each close is a klog event `pm conn.closed` with every field, so the journal (RB-8) keeps the history.

use crate::conn::{conn_keep, push_event};
use crate::{dns, ConnKind, ConnRecord, IpAddr, NetProto, ProgramId, Subject};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use ksync::{OnceRef, SpinLock};

/// Who owns a connection by name (PM-2c): policy knows pids, not processes, so the crate that knows them
/// (policy-mcp, over `proc`) answers here when a connection opens. The names are stored with the connection then,
/// so a close after the process ended (most of Brave's) and the view of an ended program still name it.
pub trait Owners: Sync {
    /// The program name (the executable's file name) and the comm of `subject`'s process if it runs now and
    /// started at or before `at_ns` (the connection's first time, `ksync::now_ns` clock as proc's `start_ns`); None
    /// for an unknown pid or a younger process (the pid was reused since a late record's time). Called in the
    /// net path before any view lock is taken: must not block or sleep.
    fn names(&self, subject: &Subject, at_ns: u64) -> Option<(Arc<str>, Arc<str>)>;
}

static OWNERS: OnceRef<dyn Owners> = OnceRef::new();

/// Installs the owner names (once; a second call changes nothing and returns false).
pub fn set_owners(o: &'static dyn Owners) -> bool {
    OWNERS.set(o)
}

/// The owner's (program name, comm when it differs) for a new connection first seen at `t`; nothing for the
/// kernel.
fn owner_names(subject: &Subject, t: u64) -> (Option<Arc<str>>, Option<Arc<str>>) {
    if subject.pid == 0 {
        return (None, None);
    }
    match OWNERS.get().and_then(|o| o.names(subject, t)) {
        Some((n, c)) => {
            let comm = (*c != *n).then_some(c);
            (Some(n), comm)
        }
        None => (None, None),
    }
}

#[derive(Default)]
struct Labels {
    name: Option<Arc<str>>,
    program_name: Option<Arc<str>>,
    /// The process's comm when it differs from `program_name` (Chromium's helpers run as `exe`).
    comm: Option<Arc<str>>,
}

pub struct Conn {
    id: u64,
    subject: Subject,
    proto: NetProto,
    accepted: bool,
    local_port: u16,
    remote: IpAddr,
    remote_port: u16,
    first_ns: u64,
    bytes: Arc<ConnCounters>,
    /// Held by a socket's `ConnHandle` (else made from `record_conn`; only those end by a Closed record).
    handled: bool,
    last_ns: AtomicU64,
    /// 0 while live.
    closed_ns: AtomicU64,
    /// bytes_in + bytes_out when the view last looked.
    seen: AtomicU64,
    /// Index in the live list; changed only under the store lock.
    slot: AtomicUsize,
    labels: SpinLock<Labels>,
}

/// Bytes each way of one connection. A socket that counts before its connection is recorded (TCP: from the
/// handshake on) makes its own and hands it to `open_counted`; the view reads the same counters.
#[derive(Default)]
pub struct ConnCounters {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl ConnCounters {
    pub fn new() -> Arc<ConnCounters> {
        Arc::new(ConnCounters::default())
    }

    #[inline]
    pub fn add_in(&self, n: u64) {
        self.bytes_in.fetch_add(n, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_out(&self, n: u64) {
        self.bytes_out.fetch_add(n, Ordering::Relaxed);
    }

    /// (in, out).
    pub fn get(&self) -> (u64, u64) {
        (self.bytes_in.load(Ordering::Relaxed), self.bytes_out.load(Ordering::Relaxed))
    }
}

/// A live connection, held by its socket. Dropping it ends the connection.
pub struct ConnHandle(Arc<Conn>);

impl ConnHandle {
    #[inline]
    pub fn add_in(&self, n: u64) {
        self.0.bytes.add_in(n);
    }

    #[inline]
    pub fn add_out(&self, n: u64) {
        self.0.bytes.add_out(n);
    }

    pub fn bytes(&self) -> (u64, u64) {
        self.0.bytes.get()
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }

    /// Shows `name` instead of a resolved one (the UDP entry that stands for many peers).
    pub(crate) fn label(&self, name: &'static str) {
        self.0.labels.lock().name = Some(Arc::from(name));
    }

    /// Ends the connection now (a second close or the drop afterwards changes nothing).
    pub fn close(&self) {
        close(&self.0, now());
    }
}

impl Drop for ConnHandle {
    fn drop(&mut self) {
        close(&self.0, now());
    }
}

struct Store {
    live: VecDeque<Arc<Conn>>,
    past: VecDeque<Arc<Conn>>,
}

static STORE: SpinLock<Store> = SpinLock::named("policy.view", Store { live: VecDeque::new(), past: VecDeque::new() });
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Room for one more in `v` without allocating here: Err(want) = allocate a ring of `want` outside the lock and
/// call again with it in `spare` (which then holds the old, empty ring, freed after the lock).
fn room<T>(v: &mut VecDeque<T>, spare: &mut Option<VecDeque<T>>, limit: usize) -> Result<(), usize> {
    if v.len() < v.capacity() || v.len() >= limit {
        return Ok(());
    }
    if let Some(b) = spare.as_mut().filter(|b| b.capacity() > v.len()) {
        b.append(v);
        core::mem::swap(v, b);
        return Ok(());
    }
    Err(limit.min(v.capacity().max(8) * 2))
}

#[allow(clippy::too_many_arguments)]
fn new_conn(subject: Subject, proto: NetProto, accepted: bool, local_port: u16, remote: IpAddr, remote_port: u16, t: u64, bytes: Arc<ConnCounters>, handled: bool) -> Arc<Conn> {
    let name = dns::name_of(&subject, remote);
    // A live open is the running owner's own (no time check: CPUs' clocks may differ by a little); a record's
    // time may be long past, when the pid may have been reused since.
    let (program_name, comm) = owner_names(&subject, if handled { u64::MAX } else { t });
    Arc::new(Conn {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        subject,
        proto,
        accepted,
        local_port,
        remote,
        remote_port,
        first_ns: t,
        bytes,
        handled,
        last_ns: AtomicU64::new(t),
        closed_ns: AtomicU64::new(0),
        seen: AtomicU64::new(0),
        slot: AtomicUsize::new(usize::MAX),
        labels: SpinLock::named("policy.view.labels", Labels { name, program_name, comm }),
    })
}

fn insert_live(c: &Arc<Conn>) {
    let mut spare = None;
    loop {
        let mut s = STORE.lock();
        if let Err(want) = room(&mut s.live, &mut spare, usize::MAX) {
            drop(s);
            spare = Some(VecDeque::with_capacity(want));
            continue;
        }
        c.slot.store(s.live.len(), Ordering::Relaxed);
        s.live.push_back(c.clone());
        return;
    }
}

/// Moves `c` from the live list (if it is there) to the past ones.
fn retire(c: &Arc<Conn>) {
    let keep = conn_keep().max(1);
    let mut spare = None;
    // At most two go per retire (one when full; two while a lowered conn_keep has not been trimmed yet, so the
    // list shrinks back), freed after the lock.
    let mut gone: [Option<Arc<Conn>>; 2] = [None, None];
    loop {
        let mut s = STORE.lock();
        if let Err(want) = room(&mut s.past, &mut spare, keep) {
            drop(s);
            spare = Some(VecDeque::with_capacity(want));
            continue;
        }
        let i = c.slot.load(Ordering::Relaxed);
        if s.live.get(i).is_some_and(|x| Arc::ptr_eq(x, c)) {
            s.live.swap_remove_back(i);
            if let Some(m) = s.live.get(i) {
                m.slot.store(i, Ordering::Relaxed);
            }
            c.slot.store(usize::MAX, Ordering::Relaxed);
        }
        for g in gone.iter_mut() {
            if s.past.len() >= keep {
                *g = s.past.pop_front();
            }
        }
        s.past.push_back(c.clone());
        drop(s);
        // An evicted connection is freed here, after the lock.
        drop(gone);
        return;
    }
}

fn close(c: &Arc<Conn>, t: u64) {
    let t = t.max(c.first_ns).max(1);
    if c.closed_ns.compare_exchange(0, t, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    c.last_ns.fetch_max(t, Ordering::Relaxed);
    retire(c);
    let (bin, bout) = ended(c, t);
    push_event(ConnRecord {
        subject: c.subject,
        kind: ConnKind::Closed,
        proto: c.proto,
        local_port: c.local_port,
        remote: c.remote,
        remote_port: c.remote_port,
        bytes_in: bin,
        bytes_out: bout,
        time_ns: t,
    });
}

pub fn proto_name(p: NetProto) -> &'static str {
    match p {
        NetProto::Tcp => "tcp",
        NetProto::Udp => "udp",
        NetProto::Unix => "unix",
        NetProto::Other => "other",
    }
}

/// A connection begins (`accepted`: the peer started it). The name is the one `subject`'s program resolved for
/// `remote`, if any. Also an Opened/Accepted event record (`recent_connections`).
pub fn open(subject: Subject, proto: NetProto, accepted: bool, local_port: u16, remote: IpAddr, remote_port: u16) -> ConnHandle {
    open_counted(subject, proto, accepted, local_port, remote, remote_port, ConnCounters::new())
}

/// `open` with the socket's own counters (bytes it counted before are the connection's).
pub fn open_counted(subject: Subject, proto: NetProto, accepted: bool, local_port: u16, remote: IpAddr, remote_port: u16, bytes: Arc<ConnCounters>) -> ConnHandle {
    let t = now();
    let c = new_conn(subject, proto, accepted, local_port, remote, remote_port, t, bytes, true);
    insert_live(&c);
    let kind = if accepted { ConnKind::Accepted } else { ConnKind::Opened };
    push_event(ConnRecord { subject, kind, proto, local_port, remote, remote_port, bytes_in: 0, bytes_out: 0, time_ns: t });
    ConnHandle(c)
}

fn same(c: &Conn, r: &ConnRecord) -> bool {
    c.subject == r.subject && c.proto == r.proto && c.local_port == r.local_port && c.remote == r.remote && c.remote_port == r.remote_port
}

/// The view's side of `record_conn` (callers without a handle). UDP has no close there: such a peer is a past
/// entry at once ("seen"). A Closed record ends the newest live one with the same addresses that was made from
/// a record (never one a socket's handle holds), or is a past one of its own.
pub(crate) fn from_record(r: &ConnRecord) {
    let accepted = r.kind == ConnKind::Accepted;
    match r.kind {
        ConnKind::Opened | ConnKind::Accepted => {
            sweep_lost(r.time_ns);
            let c = new_conn(r.subject, r.proto, accepted, r.local_port, r.remote, r.remote_port, r.time_ns, ConnCounters::new(), false);
            if r.proto == NetProto::Udp {
                finish(&c, r);
            } else {
                insert_live(&c);
            }
        }
        ConnKind::Closed => {
            let found = STORE.lock().live.iter().rev().find(|c| !c.handled && same(c, r)).cloned();
            let c = found.unwrap_or_else(|| new_conn(r.subject, r.proto, false, r.local_port, r.remote, r.remote_port, r.time_ns, ConnCounters::new(), false));
            finish(&c, r);
        }
    }
}

fn finish(c: &Arc<Conn>, r: &ConnRecord) {
    c.bytes.bytes_in.fetch_max(r.bytes_in, Ordering::Relaxed);
    c.bytes.bytes_out.fetch_max(r.bytes_out, Ordering::Relaxed);
    let t = r.time_ns.max(c.first_ns).max(1);
    if c.closed_ns.compare_exchange(0, t, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        c.last_ns.fetch_max(t, Ordering::Relaxed);
        retire(c);
        ended(c, t);
    }
}

/// The journal's record of a connection (RB-8 reads it from the event stream). Once the names are stored (at
/// the open, `Owners`, or by the view's caller, `set_program`): `comm` is the process's comm (Linux's meaning, as before PM-2b) and `exe`
/// the file name of the executable it runs; '-' before.
fn ended(c: &Conn, t: u64) -> (u64, u64) {
    let (bin, bout) = c.bytes.get();
    let (name, exe, comm) = {
        let l = c.labels.lock();
        (l.name.clone(), l.program_name.clone(), l.comm.clone().or_else(|| l.program_name.clone()))
    };
    let peer = format!("{}:{}", c.remote, c.remote_port);
    let (name, exe, comm) = (name.as_deref().unwrap_or("-"), exe.as_deref().unwrap_or("-"), comm.as_deref().unwrap_or("-"));
    let (program, pid, proto, acc) = (c.subject.program.0, c.subject.pid, proto_name(c.proto), c.accepted as u32);
    // Info: what a program or BitsOS opened, and connections peers opened to a program's TCP server. Debug: the
    // ones peers opened to the kernel (each MCP call is one) and UDP peers that sent first (anyone can).
    if !c.accepted || (pid != 0 && c.proto != NetProto::Udp) {
        klog::event!(Info, "pm", "conn.closed", program = program, pid = pid, comm = comm, exe = exe, proto = proto, peer = peer, name = name,
            local_port = c.local_port, accepted = acc, bytes_in = bin, bytes_out = bout, first_ns = c.first_ns, last_ns = t);
    } else {
        klog::event!(Debug, "pm", "conn.closed", program = program, pid = pid, comm = comm, exe = exe, proto = proto, peer = peer, name = name,
            local_port = c.local_port, accepted = acc, bytes_in = bin, bytes_out = bout, first_ns = c.first_ns, last_ns = t);
    }
    (bin, bout)
}

/// How long a live entry made from a record (no socket holds it) waits for its Closed record before the view
/// ends it as lost: a caller that never sends Closed must not leave live entries forever. A setting.
static LOST_NS: AtomicU64 = AtomicU64::new(30 * 60 * 1_000_000_000);

pub fn set_record_live_ns(ns: u64) {
    LOST_NS.store(ns.max(1), Ordering::Relaxed);
}

pub fn record_live_ns() -> u64 {
    LOST_NS.load(Ordering::Relaxed)
}

/// Ends record-made live entries older than `record_live_ns` (a few per call; runs on each new record and each
/// query, so the backlog never grows). Each is a `pm conn.lost` event.
fn sweep_lost(t: u64) {
    let max_age = record_live_ns();
    let mut lost: Vec<Arc<Conn>> = Vec::with_capacity(8);
    {
        let s = STORE.lock();
        for c in s.live.iter().filter(|c| !c.handled && t.saturating_sub(c.first_ns) > max_age) {
            if lost.len() == lost.capacity() {
                break;
            }
            lost.push(c.clone());
        }
    }
    for c in lost {
        klog::event!(Warn, "pm", "conn.lost", program = c.subject.program.0, pid = c.subject.pid, proto = proto_name(c.proto),
            local_port = c.local_port, remote_port = c.remote_port; "no Closed record came for a recorded connection: ended");
        let (bin, bout) = c.bytes.get();
        finish(&c, &ConnRecord { subject: c.subject, kind: ConnKind::Closed, proto: c.proto, local_port: c.local_port, remote: c.remote,
            remote_port: c.remote_port, bytes_in: bin, bytes_out: bout, time_ns: t });
    }
}

/// Forgets everything kept about process `pid`: its past connections, its event records and the names this
/// pid resolved (live ones stay with their sockets; names other processes of its program resolved stay too). Only for rerunnable tests with pids of their own, which must
/// not push the real history out of the kept lists.
pub fn forget_test_connections(subject: &Subject) {
    let pid = subject.pid;
    let n = STORE.lock().past.iter().filter(|c| c.subject.pid == pid).count();
    let mut gone: Vec<Arc<Conn>> = Vec::with_capacity(n);
    {
        let mut s = STORE.lock();
        let mut i = 0;
        while i < s.past.len() && gone.len() < gone.capacity() {
            if s.past[i].subject.pid == pid {
                gone.extend(s.past.remove(i));
            } else {
                i += 1;
            }
        }
    }
    drop(gone);
    crate::conn::forget_pid(pid);
    dns::forget(subject);
}

/// Forgets one peer of `subject`'s socket on `local_port`: its past connections and event records. Only for
/// rerunnable tests that must use a real subject (the kernel, pid 0), so nothing else of it is touched.
pub(crate) fn forget_test_peer(subject: &Subject, local_port: u16, remote: IpAddr, remote_port: u16) {
    let hit = |s: &Subject, l: u16, r: IpAddr, rp: u16| s == subject && l == local_port && r == remote && rp == remote_port;
    let n = STORE.lock().past.iter().filter(|c| hit(&c.subject, c.local_port, c.remote, c.remote_port)).count();
    let mut gone: Vec<Arc<Conn>> = Vec::with_capacity(n);
    {
        let mut s = STORE.lock();
        let mut i = 0;
        while i < s.past.len() && gone.len() < gone.capacity() {
            let c = &s.past[i];
            if hit(&c.subject, c.local_port, c.remote, c.remote_port) {
                gone.extend(s.past.remove(i));
            } else {
                i += 1;
            }
        }
    }
    drop(gone);
    crate::conn::forget_peer(subject, local_port, remote, remote_port);
}

/// Which connections a query wants. `text` matches (ASCII case-insensitively) part of the resolved name, the
/// peer address or the program's name.
#[derive(Clone, Copy, Default, Debug)]
pub struct ConnFilter<'a> {
    pub live_only: bool,
    pub program: Option<ProgramId>,
    pub pid: Option<u32>,
    /// Only BitsOS's own parts (kernel and ProgramId 0) or only the others.
    pub bitsos: Option<bool>,
    pub text: Option<&'a str>,
}

impl ConnFilter<'_> {
    fn wants(&self, c: &Conn) -> bool {
        self.program.is_none_or(|p| p == c.subject.program)
            && self.pid.is_none_or(|p| p == c.subject.pid)
            && self.bitsos.is_none_or(|b| b == (c.subject.program == ProgramId::SYSTEM))
    }
}

/// One connection as the view sees it now.
#[derive(Clone)]
pub struct ConnInfo {
    pub id: u64,
    pub subject: Subject,
    pub proto: NetProto,
    pub accepted: bool,
    pub local_port: u16,
    pub remote: IpAddr,
    pub remote_port: u16,
    pub name: Option<Arc<str>>,
    /// The file name of the executable the process runs (the view's caller stores it, `set_program`).
    pub program_name: Option<Arc<str>>,
    /// The process's comm, only when it differs from `program_name`.
    pub comm: Option<Arc<str>>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub first_ns: u64,
    pub last_ns: u64,
    pub closed_ns: Option<u64>,
    conn: Arc<Conn>,
}

impl ConnInfo {
    pub fn live(&self) -> bool {
        self.closed_ns.is_none()
    }

    /// Remembers the program's name with the connection (the view's caller knows processes; policy does not),
    /// so it stays after the process has ended. The name is also the comm: a comm stored before is dropped,
    /// so header, filter and `conn.closed` never mix a new name with an old comm.
    pub fn set_program_name(&mut self, n: &str) {
        self.set_program(n, n);
    }

    /// `set_program_name` with the process's comm, kept only when it differs from the name.
    pub fn set_program(&mut self, name: &str, comm: &str) {
        let (n, c): (Arc<str>, Option<Arc<str>>) = (Arc::from(name), (comm != name).then(|| Arc::from(comm)));
        {
            let mut l = self.conn.labels.lock();
            l.program_name = Some(n.clone());
            l.comm = c.clone();
        }
        self.program_name = Some(n);
        self.comm = c;
    }

    fn of(c: Arc<Conn>, t: u64) -> ConnInfo {
        let closed = c.closed_ns.load(Ordering::Acquire);
        let (bin, bout) = c.bytes.get();
        if closed == 0 && c.seen.swap(bin + bout, Ordering::Relaxed) != bin + bout {
            c.last_ns.fetch_max(t, Ordering::Relaxed);
        }
        // An answer that came after the connection began names it now.
        let late = if c.labels.lock().name.is_none() { dns::name_of(&c.subject, c.remote) } else { None };
        let (name, program_name, comm) = {
            let mut l = c.labels.lock();
            if late.is_some() {
                l.name = late;
            }
            (l.name.clone(), l.program_name.clone(), l.comm.clone())
        };
        ConnInfo {
            id: c.id,
            subject: c.subject,
            proto: c.proto,
            accepted: c.accepted,
            local_port: c.local_port,
            remote: c.remote,
            remote_port: c.remote_port,
            name,
            program_name,
            comm,
            bytes_in: bin,
            bytes_out: bout,
            first_ns: c.first_ns,
            last_ns: c.last_ns.load(Ordering::Relaxed),
            closed_ns: (closed != 0).then_some(closed),
            conn: c,
        }
    }

    /// `text` is part of the name, the program's name, its comm or the peer (ASCII case-insensitively).
    pub fn matches(&self, text: &str) -> bool {
        let has = |h: &str| h.as_bytes().windows(text.len()).any(|w| w.eq_ignore_ascii_case(text.as_bytes()));
        text.is_empty()
            || self.name.as_deref().is_some_and(has)
            || self.program_name.as_deref().is_some_and(has)
            || self.comm.as_deref().is_some_and(has)
            || has(&format!("{}:{}", self.remote, self.remote_port))
    }
}

/// Keeps at most `n` past connections (`set_conn_keep`).
pub(crate) fn trim(n: usize) {
    let mut gone: Vec<Arc<Conn>> = Vec::new();
    let mut s = STORE.lock();
    let extra = s.past.len().saturating_sub(n.max(1));
    if extra > 0 {
        drop(s);
        gone.reserve(extra);
        s = STORE.lock();
    }
    while s.past.len() > n.max(1) && gone.len() < gone.capacity() {
        gone.extend(s.past.pop_front());
    }
    drop(s);
}

/// Live and past connections now kept.
pub fn connection_counts() -> (usize, usize) {
    let s = STORE.lock();
    (s.live.len(), s.past.len())
}

/// The connections `f` wants, live ones first (newest first), then past ones (newest close first), at most
/// `max`. With the default filter and `usize::MAX` it is everything kept (the export for the journal).
pub fn connections(f: &ConnFilter<'_>, max: usize) -> Vec<ConnInfo> {
    sweep_lost(now());
    let (live, past) = connection_counts();
    let want = if f.live_only { live } else { live + past };
    // Allocated before the lock; connections opened meanwhile beyond the slack are left for the next query.
    let mut picked: Vec<Arc<Conn>> = Vec::with_capacity(want + 16);
    {
        let s = STORE.lock();
        let past = s.past.iter().rev().filter(|_| !f.live_only);
        for c in s.live.iter().rev().chain(past).filter(|c| f.wants(c)) {
            if picked.len() == picked.capacity() {
                break;
            }
            picked.push(c.clone());
        }
    }
    let t = now();
    let mut live: Vec<ConnInfo> = Vec::new();
    let mut out: Vec<ConnInfo> = Vec::new();
    for c in picked {
        let i = ConnInfo::of(c, t);
        if f.text.is_some_and(|x| !i.matches(x)) {
            continue;
        }
        if i.live() { live.push(i) } else { out.push(i) }
    }
    live.sort_by_key(|c| core::cmp::Reverse(c.first_ns));
    out.sort_by_key(|c| core::cmp::Reverse(c.closed_ns));
    live.append(&mut out);
    live.truncate(max);
    live
}

impl core::fmt::Display for IpAddr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            IpAddr::V4(a) => write!(f, "{}.{}.{}.{}", a[0], a[1], a[2], a[3]),
            IpAddr::V6(a) => {
                let w: Vec<u16> = a.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if w[..5].iter().all(|&x| x == 0) && w[5] == 0xffff {
                    return write!(f, "::ffff:{}.{}.{}.{}", a[12], a[13], a[14], a[15]);
                }
                // RFC 5952: the longest run of two or more zero groups (the first if tied) becomes "::".
                let (mut best, mut cur) = ((0usize, 0usize), (0usize, 0usize));
                for (i, &x) in w.iter().enumerate() {
                    if x == 0 {
                        if cur.1 == 0 {
                            cur.0 = i;
                        }
                        cur.1 += 1;
                        if cur.1 > best.1 {
                            best = cur;
                        }
                    } else {
                        cur.1 = 0;
                    }
                }
                let mut s = String::new();
                let mut i = 0;
                while i < 8 {
                    if best.1 >= 2 && i == best.0 {
                        s.push_str("::");
                        i += best.1;
                        continue;
                    }
                    if !s.is_empty() && !s.ends_with(':') {
                        s.push(':');
                    }
                    s.push_str(&format!("{:x}", w[i]));
                    i += 1;
                }
                f.write_str(&s)
            }
        }
    }
}
