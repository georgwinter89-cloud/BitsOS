//! The console mirror (RB-4, spec net.md 5.3, core.md 10.3; V1 `net/mirror*.asm`): every rendered log line as
//! UDP to the listener CONFIG names (`mirror=a.b.c.d[:port]`, V1's `console=` too; R9: nothing without it), in
//! V1's wire format (`wire`), so the unchanged `console-listen.mjs` on the Gaming-PC shows the log from the
//! first line of the boot.
//!
//! - **Sink:** a klog deferred sink registered with `replay`, so it gets every record since boot; lines are
//!   only appended to the mirror's own byte ring (`buf`, sized from RAM), so the emitter and klogd never wait
//!   for the network.
//! - **Before the network:** the ring collects. The `mirror` thread (class Normal, `kernel.rs`) waits for a
//!   card with an address and a route to the listener, then sends the start datagram (`B`, position = the
//!   oldest whole line still held) and the whole backlog, then follows the ring. klogd kicks it after each drain.
//! - **Emergency path** (`Sink::write_emergency`, i.e. `try_event!` on panic/NMI paths): no waiting lock, no
//!   allocation, no stack. If the ring's lock is free the line joins the log stream and everything not yet out
//!   goes as raw Ethernet/IPv4/UDP frames through `NetDevice::poll_tx` (the card and addresses come from a
//!   snapshot the thread keeps); if the lock is held (the panic interrupted its holder) the line goes alone in
//!   the screen stream (`S`, its own positions), which the listener shows as a `scr|` line. `flush_emergency`
//!   then gives the card `SETTLE_NS` to send.
//! - **Quiesce** (Reload, reboot): what waits and the end datagram (`E`) go out through `poll_tx` (interrupts are
//!   off there, the stack's thread no longer runs), then the mirror is off; registered once live, so it comes
//!   before the network cards in `quiesce_all` (newest first). `Mirror::resume` (for RB-5b's way back after a
//!   failed Reload, once the cards run again) lets the thread start a new listener view (new `B`, new id)
//!   from where the stop ended.
//!
//! Locks: one `IrqSpinLock` ("klog.mirror", klog's level) around the ring, held only to copy bytes; the
//! emergency path only tries it. The route snapshot lives in two slots ("klog.mirror.route", never held
//! together with the ring's lock): the thread fills the spare one and flips the index, the emergency path only
//! tries them, so it never waits and memory stays at two snapshots however often the route changes.
//! Positions make every loss visible on the listener: bytes the ring overwrote before they went out appear
//! there as `[... n bytes lost]`; records klog lost before the mirror saw them as klog's own
//! `[... n records lost]` line.

pub mod buf;
#[cfg(target_os = "none")]
mod kernel;
pub mod ktests;
pub mod wire;

use alloc::sync::Arc;
use buf::ByteRing;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use devmodel::net::NetDevice;
use klog::{Level, Sink};
use ksync::{IrqSpinLock, OnceRef, WaitQueue};
use net::{Ipv4, NetError, SockAddr, UdpSocket};
use wire::{CHUNK, DGRAM_MAX, FRAME_HEAD, FRAME_MAX, FrameAddr, HEAD, Kind, Target};

#[cfg(target_os = "none")]
pub use kernel::kick;

/// How long `flush_emergency` lets the card send after the last emergency frame (V1 `MIRROR_FIFO_MS`).
pub const SETTLE_NS: u64 = 20_000_000;
/// Quiesce: everything waiting plus the end datagram within this time (V1 `MIRROR_STOP_MS`).
pub const STOP_NS: u64 = 200_000_000;
/// The first emergency send after a quiet second also repeats this much of what already went out: datagrams
/// the stack queued but its thread never sent (a panic stops it). The listener drops what it has already.
pub const RESEND_TAIL: u64 = 8 << 10;
/// An emergency episode ends after this much quiet (the next one repeats the tail again).
const EPISODE_NS: u64 = 1_000_000_000;
/// Recent emergency lines remembered so klogd does not send them a second time (a cache, not a limit).
const RECENT: usize = 8;

klog::counter!(DATAGRAMS, "mirror.datagrams");
klog::counter!(BYTES, "mirror.bytes");
klog::counter!(LOST_BYTES, "mirror.lost_bytes");
klog::counter!(SEND_ERRORS, "mirror.send_errors");
klog::counter!(EMERGENCY_FRAMES, "mirror.emergency_frames");
klog::counter!(EMERGENCY_DROPPED, "mirror.emergency_dropped");
klog::counter!(EMERGENCY_SCREEN, "mirror.emergency_screen_lines");
klog::counter!(STOPS, "mirror.stops");
klog::counter!(STOP_FAILURES, "mirror.stops_without_end");

/// Where emergency frames go out: the card routing picked for the listener, our address and MACs.
#[derive(Clone)]
pub struct RouteInfo {
    pub card: Arc<dyn NetDevice>,
    pub src_ip: [u8; 4],
    pub src_mac: [u8; 6],
    /// The next hop's MAC when known, else Ethernet broadcast (change request to NW in the notes).
    pub dst_mac: [u8; 6],
}

impl RouteInfo {
    fn same(&self, o: &RouteInfo) -> bool {
        core::ptr::addr_eq(Arc::as_ptr(&self.card), Arc::as_ptr(&o.card))
            && self.src_ip == o.src_ip
            && self.src_mac == o.src_mac
            && self.dst_mac == o.dst_mac
    }
}

struct State {
    ring: ByteRing,
}

pub struct Mirror {
    target: Target,
    clock: fn() -> u64,
    st: IrqSpinLock<State>,
    live: AtomicBool,
    id: AtomicU32,
    /// Positions of the screen stream (only emergency lines that could not join the log stream).
    scr_pos: AtomicU64,
    /// Two route snapshots; `route_cur` names the current one (see the module text).
    routes: [IrqSpinLock<Option<RouteInfo>>; 2],
    route_cur: AtomicUsize,
    /// Stopped by a quiesce (not just never live); `resume` clears it and asks the thread for a new start.
    stopped: AtomicBool,
    resume_req: AtomicBool,
    /// The last stop's report, packed (`StopReport::pack`), 0 = none waiting: a Quiesce never logs (CONTRACTS,
    /// PC-2), so the thread logs it when it runs again.
    stop_note: AtomicU64,
    ip_id: AtomicU32,
    recent: [AtomicU64; RECENT],
    recent_next: AtomicUsize,
    last_emergency_ns: AtomicU64,
    kicked: AtomicBool,
    wq: WaitQueue,
}

fn fnv(s: &str) -> u64 {
    let mut h = kexec::Fnv::new();
    h.update(s.as_bytes());
    h.finish() | 1
}

/// What one emergency call did (tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EmergencyReport {
    pub frames: u32,
    pub dropped: bool,
    pub screen: bool,
}

/// What a quiesce did (logged by `MirrorQuiesce`, checked by tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StopReport {
    /// It was live (else nothing happened).
    pub was_live: bool,
    /// The end datagram went out.
    pub ended: bool,
    /// Raw frames the card took (the end datagram included).
    pub frames: u32,
    /// The card refused a frame at least once.
    pub dropped: bool,
    /// No route snapshot: nothing could be sent.
    pub no_route: bool,
    /// `STOP_NS` ran out before the end datagram went out.
    pub timed_out: bool,
}

impl StopReport {
    fn pack(&self) -> u64 {
        let flags = [self.was_live, self.ended, self.dropped, self.no_route, self.timed_out];
        let bits = flags.iter().enumerate().fold(0u64, |a, (i, &f)| a | (f as u64) << i);
        (self.frames as u64) << 32 | bits
    }
    fn unpack(v: u64) -> StopReport {
        let f = |i: u32| v >> i & 1 == 1;
        StopReport { was_live: f(0), ended: f(1), dropped: f(2), no_route: f(3), timed_out: f(4), frames: (v >> 32) as u32 }
    }
}

impl Mirror {
    /// A mirror to `target` with a ring of `ring_bytes`; None without the memory.
    pub fn new(target: Target, ring_bytes: usize, clock: fn() -> u64) -> Option<Mirror> {
        Some(Mirror {
            target,
            clock,
            st: IrqSpinLock::named("klog.mirror", State { ring: ByteRing::new(ring_bytes)? }),
            live: AtomicBool::new(false),
            id: AtomicU32::new(0),
            scr_pos: AtomicU64::new(0),
            routes: [IrqSpinLock::named("klog.mirror.route", None), IrqSpinLock::named("klog.mirror.route", None)],
            route_cur: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            resume_req: AtomicBool::new(false),
            stop_note: AtomicU64::new(0),
            ip_id: AtomicU32::new(0),
            recent: [const { AtomicU64::new(0) }; RECENT],
            recent_next: AtomicUsize::new(0),
            last_emergency_ns: AtomicU64::new(0),
            kicked: AtomicBool::new(false),
            wq: WaitQueue::new(),
        })
    }

    pub fn target(&self) -> Target {
        self.target
    }
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    pub fn id(&self) -> u32 {
        self.id.load(Ordering::Acquire)
    }
    fn to(&self) -> SockAddr {
        SockAddr { ip: Ipv4(self.target.ip), port: self.target.port }
    }

    /// (written, sent, oldest, lost, capacity) of the ring (tests, status).
    pub fn ring_state(&self) -> (u64, u64, u64, u64, usize) {
        let g = self.st.lock();
        (g.ring.written(), g.ring.sent(), g.ring.oldest(), g.ring.lost(), g.ring.capacity())
    }

    /// The ring's bytes from `from` (tests: what the mirror holds).
    pub fn ring_text(&self, from: u64, max: usize) -> alloc::vec::Vec<u8> {
        let g = self.st.lock();
        let from = from.max(g.ring.oldest());
        (from..g.ring.written()).take(max).filter_map(|p| g.ring.at(p)).collect()
    }

    /// Back to a fresh, not live mirror (ktests reuse one instance per boot instead of leaking a ring per run).
    pub(crate) fn reset(&self) {
        self.live.store(false, Ordering::Release);
        self.stopped.store(false, Ordering::Release);
        self.resume_req.store(false, Ordering::Release);
        self.st.lock().ring.clear();
        self.scr_pos.store(0, Ordering::Release);
        self.last_emergency_ns.store(0, Ordering::Release);
        self.recent.iter().for_each(|r| r.store(0, Ordering::Release));
    }

    fn append(ring: &mut ByteRing, line: &str) {
        wire::encode(line, |b| ring.push(b));
        ring.push(b'\n');
    }

    fn remember(&self, h: u64) {
        let i = self.recent_next.fetch_add(1, Ordering::Relaxed) % RECENT;
        self.recent[i].store(h, Ordering::Release);
    }

    /// True (once) if `line` went out by the emergency path already.
    fn seen_in_emergency(&self, line: &str) -> bool {
        if self.recent.iter().all(|r| r.load(Ordering::Acquire) == 0) {
            return false;
        }
        let h = fnv(line);
        self.recent.iter().any(|r| r.compare_exchange(h, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok())
    }

    // ------------------------------------------------------------------------------------------------------
    // Start and the normal path (the mirror thread)

    /// Begins a start: the log stream goes out from the oldest whole line the ring holds (V1), or after a stop
    /// from where the stop ended (the listener has everything before it). Returns the start datagram's length
    /// in `out`, the start position and the bytes the ring had overwritten before they went to anyone.
    pub fn begin(&self, id: u32, reload: u32, src_ip: [u8; 4], out: &mut [u8]) -> Option<(usize, u64, u64)> {
        let mut g = self.st.lock();
        let before = g.ring.sent();
        let start = if before > 0 && before >= g.ring.oldest() { before } else { g.ring.line_start() };
        g.ring.start_at(start);
        self.id.store(id, Ordering::Release);
        // A new start: the listener counts the screen stream from 0 again.
        self.scr_pos.store(0, Ordering::Release);
        let n = wire::begin(out, id, start, reload, src_ip)?;
        Some((n, start, start.saturating_sub(before)))
    }

    /// The way back after a quiesce whose Reload or reboot did not happen (RB-5b calls it once the cards run
    /// again): the thread starts a new listener view. False if the mirror was not stopped.
    pub fn resume(&self) -> bool {
        if !self.stopped.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.resume_req.store(true, Ordering::Release);
        self.kick();
        true
    }

    /// The thread takes a resume request (once).
    pub fn take_resume(&self) -> bool {
        self.resume_req.swap(false, Ordering::AcqRel)
    }

    /// The last stop's report, once (the thread logs it after the stop; None if nothing stopped since).
    pub fn take_stop_report(&self) -> Option<StopReport> {
        match self.stop_note.swap(0, Ordering::AcqRel) {
            0 => None,
            v => Some(StopReport::unpack(v)),
        }
    }

    /// The thread has sent the start datagram: from now on it sends, and so may the emergency path.
    pub fn set_live(&self, on: bool) {
        self.live.store(on, Ordering::Release);
    }

    /// Sends what waits through `sock`, at most `budget` datagrams. Returns the datagrams sent and the bytes
    /// lost to overwriting meanwhile; a send error stops it (what failed stays waiting).
    pub fn flush(&self, sock: &dyn UdpSocket, budget: usize) -> Result<(usize, u64), NetError> {
        let mut buf = [0u8; DGRAM_MAX];
        let (mut sent, mut lost) = (0, 0);
        while sent < budget && self.is_live() {
            let (n, end) = {
                let mut g = self.st.lock();
                lost += g.ring.skip_overwritten();
                let from = g.ring.sent();
                let k = g.ring.copy(from, &mut buf[HEAD..HEAD + CHUNK]);
                if k == 0 {
                    break;
                }
                wire::head(&mut buf, Kind::Log, self.id(), from);
                (HEAD + k, from + k as u64)
            };
            if let Err(e) = sock.send_to(&buf[..n], self.to()) {
                SEND_ERRORS.inc();
                LOST_BYTES.add(lost);
                return Err(e);
            }
            self.st.lock().ring.mark_sent(end);
            DATAGRAMS.inc();
            BYTES.add((n - HEAD) as u64);
            sent += 1;
        }
        LOST_BYTES.add(lost);
        Ok((sent, lost))
    }

    /// Bytes waiting to go out.
    pub fn pending(&self) -> u64 {
        self.st.lock().ring.pending()
    }

    /// Publishes where emergency frames go (None: no route right now). One writer (the mirror thread): it
    /// fills the spare slot, then makes it current. Returns whether anything changed.
    pub fn set_route(&self, r: Option<RouteInfo>) -> bool {
        let cur = self.route_cur.load(Ordering::Acquire) & 1;
        let same = match (self.routes[cur].lock().as_ref(), r.as_ref()) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return false;
        }
        let old = core::mem::replace(&mut *self.routes[cur ^ 1].lock(), r);
        self.route_cur.store(cur ^ 1, Ordering::Release);
        // Outside the slot's lock: the last reference to a card may go here.
        drop(old);
        true
    }

    /// The route emergency frames use now (a copy: the card's Arc, no allocation). Never waits: a slot busy on
    /// another CPU is skipped (the other one holds the snapshot before it).
    pub fn route(&self) -> Option<RouteInfo> {
        let cur = self.route_cur.load(Ordering::Acquire) & 1;
        [cur, cur ^ 1].into_iter().find_map(|i| self.routes[i].try_lock()).and_then(|g| g.clone())
    }

    /// The mirror thread's idle wait: until klogd kicks or `timeout_ns`.
    pub fn wait_kick(&self, timeout_ns: u64) {
        self.wq.wait_until_timeout(|| self.kicked.swap(false, Ordering::AcqRel), timeout_ns);
    }

    /// klogd fed the sinks: the thread looks at once.
    pub fn kick(&self) {
        if !self.kicked.swap(true, Ordering::AcqRel) {
            self.wq.wake_one();
        }
    }

    // ------------------------------------------------------------------------------------------------------
    // The raw path: emergency lines and quiesce

    fn addr(&self, r: &RouteInfo) -> FrameAddr {
        FrameAddr {
            src_mac: r.src_mac,
            dst_mac: r.dst_mac,
            src_ip: r.src_ip,
            dst_ip: self.target.ip,
            src_port: wire::DEFAULT_PORT,
            dst_port: self.target.port,
        }
    }

    fn send_frame(&self, r: &RouteInfo, f: &mut [u8; FRAME_MAX], dgram_len: usize) -> bool {
        let id = self.ip_id.fetch_add(1, Ordering::Relaxed) as u16;
        let Some(n) = wire::frame(f, &self.addr(r), id, dgram_len) else { return false };
        let ok = r.card.poll_tx(&f[..n]);
        if ok { EMERGENCY_FRAMES.inc() } else { EMERGENCY_DROPPED.inc() }
        ok
    }

    /// Everything from `from` to the ring's end as raw frames; stops at the first frame the card refuses (or
    /// when the ring's lock is busy). The lock is held only to copy a piece, never while the card is called,
    /// so the driver's own TX lock never nests inside it. Returns the ring's end it reached (everything before
    /// it is out), None if it stopped early.
    fn send_raw(&self, r: &RouteInfo, from: u64, rep: &mut EmergencyReport) -> Option<u64> {
        let mut f = [0u8; FRAME_MAX];
        let mut pos = from;
        loop {
            let k = {
                let g = self.st.try_lock()?;
                pos = pos.max(g.ring.oldest());
                g.ring.copy(pos, &mut f[FRAME_HEAD + HEAD..FRAME_HEAD + HEAD + CHUNK])
            };
            if k == 0 {
                return Some(pos);
            }
            wire::head(&mut f[FRAME_HEAD..], Kind::Log, self.id(), pos);
            if !self.send_frame(r, &mut f, HEAD + k) {
                rep.dropped = true;
                return None;
            }
            rep.frames += 1;
            pos += k as u64;
            if let Some(mut g) = self.st.try_lock() {
                g.ring.mark_sent(pos);
            }
        }
    }

    /// The line alone in the screen stream (the ring's lock is held by someone this path must not wait for).
    fn send_screen_line(&self, r: &RouteInfo, line: &str, rep: &mut EmergencyReport) {
        let mut f = [0u8; FRAME_MAX];
        let body = &mut f[FRAME_HEAD + HEAD..FRAME_HEAD + HEAD + CHUNK];
        let mut n = 0;
        wire::encode(line, |b| {
            if n + 1 < body.len() {
                body[n] = b;
                n += 1;
            }
        });
        body[n] = b'\n';
        n += 1;
        let pos = self.scr_pos.fetch_add(n as u64, Ordering::AcqRel);
        wire::head(&mut f[FRAME_HEAD..], Kind::Screen, self.id(), pos);
        rep.screen = true;
        EMERGENCY_SCREEN.inc();
        if self.send_frame(r, &mut f, HEAD + n) {
            rep.frames += 1;
        } else {
            rep.dropped = true;
        }
    }

    /// The emergency path for one line (see the module text). Never waits, never allocates.
    pub fn emergency(&self, line: &str) -> EmergencyReport {
        let mut rep = EmergencyReport::default();
        let now = (self.clock)();
        let episode = now.saturating_sub(self.last_emergency_ns.load(Ordering::Acquire)) > EPISODE_NS;
        let route = if self.is_live() { self.route() } else { None };
        // Into the log stream if the ring is free (it then also goes out with what waited before it).
        let from = self.st.try_lock().map(|mut g| {
            self.remember(fnv(line));
            Self::append(&mut g.ring, line);
            LOST_BYTES.add(g.ring.skip_overwritten());
            if episode { g.ring.sent().saturating_sub(RESEND_TAIL) } else { g.ring.sent() }
        });
        if let Some(r) = route.as_ref() {
            match from {
                Some(from) => {
                    let _ = self.send_raw(r, from, &mut rep);
                }
                None => self.send_screen_line(r, line, &mut rep),
            }
        }
        if rep.frames > 0 {
            self.last_emergency_ns.store(now.max(1), Ordering::Release);
        }
        rep
    }

    /// Gives the card `SETTLE_NS` after the last emergency frame (busy wait: the panic path has no scheduler).
    pub fn settle(&self) {
        let last = self.last_emergency_ns.load(Ordering::Acquire);
        if last == 0 {
            return;
        }
        let until = last.saturating_add(SETTLE_NS);
        while (self.clock)() < until {
            core::hint::spin_loop();
        }
    }

    /// Quiesce: what waits and the end datagram out through `poll_tx` within `STOP_NS`, then off (V1
    /// `mirror_stop`). Interrupts are off and the stack's thread does not run any more. The end datagram names
    /// the end the raw path reached, so a line appended meanwhile is not shown as lost (after a resume it goes
    /// out under the new start; on a real Reload it is gone either way).
    pub fn stop(&self) -> StopReport {
        let out = self.stop_inner();
        if out.was_live {
            // B6 without logging here: counters now, the event from the thread when it runs again.
            STOPS.inc();
            if !out.ended {
                STOP_FAILURES.inc();
            }
            self.stop_note.store(out.pack(), Ordering::Release);
        }
        out
    }

    fn stop_inner(&self) -> StopReport {
        let mut out = StopReport::default();
        if !self.live.swap(false, Ordering::AcqRel) {
            return out;
        }
        out.was_live = true;
        self.stopped.store(true, Ordering::Release);
        let Some(r) = self.route() else {
            out.no_route = true;
            return out;
        };
        let deadline = (self.clock)().saturating_add(STOP_NS);
        let mut rep = EmergencyReport::default();
        let mut tail = true;
        while (self.clock)() < deadline {
            let from = self.st.try_lock().map(|mut g| {
                LOST_BYTES.add(g.ring.skip_overwritten());
                if tail { g.ring.sent().saturating_sub(RESEND_TAIL) } else { g.ring.sent() }
            });
            if let Some(from) = from
                && let Some(log_end) = self.send_raw(&r, from, &mut rep)
            {
                let mut f = [0u8; FRAME_MAX];
                if let Some(n) = wire::end(&mut f[FRAME_HEAD..], self.id(), self.scr_pos.load(Ordering::Acquire), log_end)
                    && self.send_frame(&r, &mut f, n)
                {
                    out.ended = true;
                    break;
                }
            }
            // The ring's lock was busy or the card's ring full: it frees descriptors as it sends.
            tail = false;
            drop_wait(self.clock, 50_000);
        }
        out.timed_out = !out.ended;
        out.frames = rep.frames + out.ended as u32;
        out.dropped = rep.dropped;
        if out.frames > 0 {
            drop_wait(self.clock, SETTLE_NS);
        }
        out
    }
}

fn drop_wait(clock: fn() -> u64, ns: u64) {
    let until = clock().saturating_add(ns);
    while clock() < until {
        core::hint::spin_loop();
    }
}

impl Sink for Mirror {
    fn write_line(&self, _level: Level, line: &str) {
        if self.seen_in_emergency(line) {
            return;
        }
        Self::append(&mut self.st.lock().ring, line);
    }
    fn write_emergency(&self, _level: Level, line: &str) {
        self.emergency(line);
    }
    fn flush_emergency(&self) {
        self.settle();
    }
}

/// The kernel's mirror, once CONFIG named a listener and the ring could be allocated.
static MIRROR: OnceRef<Mirror> = OnceRef::new();

pub fn mirror() -> Option<&'static Mirror> {
    MIRROR.get()
}

/// The `devmodel::Quiesce` face of the mirror (registered when it goes live).
pub struct MirrorQuiesce(pub &'static Mirror);

impl devmodel::Quiesce for MirrorQuiesce {
    fn name(&self) -> &str {
        "mirror"
    }
    /// No klog here (drivers' Quiesce never logs): `stop` keeps its report for the thread and counts.
    fn quiesce(&self) {
        self.0.stop();
    }
}

#[cfg(test)]
mod tests;
