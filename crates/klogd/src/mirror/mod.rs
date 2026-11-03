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
//! - **Pause** (RB-15, a reboot or power-off: `klog::shutdown`): before the storage stop and the CPU park the
//!   thread leaves the stack (it finishes the datagram it is in, then idles), the stack gets a moment for what
//!   it queued, and from then on what is logged goes out on the raw path at each shutdown step, the stack's
//!   last 8 KiB repeated once. After each raw round the mirror asks the card (`klog::shutdown::TxDone`) until it
//!   has sent everything: no mirror datagram is half-way through smoltcp or the card's NAPI path when the CPUs
//!   are parked, and none is in the card's DMA when the reset comes (the Mini-PC's FCH answers a reset with
//!   PCIe DMA in flight with a sync flood, desk 28.10.).
//! - **Quiesce** (Reload, reboot): what waits and the end datagram (`E`) go out through `poll_tx` (interrupts are
//!   off there, the stack's thread no longer runs), the card is asked until it has sent them, then the mirror is
//!   off; registered once live, so it comes before the network cards in `quiesce_all` (newest first).
//!   `Mirror::resume` (for RB-5b's way back after a failed Reload, once the cards run again) lets the thread
//!   start a new listener view (new `B`, new id) from where the stop ended.
//!
//! Locks: one `IrqSpinLock` ("klog.mirror", klog's level) around the ring, held only to copy bytes; the
//! emergency path only tries it. The route snapshot lives in two slots ("klog.mirror.route", never held
//! together with the ring's lock): the thread fills the spare one and flips the index, the emergency path only
//! tries them, so it never waits and memory stays at two snapshots however often the route changes.
//! Positions make every loss visible on the listener: bytes the ring overwrote before they went out appear
//! there as `[... n bytes lost]`; records klog lost before the mirror saw them as klog's own
//! `[... n records lost]` line.

pub mod buf;
mod crumbs;
#[cfg(target_os = "none")]
mod kernel;
pub mod ktests;
pub mod wire;

use alloc::sync::Arc;
use buf::ByteRing;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use devmodel::net::NetDevice;
use klog::shutdown::{MirrorHook, PauseReport, TxDone};
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
/// Pause: how long the thread may take to finish the datagram it is sending.
pub const PAUSE_WAIT_NS: u64 = 200_000_000;
/// Pause: the stack's moment for the datagrams the thread queued (its own thread still runs then).
pub const STACK_SETTLE_NS: u64 = 20_000_000;
/// How long the card may take to report the raw frames sent (TDH == TDT, every descriptor written back).
pub const DONE_NS: u64 = 100_000_000;
/// After the card said done: the last frame's way from the card's FIFO to the wire.
pub const DONE_SETTLE_NS: u64 = 1_000_000;
/// Between two looks at a busy ring lock, a refusing card or a card not yet done.
const RAW_RETRY_NS: u64 = 50_000;
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
klog::counter!(PAUSES, "mirror.pauses");
klog::counter!(PAUSE_THREAD_BUSY, "mirror.pause_thread_busy");
klog::counter!(DONE_TIMEOUTS, "mirror.card_done_timeouts");
klog::counter!(DONE_UNKNOWN, "mirror.card_done_unknown");

/// Where emergency frames go out: the card routing picked for the listener, our address and MACs.
#[derive(Clone)]
pub struct RouteInfo {
    pub card: Arc<dyn NetDevice>,
    pub src_ip: [u8; 4],
    pub src_mac: [u8; 6],
    /// The next hop's MAC when known, else Ethernet broadcast (change request to NW in the notes).
    pub dst_mac: [u8; 6],
    /// The card's "everything sent" (RB-15; None: this card cannot say, the mirror settles by time).
    pub done: Option<Arc<dyn TxDone>>,
}

impl RouteInfo {
    fn same(&self, o: &RouteInfo) -> bool {
        core::ptr::addr_eq(Arc::as_ptr(&self.card), Arc::as_ptr(&o.card))
            && self.src_ip == o.src_ip
            && self.src_mac == o.src_mac
            && self.dst_mac == o.dst_mac
            && self.done.is_some() == o.done.is_some()
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
    /// RB-15: the normal path is off (a reboot or power-off is under way); lines go out raw at its steps.
    paused: AtomicBool,
    /// `flush` calls in progress (the pause waits for 0).
    busy: AtomicU32,
    ip_id: AtomicU32,
    recent: [AtomicU64; RECENT],
    recent_next: AtomicUsize,
    last_emergency_ns: AtomicU64,
    kicked: AtomicBool,
    wq: WaitQueue,
    /// RB-16: positions of the breadcrumb stream that follows the end datagram (`crumbs`).
    crumb_pos: AtomicU64,
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
    /// The card said it had sent the frames (else `DONE_NS` ran out or it could not say).
    pub done: bool,
    /// The card could not say (no probe, not running): the mirror settled by time.
    pub done_unknown: bool,
}

impl StopReport {
    fn pack(&self) -> u64 {
        let flags = [self.was_live, self.ended, self.dropped, self.no_route, self.timed_out, self.done, self.done_unknown];
        let bits = flags.iter().enumerate().fold(0u64, |a, (i, &f)| a | (f as u64) << i);
        (self.frames as u64) << 32 | bits
    }
    fn unpack(v: u64) -> StopReport {
        let f = |i: u32| v >> i & 1 == 1;
        StopReport {
            was_live: f(0),
            ended: f(1),
            dropped: f(2),
            no_route: f(3),
            timed_out: f(4),
            done: f(5),
            done_unknown: f(6),
            frames: (v >> 32) as u32,
        }
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
            paused: AtomicBool::new(false),
            busy: AtomicU32::new(0),
            ip_id: AtomicU32::new(0),
            recent: [const { AtomicU64::new(0) }; RECENT],
            recent_next: AtomicUsize::new(0),
            last_emergency_ns: AtomicU64::new(0),
            kicked: AtomicBool::new(false),
            wq: WaitQueue::new(),
            crumb_pos: AtomicU64::new(0),
        })
    }

    pub fn target(&self) -> Target {
        self.target
    }
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
    /// RB-15: a reboot or power-off paused the normal path.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
    /// The thread sends through the stack (live and not paused).
    pub fn sends_normally(&self) -> bool {
        self.is_live() && !self.is_paused()
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
        self.paused.store(false, Ordering::SeqCst);
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
    /// A pause waits for it to return; it checks for one before every datagram.
    pub fn flush(&self, sock: &dyn UdpSocket, budget: usize) -> Result<(usize, u64), NetError> {
        struct Busy<'a>(&'a AtomicU32);
        impl Drop for Busy<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        // SeqCst with the pause's store and load: either this sees the pause, or the pause sees it busy.
        self.busy.fetch_add(1, Ordering::SeqCst);
        let _busy = Busy(&self.busy);
        let mut buf = [0u8; DGRAM_MAX];
        let (mut sent, mut lost) = (0, 0);
        while sent < budget && self.sends_normally() {
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

    /// Everything waiting from the ring's `sent` (with `tail`: the last `RESEND_TAIL` again) out raw by
    /// `deadline`; a busy ring lock or a refusing card is retried after `wait(RAW_RETRY_NS)`. Returns the end the
    /// ring reached (everything before it is out), None if the deadline came first.
    fn raw_rest(&self, r: &RouteInfo, mut tail: bool, deadline: u64, wait: &dyn Fn(u64), rep: &mut EmergencyReport) -> Option<u64> {
        loop {
            let from = self.st.try_lock().map(|mut g| {
                LOST_BYTES.add(g.ring.skip_overwritten());
                if tail { g.ring.sent().saturating_sub(RESEND_TAIL) } else { g.ring.sent() }
            });
            if let Some(from) = from
                && let Some(end) = self.send_raw(r, from, rep)
            {
                return Some(end);
            }
            // Only the first try repeats the tail; send_raw marks what went out, so the next try goes on.
            tail = from.is_none() && tail;
            if (self.clock)() >= deadline {
                return None;
            }
            wait(RAW_RETRY_NS);
        }
    }

    /// Asks the route's card until it has sent everything (`DONE_NS` at most), then the frame's way to the wire.
    /// Returns (done, unknown); a card that cannot say gets `SETTLE_NS` instead.
    fn wait_done(&self, r: &RouteInfo, wait: &dyn Fn(u64)) -> (bool, bool) {
        let deadline = (self.clock)().saturating_add(DONE_NS);
        let done = loop {
            match r.done.as_ref().and_then(|p| p.tx_done()) {
                Some(true) => break Some(true),
                None => break None,
                Some(false) if (self.clock)() >= deadline => break Some(false),
                Some(false) => wait(RAW_RETRY_NS),
            }
        };
        match done {
            Some(true) => {
                wait(DONE_SETTLE_NS);
                (true, false)
            }
            Some(false) => {
                DONE_TIMEOUTS.inc();
                wait(SETTLE_NS);
                (false, false)
            }
            None => {
                DONE_UNKNOWN.inc();
                wait(SETTLE_NS);
                (false, true)
            }
        }
    }

    /// RB-15 step 1 (thread context, `sleep` may sleep): the normal path off, the thread out of its send, the
    /// stack's moment for what the thread queued, then what waits out raw (the stack's tail once more: what it
    /// still held when its card stops is not lost; the listener drops what it has), and the card's done.
    pub fn pause(&self, sleep: &dyn Fn(u64)) -> PauseReport {
        let mut out = PauseReport::default();
        // Set even before the mirror is live: a thread that goes live during the shutdown then idles too. A
        // shutdown that does not end in a reset (a failed power-off) takes it back with `unpause`.
        if self.paused.swap(true, Ordering::SeqCst) || !self.is_live() {
            return out;
        }
        out.was_live = true;
        PAUSES.inc();
        let t0 = (self.clock)();
        while self.busy.load(Ordering::SeqCst) > 0 {
            if (self.clock)().saturating_sub(t0) >= PAUSE_WAIT_NS {
                out.thread_busy = true;
                PAUSE_THREAD_BUSY.inc();
                break;
            }
            sleep(1_000_000);
        }
        sleep(STACK_SETTLE_NS);
        self.raw_round(true, sleep, &mut out);
        out
    }

    /// After a shutdown step (thread context): what was logged since the last round out raw, the card's done.
    /// Nothing when the mirror is not paused (or stopped).
    pub fn checkpoint(&self, sleep: &dyn Fn(u64)) -> PauseReport {
        let mut out = PauseReport::default();
        if !self.is_live() || !self.is_paused() {
            return out;
        }
        out.was_live = true;
        self.raw_round(false, sleep, &mut out);
        out
    }

    fn raw_round(&self, tail: bool, sleep: &dyn Fn(u64), out: &mut PauseReport) {
        let Some(r) = self.route() else {
            out.done_unknown = true;
            return;
        };
        let mut rep = EmergencyReport::default();
        let deadline = (self.clock)().saturating_add(STOP_NS);
        let _ = self.raw_rest(&r, tail, deadline, sleep, &mut rep);
        out.frames = rep.frames;
        out.dropped = rep.dropped;
        // At the pause the card is asked even when nothing went out raw: the stack and the card's NAPI path
        // may have queued datagrams just before it (RB-15 fixer). A later checkpoint that sent nothing has
        // nothing new in the card (the pause or the round before asked already).
        if rep.frames > 0 || tail {
            (out.done, out.done_unknown) = self.wait_done(&r, sleep);
        } else {
            out.done = true;
        }
    }

    /// The normal path back (a power-off that did not reach the quiesce; tests): the thread sends again.
    pub fn unpause(&self) {
        if self.paused.swap(false, Ordering::SeqCst) {
            self.kick();
        }
    }

    /// Quiesce: what waits and the end datagram out through `poll_tx` within `STOP_NS`, then the card asked until
    /// it has sent them (RB-15: the reset must not meet them in its DMA), then off (V1 `mirror_stop`). Interrupts
    /// are off and the stack's thread does not run any more. The end datagram names the end the raw path
    /// reached, so a line appended meanwhile is not shown as lost (after a resume it goes out under the new
    /// start; on a real Reload it is gone either way).
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
        // A pause repeated the stack's tail already; without one (Reload, a panic's quiesce) the stop does.
        let tail = !self.paused.swap(false, Ordering::SeqCst);
        out.was_live = true;
        self.stopped.store(true, Ordering::Release);
        let Some(r) = self.route() else {
            out.no_route = true;
            return out;
        };
        let clock = self.clock;
        let spin = |ns: u64| drop_wait(clock, ns);
        let deadline = clock().saturating_add(STOP_NS);
        let mut rep = EmergencyReport::default();
        let mut tail = tail;
        while let Some(log_end) = self.raw_rest(&r, tail, deadline, &spin, &mut rep) {
            tail = false;
            let mut f = [0u8; FRAME_MAX];
            if let Some(n) = wire::end(&mut f[FRAME_HEAD..], self.id(), self.scr_pos.load(Ordering::Acquire), log_end)
                && self.send_frame(&r, &mut f, n)
            {
                out.ended = true;
                break;
            }
            // The card's ring full: it frees descriptors as it sends.
            rep.dropped = true;
            if clock() >= deadline {
                break;
            }
            spin(RAW_RETRY_NS);
        }
        out.timed_out = !out.ended;
        out.frames = rep.frames + out.ended as u32;
        out.dropped = rep.dropped;
        if out.frames > 0 {
            (out.done, out.done_unknown) = self.wait_done(&r, &spin);
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

impl MirrorHook for Mirror {
    fn pause(&self, sleep: &dyn Fn(u64)) -> PauseReport {
        Mirror::pause(self, sleep)
    }
    fn checkpoint(&self, sleep: &dyn Fn(u64)) -> PauseReport {
        Mirror::checkpoint(self, sleep)
    }
    fn unpause(&self) {
        Mirror::unpause(self)
    }
}

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
