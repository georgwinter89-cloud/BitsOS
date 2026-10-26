//! `/dev/input/eventN` for programs (IN-4): Linux evdev (linux-6.6/drivers/input/evdev.c) on the input core of
//! `evdev`. One node per input device of the kernel's sink, made when the device registers (or as soon as
//! devtmpfs exists) and removed when it goes; MCP's injected keyboard and pointer are devices too
//! (`register_mcp_devices`), so a program reading evdev (bitscomp, A5) sees what Claude types.
//!
//! Per open a client (`Handle`, Linux `evdev_client`) with its own ring of `bufsize` events
//! (`evdev_compute_buffer_size` :461), packets made visible at SYN_REPORT, SYN_DROPPED when the ring overflows
//! (`__pass_event` :214), the clock of EVIOCSCLOCKID (default CLOCK_REALTIME, :174), type/code masks (:955-1027),
//! EVIOCGRAB (only the grabbing client gets the device's events, the VT does not; :341), EVIOCREVOKE. Reads give
//! whole 24-byte `struct input_event`s (x86-64: timeval of two longs, type, code, value). The ioctls follow
//! `evdev_do_ioctl` (:1029) including its return values (EVIOCGNAME and the bit queries answer the bytes copied)
//! and errno choices (unknown commands EINVAL, a removed device or revoked client ENODEV).
//!
//! Differences on purpose: EVIOCGKEYCODE/EVIOCSKEYCODE answer EINVAL (the input devices carry no scancode
//! table; Linux's HID devices do), EVIOCSFF/EVIOCRMFF ENOSYS (no force feedback before IN-5), EVIOCGMTSLOTS
//! EINVAL (no multitouch devices), EVIOCSABS changes what EVIOCGABS answers but not the core's fuzz filter.
//! Nodes are owned by uid 1000 (the seat user, as systemd-logind's ACL gives it on Linux), mode 0660, 13:(64+N).
//! Keys of devices with EV_REP repeat to the clients as Linux's soft repeat does (`evrepeat`, value 2), with the
//! device's EVIOCGREP settings (the console's 500/33 ms by default, Linux's input core has 250/33).

use crate::evdev::{self, AbsInfo, Caps, DeviceInfo, EV_ABS, EV_KEY, EV_LED, EV_MSC, EV_REL, EV_SYN, InputDevice, InputEvent, Listener, SYN_REPORT};
use crate::jobs;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Mutex, SpinLock, WaitQueue};
use uapi::errno::*;
use vfs::sys::UserMem;
use vfs::{Errno, Events, File};

pub const EV_REP: u16 = 0x14;
pub const EV_FF: u16 = 0x15;
pub const EV_SND: u16 = 0x12;
pub const EV_SW: u16 = 0x05;
pub const SYN_DROPPED: u16 = 3;
/// `EV_VERSION`.
pub const VERSION: i32 = 0x010001;
/// `sizeof(struct input_event)` on x86-64.
pub const EVENT_SIZE: usize = 24;
const ABS_MT_SLOT: u16 = 0x2f;

/// The `*_MAX` and `*_CNT` of input-event-codes.h the bit queries use.
const EV_MAX: u32 = 0x1f;
const KEY_MAX: u32 = 0x2ff;
const REL_MAX: u32 = 0x0f;
const ABS_MAX: u32 = 0x3f;
const MSC_MAX: u32 = 0x07;
const LED_MAX: u32 = 0x0f;
const SND_MAX: u32 = 0x07;
const FF_MAX: u32 = 0x7f;
const SW_MAX: u32 = 0x10;
const INPUT_PROP_MAX: u32 = 0x1f;

/// ioctl numbers (`_IOC(dir, 'E', nr, size)`; include/uapi/linux/input.h).
pub mod ioc {
    pub const fn ior(nr: u64, size: u64) -> u64 {
        (2 << 30) | (size << 16) | (0x45 << 8) | nr
    }
    pub const fn iow(nr: u64, size: u64) -> u64 {
        (1 << 30) | (size << 16) | (0x45 << 8) | nr
    }
    pub const EVIOCGVERSION: u64 = ior(0x01, 4);
    pub const EVIOCGID: u64 = ior(0x02, 8);
    pub const EVIOCGREP: u64 = ior(0x03, 8);
    pub const EVIOCSREP: u64 = iow(0x03, 8);
    pub const EVIOCGKEYCODE: u64 = ior(0x04, 8);
    pub const EVIOCGKEYCODE_V2: u64 = ior(0x04, 40);
    pub const EVIOCSKEYCODE: u64 = iow(0x04, 8);
    pub const EVIOCSKEYCODE_V2: u64 = iow(0x04, 40);
    pub const EVIOCRMFF: u64 = iow(0x81, 4);
    pub const EVIOCGEFFECTS: u64 = ior(0x84, 4);
    pub const EVIOCGRAB: u64 = iow(0x90, 4);
    pub const EVIOCREVOKE: u64 = iow(0x91, 4);
    pub const EVIOCGMASK: u64 = ior(0x92, 16);
    pub const EVIOCSMASK: u64 = iow(0x93, 16);
    pub const EVIOCSCLOCKID: u64 = iow(0xa0, 4);
    pub const EVIOCSFF_NR: u64 = 0x80;
    /// Variable-length reads: `EVIOCGNAME(len)` = `ior(NAME, len)`.
    pub const NAME: u64 = 0x06;
    pub const PHYS: u64 = 0x07;
    pub const UNIQ: u64 = 0x08;
    pub const PROP: u64 = 0x09;
    pub const MTSLOTS: u64 = 0x0a;
    pub const KEY: u64 = 0x18;
    pub const LED: u64 = 0x19;
    pub const SND: u64 = 0x1a;
    pub const SW: u64 = 0x1b;
    /// `EVIOCGBIT(ev, len)` = `ior(BIT + ev, len)`; `EVIOCGABS(a)` = `ior(ABS + a, 24)`; `EVIOCSABS(a)` = `iow(SABS + a, 24)`.
    pub const BIT: u64 = 0x20;
    pub const ABS: u64 = 0x40;
    pub const SABS: u64 = 0xc0;
    pub const FF_EFFECT_SIZE: u64 = 48;
}

const CLOCK_REALTIME: u32 = 0;
const CLOCK_MONOTONIC: u32 = 1;
const CLOCK_BOOTTIME: u32 = 7;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Clock {
    Real,
    Mono,
    Boot,
}

// ---- the client ring (evdev_client)

/// One client's ring, exactly as evdev.c keeps it: `head`, `tail`, `packet_head`, a power-of-two size.
pub struct Ring {
    buf: Vec<InputEvent>,
    head: usize,
    tail: usize,
    packet_head: usize,
    pub clock: Clock,
    /// EVIOCSMASK: per type (index 0 = the type mask) the codes that pass; None = all.
    masks: Vec<Option<Vec<u64>>>,
    pub revoked: bool,
    /// The newest event time in this ring (IN-6: never goes back).
    last_ns: u64,
    /// Clamps not counted yet (IN-6b): how many and the largest step back; `take_clamps` after the lock.
    clamps: u64,
    clamp_behind: u64,
    /// Overflows so far and the events they threw away (SYN_DROPPED; counted by `Queue` after the lock).
    overflows: u64,
    lost: u64,
}

/// `evdev_get_mask_cnt`.
fn mask_cnt(t: u32) -> usize {
    match t {
        0 => 0x20,
        1 => 0x300,
        2 => 0x10,
        3 => 0x40,
        4 => 0x08,
        5 => 0x11,
        0x11 => 0x10,
        0x12 => 0x08,
        0x15 => 0x80,
        _ => 0,
    }
}

fn bit(words: &[u64], n: usize) -> bool {
    words.get(n / 64).is_some_and(|w| w >> (n % 64) & 1 != 0)
}

impl Ring {
    pub fn new(size: usize) -> Ring {
        let size = size.max(2).next_power_of_two();
        let zero = InputEvent { time_ns: 0, kind: 0, code: 0, value: 0, device: 0, injected: false };
        Ring { buf: vec![zero; size], head: 0, tail: 0, packet_head: 0, clock: Clock::Real, masks: vec![None; 0x20], revoked: false, last_ns: 0, clamps: 0, clamp_behind: 0, overflows: 0, lost: 0 }
    }
    fn mask(&self) -> usize {
        self.buf.len() - 1
    }
    /// `__evdev_is_filtered`.
    fn filtered(&self, t: u16, code: u16) -> bool {
        if t == EV_SYN || t as usize >= 0x20 {
            return false;
        }
        if let Some(m) = &self.masks[0]
            && !bit(m, t as usize)
        {
            return true;
        }
        let cnt = mask_cnt(t as u32);
        if cnt == 0 || code as usize >= cnt {
            return false;
        }
        self.masks[t as usize].as_ref().is_some_and(|m| !bit(m, code as usize))
    }
    /// `evdev_pass_values` for one event; true when a packet became readable (wake readers).
    pub fn pass(&mut self, ev: InputEvent) -> bool {
        if self.revoked || self.filtered(ev.kind, ev.code) {
            return false;
        }
        let report = ev.kind == EV_SYN && ev.code == SYN_REPORT;
        if report && self.packet_head == self.head {
            return false; // empty SYN_REPORT
        }
        self.pass_one(ev);
        report
    }
    /// `__pass_event`.
    fn pass_one(&mut self, ev: InputEvent) {
        if ev.time_ns < self.last_ns {
            self.clamps += 1;
            self.clamp_behind = self.clamp_behind.max(self.last_ns - ev.time_ns);
        }
        let ev = InputEvent { time_ns: ev.time_ns.max(self.last_ns), ..ev };
        self.last_ns = ev.time_ns;
        let m = self.mask();
        self.buf[self.head] = ev;
        self.head = (self.head + 1) & m;
        if self.head == self.tail {
            // Everything unread goes: SYN_DROPPED plus the newest event stay.
            self.overflows += 1;
            self.lost += m as u64;
            self.tail = self.head.wrapping_sub(2) & m;
            self.buf[self.tail] = InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: 0, ..ev };
            self.packet_head = self.tail;
        }
        if ev.kind == EV_SYN && ev.code == SYN_REPORT {
            self.packet_head = self.head;
        }
    }
    /// The clamps since the last call (count, largest step back), to be counted outside the ring's lock.
    pub fn take_clamps(&mut self) -> (u64, u64) {
        let r = (self.clamps, self.clamp_behind);
        (self.clamps, self.clamp_behind) = (0, 0);
        r
    }
    /// Overflows and the events they lost since the ring was made.
    pub fn losses(&self) -> (u64, u64) {
        (self.overflows, self.lost)
    }
    /// Events that can still go in before the ring overflows (`__pass_event` drops at head == tail).
    pub fn room(&self) -> usize {
        self.mask() - (self.head.wrapping_sub(self.tail) & self.mask())
    }
    /// `__evdev_queue_syn_dropped`.
    fn queue_syn_dropped(&mut self, time_ns: u64) {
        let m = self.mask();
        self.buf[self.head] = InputEvent { time_ns, kind: EV_SYN, code: SYN_DROPPED, value: 0, device: 0, injected: false };
        self.head = (self.head + 1) & m;
        if self.head == self.tail {
            self.tail = self.head.wrapping_sub(1) & m;
            self.packet_head = self.tail;
        }
    }
    /// `__evdev_flush_queue`: drops the queued events of `t`, and SYN_REPORT groups left empty.
    pub fn flush_type(&mut self, t: u16) {
        let m = self.mask();
        let mut head = self.tail;
        self.packet_head = self.tail;
        let mut num = 1;
        let mut i = self.tail;
        while i != self.head {
            let ev = self.buf[i];
            let report = ev.kind == EV_SYN && ev.code == SYN_REPORT;
            i = (i + 1) & m;
            if ev.kind == t || (report && num == 0) {
                continue;
            }
            self.buf[head] = ev;
            num += 1;
            head = (head + 1) & m;
            if report {
                num = 0;
                self.packet_head = head;
            }
        }
        self.head = head;
    }
    pub fn readable(&self) -> bool {
        self.packet_head != self.tail
    }
    /// `evdev_fetch_next_event`.
    pub fn fetch(&mut self) -> Option<InputEvent> {
        if !self.readable() {
            return None;
        }
        let ev = self.buf[self.tail];
        self.tail = (self.tail + 1) & self.mask();
        Some(ev)
    }
    fn empty(&self) -> bool {
        self.head == self.tail
    }
    /// `evdev_set_clk_type`: a new clock flushes what is queued and leaves one SYN_DROPPED.
    pub fn set_clock(&mut self, c: Clock, now_ns: u64) {
        if self.clock != c {
            self.clock = c;
            if !self.empty() {
                self.head = self.tail;
                self.packet_head = self.tail;
                self.queue_syn_dropped(now_ns);
            }
        }
    }
}

/// `input_estimate_events_per_packet` (input.c) and `evdev_compute_buffer_size`.
pub fn buffer_size(caps: &Caps) -> usize {
    let mt_slots = if caps.abs.contains_key(&0x35) { 2 } else { 0 };
    let mut events = mt_slots + 1;
    events += caps.abs.keys().map(|&a| if (0x2f..=0x3d).contains(&a) { mt_slots } else { 1 }).sum::<usize>();
    events += caps.rel.len();
    events += 7;
    (events * 8).max(64).next_power_of_two()
}

/// Bytes of an event as a program reads it, in the client's clock.
pub fn event_bytes(ev: &InputEvent, clock: Clock, realtime_offset: i128) -> [u8; EVENT_SIZE] {
    let ns: i128 = match clock {
        Clock::Real => ev.time_ns as i128 + realtime_offset,
        Clock::Mono | Clock::Boot => ev.time_ns as i128,
    };
    let ns = ns.max(0) as u64;
    let mut b = [0u8; EVENT_SIZE];
    b[0..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_le_bytes());
    b[8..16].copy_from_slice(&(((ns % 1_000_000_000) / 1000) as i64).to_le_bytes());
    b[16..18].copy_from_slice(&ev.kind.to_le_bytes());
    b[18..20].copy_from_slice(&ev.code.to_le_bytes());
    b[20..24].copy_from_slice(&ev.value.to_le_bytes());
    b
}

/// CLOCK_REALTIME minus the monotonic clock now (ns).
fn realtime_offset() -> i128 {
    let mono = time::now_ns();
    time::wall::realtime_ns().map(|r| r as i128 - mono as i128).unwrap_or(0)
}

// ---- bits

/// `bits_to_user`'s length for `maxbit`: BITS_TO_LONGS(maxbit) longs.
pub fn bits_len(maxbit: u32) -> usize {
    (maxbit as usize).div_ceil(64) * 8
}

/// A Linux bitmap of `maxbit` bits with `set` on, as the bytes `bits_to_user` copies (at most `size`).
pub fn bitmap(set: impl IntoIterator<Item = u32>, maxbit: u32, size: usize) -> Vec<u8> {
    let mut b = vec![0u8; bits_len(maxbit)];
    for n in set {
        // Codes up to the bitmap's end (Linux sizes the bitmaps *_CNT bits; the copy is BITS_TO_LONGS(*_MAX)).
        if let Some(x) = b.get_mut(n as usize / 8) {
            *x |= 1 << (n % 8);
        }
    }
    b.truncate(size);
    b
}

/// `evbit` as the input core sets it: EV_SYN always, then the kinds the device has.
pub fn evbits(caps: &Caps) -> Vec<u32> {
    let mut v = vec![EV_SYN as u32];
    if !caps.keys.is_empty() {
        v.push(EV_KEY as u32);
    }
    if !caps.rel.is_empty() {
        v.push(EV_REL as u32);
    }
    if !caps.abs.is_empty() {
        v.push(EV_ABS as u32);
    }
    if !caps.msc.is_empty() {
        v.push(EV_MSC as u32);
    }
    if !caps.leds.is_empty() {
        v.push(EV_LED as u32);
    }
    if caps.repeat {
        v.push(EV_REP as u32);
    }
    v
}

// ---- open clients

/// The ring and its wait queue, as the sink sees it (a `Listener` filtered to one device), and the soft
/// autorepeat this client sees (`input_start_autorepeat`/`input_repeat_key`, input.c:86 and :2250).
pub struct Queue {
    event_id: u32,
    ring: SpinLock<Ring>,
    /// Readers and poll/epoll (FS-4 CR4: a `PollQueue`, so epoll wakes at once).
    wq: vfs::poll::PollQueue,
    dev: Arc<InputDevice>,
    /// The sink's id of this listener (the grab check of a repeat).
    id: AtomicU64,
    rep: SpinLock<Rep>,
}

/// The key that repeats and when next (None: nothing repeats).
#[derive(Default, Clone, Copy)]
struct Rep {
    key: Option<(u16, bool)>,
    next_ns: u64,
}

const MS: u64 = 1_000_000;

/// Counts a reader ring's clamps (IN-6b, `input.time_clamped.reader`).
fn note_clamps((n, behind): (u64, u64)) {
    for _ in 0..n {
        evdev::note_clamp(evdev::Source::Reader, behind);
    }
}

klog::counter!(OVERFLOWS, "input.evdev_overflows");
klog::counter!(LOST, "input.evdev_lost");

impl Listener for Queue {
    fn push(&self, ev: InputEvent) {
        let (wake, clamps, before, after) = {
            let mut ring = self.ring.lock();
            let before = ring.losses();
            let wake = ring.pass(ev);
            (wake, ring.take_clamps(), before, ring.losses())
        };
        note_clamps(clamps);
        if after.0 > before.0 {
            // SYN_DROPPED: the reader did not read in time (Linux says nothing; here it is counted and told).
            OVERFLOWS.inc();
            LOST.add(after.1 - before.1);
            let n = OVERFLOWS.get();
            klog::event!(Debug, "input", "evdev.overflow", client = self.id.load(Ordering::Relaxed), device = self.event_id,
                lost = after.1 - before.1, overflows = n);
            if n.is_power_of_two() {
                klog::event!(Warn, "input", "evdev.overflow", client = self.id.load(Ordering::Relaxed), device = self.event_id,
                    lost = after.1 - before.1, overflows = n; "a reader of /dev/input did not read in time: its ring overflowed (SYN_DROPPED)");
            }
        }
        if wake {
            self.wq.wake_all();
        }
        // input_pass_values: a press (re)starts the repeat, any release stops it.
        if ev.kind == EV_KEY && ev.value != 2 && self.dev.info.caps.repeat {
            let (delay, period) = self.dev.repeat();
            let start = ev.value != 0 && delay > 0 && period > 0;
            *self.rep.lock() = Rep { key: start.then_some((ev.code, ev.injected)), next_ns: ev.time_ns.saturating_add(delay as u64 * MS) };
            if start {
                crate::evrepeat::kick();
            }
        }
    }
    fn device(&self) -> Option<u32> {
        Some(self.event_id)
    }
    fn room(&self) -> usize {
        let r = self.ring.lock();
        if r.revoked { usize::MAX } else { r.room() }
    }
    fn lost(&self) -> u64 {
        self.ring.lock().losses().1
    }
}

impl Queue {
    /// `input_repeat_key`: a repeat that is due goes into the ring (EV_KEY value 2, SYN_REPORT value 1) while the
    /// device still holds the key and no other client grabbed it. Returns when the next one is due.
    pub fn repeat_due(&self, now: u64) -> Option<u64> {
        let r = *self.rep.lock();
        let (key, injected) = r.key?;
        if now < r.next_ns {
            return Some(r.next_ns);
        }
        // The device's key state is read without the repeat lock (pushers take the device's lock first).
        let held = !self.dev.is_gone() && self.dev.held_keys().contains(&key);
        let period = self.dev.repeat().1 as u64 * MS;
        let next = now.saturating_add(period);
        {
            let mut cur = self.rep.lock();
            if cur.key != Some((key, injected)) || cur.next_ns != r.next_ns {
                return cur.key.map(|_| cur.next_ns);
            }
            if !held || period == 0 {
                cur.key = None;
                return None;
            }
            cur.next_ns = next;
        }
        let id = self.id.load(Ordering::Acquire);
        if self.dev.sink().grabbed_by(self.event_id).is_some_and(|g| g != id) {
            return Some(next);
        }
        let ev = InputEvent { time_ns: now, kind: EV_KEY, code: key, value: 2, device: self.event_id, injected };
        let (wake, clamps) = {
            let mut ring = self.ring.lock();
            ring.pass(ev);
            (ring.pass(InputEvent { kind: EV_SYN, code: SYN_REPORT, value: 1, ..ev }), ring.take_clamps())
        };
        note_clamps(clamps);
        if wake {
            self.wq.wake_all();
        }
        Some(next)
    }
}

/// One open of `/dev/input/eventN` (Linux `evdev_client`). Dropping it (the last close) detaches it from the
/// sink and ends its grab.
pub struct Handle {
    pub dev: Arc<InputDevice>,
    q: Arc<Queue>,
    id: u64,
    subject: policy::Subject,
}

klog::counter!(OPENS, "input.evdev_opens");

impl Handle {
    pub fn open(dev: &Arc<InputDevice>) -> Result<Arc<Handle>, Errno> {
        let subject = jobs::procs().current().map(|c| c.subject).unwrap_or(policy::Subject::KERNEL);
        let name = format!("/dev/input/event{}", dev.number);
        if let Err(d) = policy::check(&subject, &policy::Access::Device { class: policy::DeviceClass::Other, name: &name }) {
            klog::event!(Warn, "input", "evdev.open_refused", node = name.as_str(), reason = d.reason);
            return Err(EACCES);
        }
        if dev.is_gone() {
            return Err(ENODEV);
        }
        let q = Arc::new(Queue {
            event_id: dev.event_id(),
            ring: SpinLock::named("evdev.handle", Ring::new(buffer_size(&dev.info.caps))),
            wq: vfs::poll::PollQueue::new(),
            dev: dev.clone(),
            id: AtomicU64::new(0),
            rep: SpinLock::named("evdev.repeat", Rep::default()),
        });
        let id = dev.sink().attach(q.clone());
        q.id.store(id, Ordering::Release);
        if dev.info.caps.repeat {
            crate::evrepeat::register(&q);
        }
        OPENS.inc();
        klog::event!(Debug, "input", "evdev.open", node = name.as_str(), pid = subject.pid, client = id);
        Ok(Arc::new(Handle { dev: dev.clone(), q, id, subject }))
    }

    fn alive(&self) -> bool {
        !self.dev.is_gone() && !self.q.ring.lock().revoked
    }

    /// `evdev_read`.
    pub fn read(&self, out: &mut [u8], nonblock: bool, abort: &dyn Fn() -> bool) -> Result<usize, Errno> {
        if !out.is_empty() && out.len() < EVENT_SIZE {
            return Err(EINVAL);
        }
        let mut n = 0;
        loop {
            if !self.alive() {
                return Err(ENODEV);
            }
            if nonblock && !self.q.ring.lock().readable() {
                return Err(EAGAIN);
            }
            if out.is_empty() {
                return Ok(0);
            }
            let off = realtime_offset();
            let mut first = 0;
            while n + EVENT_SIZE <= out.len() {
                let (ev, clock) = {
                    let mut r = self.q.ring.lock();
                    (r.fetch(), r.clock)
                };
                let Some(ev) = ev else { break };
                if first == 0 {
                    first = ev.time_ns;
                }
                out[n..n + EVENT_SIZE].copy_from_slice(&event_bytes(&ev, clock, off));
                n += EVENT_SIZE;
            }
            if n > 0 {
                crate::latency::program_read(first, time::now_ns());
                self.dev.sink().reader_took();
                return Ok(n);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if abort() {
                return Err(EINTR);
            }
            self.q.wq.wait_until_timeout(|| self.q.ring.lock().readable() || !self.alive(), 50_000_000);
        }
    }

    /// `evdev_write`: whole events go into the input core as if the device sent them (LED events set the lock
    /// LEDs every keyboard shows). They count as injected (R8c: never a physical key).
    pub fn write(&self, data: &[u8]) -> Result<usize, Errno> {
        if !data.is_empty() && data.len() < EVENT_SIZE {
            return Err(EINVAL);
        }
        if !self.alive() {
            return Err(ENODEV);
        }
        let sink = self.dev.sink();
        let mut done = 0;
        while done + EVENT_SIZE <= data.len() {
            let e = &data[done..done + EVENT_SIZE];
            let kind = u16::from_le_bytes([e[16], e[17]]);
            let code = u16::from_le_bytes([e[18], e[19]]);
            let value = i32::from_le_bytes([e[20], e[21], e[22], e[23]]);
            done += EVENT_SIZE;
            if kind == EV_LED && code <= evdev::LED_SCROLLL {
                let bit = 1u32 << code;
                let locks = sink.locks();
                sink.set_locks(if value != 0 { locks | bit } else { locks & !bit });
            }
            if self.supports(kind, code) {
                // The sink stamps it per packet (IN-6b; the writer's time is ignored as by Linux evdev_write).
                sink.push_written(InputEvent { time_ns: 0, kind, code, value, device: self.dev.event_id(), injected: true });
            }
        }
        Ok(done)
    }

    /// `input_inject_event`'s check that the device has the event.
    fn supports(&self, kind: u16, code: u16) -> bool {
        let c = &self.dev.info.caps;
        match kind {
            EV_SYN => true,
            EV_KEY => c.keys.contains(&code),
            EV_REL => c.rel.contains(&code),
            EV_ABS => c.abs.contains_key(&code),
            EV_MSC => c.msc.contains(&code),
            EV_LED => c.leds.contains(&code),
            _ => false,
        }
    }

    pub fn events(&self) -> Events {
        let mut e = if self.alive() { Events::OUT } else { Events::HUP | Events::ERR };
        if self.q.ring.lock().readable() {
            e |= Events::IN;
        }
        Events(e)
    }

    pub fn wait_queue(&self) -> &WaitQueue {
        &self.q.wq
    }

    /// The soft repeat of this client (`evrepeat` calls it through the queue; tests directly).
    pub fn repeat_due(&self, now: u64) -> Option<u64> {
        self.q.repeat_due(now)
    }

    /// Whether this client holds the device's grab.
    pub fn grabbing(&self) -> bool {
        self.dev.sink().grabbed_by(self.dev.event_id()) == Some(self.id)
    }

    /// `evdev_ioctl_handler` + `evdev_do_ioctl`: the return value (bytes copied for the variable-length reads,
    /// else 0) or an errno.
    pub fn ioctl(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        use ioc::*;
        if !self.alive() {
            return Err(ENODEV);
        }
        let dev = &self.dev;
        let caps = &dev.info.caps;
        let put = |data: &[u8]| if mem.write(arg, data) { Ok(0) } else { Err(EFAULT) };
        let get = |buf: &mut [u8]| if mem.read(arg, buf) { Ok(()) } else { Err(EFAULT) };
        match cmd {
            EVIOCGVERSION => return put(&VERSION.to_le_bytes()),
            EVIOCGID => {
                let i = &dev.info;
                let mut b = [0u8; 8];
                for (k, v) in [i.bus, i.vendor, i.product, i.version].iter().enumerate() {
                    b[k * 2..k * 2 + 2].copy_from_slice(&v.to_le_bytes());
                }
                return put(&b);
            }
            EVIOCGREP | EVIOCSREP if !caps.repeat => return Err(ENOSYS),
            EVIOCGREP => {
                let (d, p) = dev.repeat();
                let mut b = [0u8; 8];
                b[..4].copy_from_slice(&d.to_le_bytes());
                b[4..].copy_from_slice(&p.to_le_bytes());
                return put(&b);
            }
            EVIOCSREP => {
                let mut b = [0u8; 8];
                get(&mut b)?;
                dev.set_repeat(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), u32::from_le_bytes([b[4], b[5], b[6], b[7]]));
                return Ok(0);
            }
            EVIOCRMFF => return Err(ENOSYS),
            EVIOCGEFFECTS => return put(&0i32.to_le_bytes()),
            EVIOCGRAB => return self.grab(arg != 0).map(|_| 0),
            EVIOCREVOKE => {
                if arg != 0 {
                    return Err(EINVAL);
                }
                self.revoke();
                return Ok(0);
            }
            EVIOCGMASK => return self.get_mask(arg, mem),
            EVIOCSMASK => return self.set_mask(arg, mem),
            EVIOCSCLOCKID => {
                let mut b = [0u8; 4];
                get(&mut b)?;
                let c = match u32::from_le_bytes(b) {
                    CLOCK_REALTIME => Clock::Real,
                    CLOCK_MONOTONIC => Clock::Mono,
                    CLOCK_BOOTTIME => Clock::Boot,
                    _ => return Err(EINVAL),
                };
                self.q.ring.lock().set_clock(c, time::now_ns());
                return Ok(0);
            }
            EVIOCGKEYCODE | EVIOCSKEYCODE => {
                let mut b = [0u8; 4];
                get(&mut b)?;
                if cmd == EVIOCSKEYCODE && !mem.read(arg + 4, &mut b) {
                    return Err(EFAULT);
                }
                return Err(EINVAL);
            }
            EVIOCGKEYCODE_V2 | EVIOCSKEYCODE_V2 => {
                let mut b = [0u8; 40];
                get(&mut b)?;
                return Err(EINVAL);
            }
            _ => {}
        }
        let size = ((cmd >> 16) & 0x3FFF) as usize;
        let dir = cmd >> 30;
        let nr = cmd & 0xFF;
        let ty = (cmd >> 8) & 0xFF;
        let put_len = |data: Vec<u8>| -> Result<i64, Errno> {
            // A zero-length copy never faults (`copy_to_user` of 0 bytes).
            if data.is_empty() || mem.write(arg, &data) { Ok(data.len() as i64) } else { Err(EFAULT) }
        };
        if dir == 2 && ty == 0x45 {
            match nr {
                PROP => return put_len(bitmap([], INPUT_PROP_MAX, size)),
                MTSLOTS => {
                    let mut b = [0u8; 4];
                    get(&mut b)?;
                    return Err(EINVAL);
                }
                KEY => {
                    self.q.ring.lock().flush_type(EV_KEY);
                    return put_len(bitmap(dev.held_keys().into_iter().map(u32::from), KEY_MAX, size));
                }
                LED => {
                    self.q.ring.lock().flush_type(EV_LED);
                    return put_len(bitmap(dev.leds_on().into_iter().map(u32::from), LED_MAX, size));
                }
                SND => {
                    self.q.ring.lock().flush_type(EV_SND);
                    return put_len(bitmap([], SND_MAX, size));
                }
                SW => {
                    self.q.ring.lock().flush_type(EV_SW);
                    return put_len(bitmap([], SW_MAX, size));
                }
                NAME => return str_to_user(Some(&dev.info.name), size, arg, mem),
                PHYS => return str_to_user(Some(&dev.info.phys).filter(|p| !p.is_empty()), size, arg, mem),
                UNIQ => return str_to_user(dev.info.uniq.as_ref(), size, arg, mem),
                _ => {}
            }
        }
        if dir == 1 && ty == 0x45 && nr == EVIOCSFF_NR {
            // `input_ff_effect_from_user` refuses another size (the caller turns that into EFAULT), then the
            // upload finds no EV_FF.
            let mut b = vec![0u8; FF_EFFECT_SIZE as usize];
            if size as u64 != FF_EFFECT_SIZE || !mem.read(arg, &mut b) {
                return Err(EFAULT);
            }
            return Err(ENOSYS);
        }
        if ty != 0x45 {
            return Err(EINVAL);
        }
        if dir == 2 && nr & !(EV_MAX as u64) == BIT {
            let (set, max): (Vec<u32>, u32) = match (nr & EV_MAX as u64) as u16 {
                0 => (evbits(caps), EV_MAX),
                EV_KEY => (caps.keys.iter().map(|&k| k as u32).collect(), KEY_MAX),
                EV_REL => (caps.rel.iter().map(|&k| k as u32).collect(), REL_MAX),
                EV_ABS => (caps.abs.keys().map(|&k| k as u32).collect(), ABS_MAX),
                EV_MSC => (caps.msc.iter().map(|&k| k as u32).collect(), MSC_MAX),
                EV_LED => (caps.leds.iter().map(|&k| k as u32).collect(), LED_MAX),
                EV_SND => (Vec::new(), SND_MAX),
                EV_FF => (Vec::new(), FF_MAX),
                EV_SW => (Vec::new(), SW_MAX),
                _ => return Err(EINVAL),
            };
            return put_len(bitmap(set, max, size));
        }
        if dir == 2 && nr & !(ABS_MAX as u64) == ABS {
            if caps.abs.is_empty() {
                return Err(EINVAL);
            }
            let code = (nr & ABS_MAX as u64) as u16;
            let (value, a) = dev.abs(code).unwrap_or_default();
            let mut b = [0u8; 24];
            for (k, v) in [value, a.min, a.max, a.fuzz, a.flat, a.resolution].iter().enumerate() {
                b[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            return if mem.write(arg, &b[..size.min(24)]) { Ok(0) } else { Err(EFAULT) };
        }
        if dir == 1 && nr & !(ABS_MAX as u64) == SABS {
            if caps.abs.is_empty() {
                return Err(EINVAL);
            }
            let code = (nr & ABS_MAX as u64) as u16;
            let mut b = [0u8; 24];
            let n = size.min(24);
            if !mem.read(arg, &mut b[..n]) {
                return Err(EFAULT);
            }
            if code == ABS_MT_SLOT {
                return Err(EINVAL);
            }
            let w = |k: usize| i32::from_le_bytes([b[k * 4], b[k * 4 + 1], b[k * 4 + 2], b[k * 4 + 3]]);
            let a = AbsInfo { min: w(1), max: w(2), fuzz: w(3), flat: w(4), resolution: if n < 24 { 0 } else { w(5) } };
            dev.set_abs(code, w(0), a);
            return Ok(0);
        }
        Err(EINVAL)
    }

    /// EVIOCGRAB: B3 asks first (input grab is a device class of its own).
    fn grab(&self, on: bool) -> Result<(), Errno> {
        let sink = self.dev.sink();
        let node = format!("/dev/input/event{}", self.dev.number);
        if !on {
            sink.ungrab(self.dev.event_id(), self.id)?;
            klog::event!(Info, "input", "evdev.ungrab", node = node.as_str(), pid = self.subject.pid);
            return Ok(());
        }
        if let Err(d) = policy::check(&self.subject, &policy::Access::Device { class: policy::DeviceClass::InputGrab, name: &node }) {
            klog::event!(Warn, "input", "evdev.grab_refused", node = node.as_str(), reason = d.reason);
            return Err(EACCES);
        }
        sink.grab(self.dev.event_id(), self.id)?;
        klog::event!(Info, "input", "evdev.grab", node = node.as_str(), pid = self.subject.pid, name = self.dev.info.name.as_str();
            "only this program gets the device's events now (the console does not)");
        Ok(())
    }

    /// `evdev_revoke`.
    fn revoke(&self) {
        self.q.ring.lock().revoked = true;
        let _ = self.dev.sink().ungrab(self.dev.event_id(), self.id);
        self.q.wq.wake_all();
    }

    /// `evdev_get_mask` (evdev.c:991): up to the mask's own size from the mask, the rest of `codes_size` zeros
    /// (`clear_user`), written in pieces so a huge `codes_size` from a program never becomes one allocation.
    fn get_mask(&self, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        let mut b = [0u8; 16];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let ty = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let codes_size = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as u64;
        let ptr = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
        let cnt = mask_cnt(ty);
        let size = (cnt.div_ceil(64) * 8) as u64;
        let xfer = codes_size.min(size) as usize;
        if cnt > 0 && xfer > 0 {
            let mut out = vec![0u8; xfer];
            match self.q.ring.lock().masks.get(ty as usize).and_then(|m| m.clone()) {
                Some(m) => {
                    let words: Vec<u8> = m.iter().flat_map(|w| w.to_le_bytes()).collect();
                    let len = bits_len(cnt as u32 - 1).min(xfer).min(words.len());
                    out[..len].copy_from_slice(&words[..len]);
                }
                None => out.fill(0xFF),
            }
            if !mem.write(ptr, &out) {
                return Err(EFAULT);
            }
        }
        let zeros = [0u8; 1024];
        let mut done = xfer as u64;
        while done < codes_size {
            let n = (codes_size - done).min(zeros.len() as u64);
            let Some(at) = ptr.checked_add(done) else { return Err(EFAULT) };
            if !mem.write(at, &zeros[..n as usize]) {
                return Err(EFAULT);
            }
            done += n;
        }
        Ok(0)
    }

    /// `evdev_set_mask`.
    fn set_mask(&self, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        let mut b = [0u8; 16];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let ty = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let codes_size = u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let ptr = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
        let cnt = mask_cnt(ty);
        if cnt == 0 {
            return Ok(0);
        }
        if !codes_size.is_multiple_of(8) {
            return Err(EINVAL);
        }
        let len = bits_len(cnt as u32 - 1).min(codes_size);
        let mut bytes = vec![0u8; len];
        if !mem.read(ptr, &mut bytes) {
            return Err(EFAULT);
        }
        let mut words = vec![0u64; cnt.div_ceil(64)];
        for (i, &x) in bytes.iter().enumerate() {
            words[i / 8] |= (x as u64) << ((i % 8) * 8);
        }
        self.q.ring.lock().masks[ty as usize] = Some(words);
        Ok(0)
    }
}

/// `str_to_user`: ENOENT without a string, else up to `size` bytes of it with its NUL; returns the count.
fn str_to_user(s: Option<&String>, size: usize, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
    let Some(s) = s else { return Err(ENOENT) };
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b.truncate(size);
    if b.is_empty() || mem.write(arg, &b) { Ok(b.len() as i64) } else { Err(EFAULT) }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.grabbing() {
            klog::event!(Info, "input", "evdev.ungrab", node = format!("/dev/input/event{}", self.dev.number).as_str(), pid = self.subject.pid; "closed while grabbing");
        }
        self.dev.sink().detach(self.id);
    }
}

impl File for Handle {
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read(buf, false, &vfs::dentry::interrupted)
    }
    fn write_at(&self, _pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.write(buf)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.read(buf, nonblock, &vfs::dentry::interrupted)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o660
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(13, 64 + self.dev.number)
    }
    fn events(&self) -> Events {
        Handle::events(self)
    }
    fn poll_head(&self) -> Option<&vfs::poll::PollQueue> {
        Some(&self.q.wq)
    }
    /// The variable-length answers need the caller's memory: through the console's user-memory hook (the
    /// return value is lost on this path; the integration patch's `ioctl_user` keeps it).
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        let m = crate::RecordMem::for_ioc(arg, cmd);
        Handle::ioctl(self, cmd, arg, &m)?;
        Ok(m.take())
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(Handle::ioctl(self, cmd, arg, mem))
    }
}

// ---- the nodes

/// The device node (`Data::Device`): one per input device. Each open gets its own `Handle` through
/// `open_instance` (the integration patch's `File::open_instance`); without it the node serves one shared
/// handle, made at first use.
pub struct EvdevNode {
    pub dev: Arc<InputDevice>,
    shared: SpinLock<Option<Arc<Handle>>>,
}

impl EvdevNode {
    pub fn new(dev: Arc<InputDevice>) -> EvdevNode {
        EvdevNode { dev, shared: SpinLock::named("evdev.node", None) }
    }
    /// Linux `evdev_open`: a client of its own.
    pub fn open_instance(&self) -> Result<Arc<Handle>, Errno> {
        Handle::open(&self.dev)
    }
    fn shared(&self) -> Result<Arc<Handle>, Errno> {
        if let Some(h) = self.shared.lock().clone() {
            return Ok(h);
        }
        let h = Handle::open(&self.dev)?;
        Ok(self.shared.lock().get_or_insert(h).clone())
    }
}

impl File for EvdevNode {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        self.shared()?.read_at(pos, buf)
    }
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        self.shared()?.write_at(pos, buf)
    }
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> Result<usize, Errno> {
        self.shared()?.read_stream(buf, nonblock)
    }
    fn mode(&self) -> u32 {
        uapi::S_IFCHR | 0o660
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(13, 64 + self.dev.number)
    }
    fn events(&self) -> Events {
        self.shared().map(|h| h.events()).unwrap_or(Events(Events::HUP | Events::ERR))
    }
    fn ioctl(&self, cmd: u64, arg: u64) -> Result<Vec<u8>, Errno> {
        File::ioctl(&*self.shared()?, cmd, arg)
    }
    // IN-4 CR1a: each open gets its own client (FS-3's `open_node` is the per-open hook; no second hook).
    fn open_node(&self, _flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(EvdevNode::open_instance(self).map(|h| h as Arc<dyn File>))
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(self.shared().and_then(|h| h.ioctl(cmd, arg, mem)))
    }
}

/// The nodes made so far: device number -> the device (a re-registered number gets a new node).
static NODES: Mutex<BTreeMap<u32, Arc<InputDevice>>> = Mutex::named("evdev.nodes", BTreeMap::new());
static DIRTY: AtomicBool = AtomicBool::new(true);

/// The owner of the nodes: the seat user (uid 1000), group root, 0660.
pub const NODE_UID: u32 = 1000;
pub const NODE_GID: u32 = 0;

/// The device list changed: bring the nodes in line now if devtmpfs is there (else the VT's thread does it
/// once it is, `sync`).
pub fn changed() {
    DIRTY.store(true, Ordering::Release);
    sync();
}

fn devfs_ready(ns: &vfs::MountNs) -> bool {
    ns.lookup_path(b"/dev", true).is_ok_and(|p| p.dentry.sb.fstype == "devtmpfs")
}

/// Makes `/dev/input/eventN` for every device of the kernel's sink and removes the nodes of devices that went.
/// One atomic load when nothing changed (the VT's thread asks every second). Returns how many nodes exist
/// afterwards when it did the work (None: nothing to do, or no devtmpfs yet).
pub fn sync() -> Option<usize> {
    if !DIRTY.load(Ordering::Acquire) {
        return None;
    }
    let ns = vfs::root_ns();
    if !devfs_ready(&ns) {
        return None;
    }
    let mut nodes = NODES.lock();
    if !DIRTY.swap(false, Ordering::AcqRel) {
        return Some(nodes.len());
    }
    let want: Vec<Arc<InputDevice>> = evdev::devices().into_iter().filter(|d| core::ptr::eq(d.sink(), evdev::sink())).collect();
    let stale: Vec<u32> = nodes.iter().filter(|(n, d)| !want.iter().any(|w| w.number == **n && Arc::ptr_eq(w, d))).map(|(n, _)| *n).collect();
    for n in stale {
        nodes.remove(&n);
        let path = format!("/dev/input/event{n}");
        match ns.remove(&path) {
            Ok(()) | Err(ENOENT) => klog::event!(Info, "input", "evdev.node_removed", node = path.as_str()),
            Err(e) => klog::event!(Warn, "input", "evdev.node_remove_failed", node = path.as_str(), errno = e),
        }
    }
    for d in want {
        if nodes.contains_key(&d.number) {
            continue;
        }
        let path = format!("/dev/input/event{}", d.number);
        match ns.create_device(&path, Arc::new(EvdevNode::new(d.clone()))) {
            Ok(node) => {
                node.inode.meta.set_owner(NODE_UID, NODE_GID);
                nodes.insert(d.number, d.clone());
                klog::event!(Info, "input", "evdev.node", node = path.as_str(), name = d.info.name.as_str());
            }
            Err(e) => {
                DIRTY.store(true, Ordering::Release);
                klog::event!(Warn, "input", "evdev.node_failed", node = path.as_str(), errno = e);
            }
        }
    }
    Some(nodes.len())
}

/// MCP's injected keyboard and pointer as input devices (`INJECT_KEYBOARD`, `INJECT_POINTER` of MC-3's `input`
/// tool), so their events reach evdev readers; the pointer's absolute axes are frame pixels (MC-3).
pub fn register_mcp_devices() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut kb = Caps::default();
    kb.keys.extend(1u16..0x100);
    kb.leds.extend([evdev::LED_NUML, evdev::LED_CAPSL, evdev::LED_SCROLLL]);
    kb.repeat = true;
    evdev::register_virtual(
        DeviceInfo { name: String::from("BitsOS MCP keyboard"), phys: String::from("mcp/input0"), bus: evdev::BUS_VIRTUAL, vendor: 0, product: 1, version: 1, caps: kb, uniq: None },
        INJECT_KEYBOARD,
    );
    let (w, h) = display::with_frame(|f| (f.width, f.height)).unwrap_or((32768, 32768));
    let mut pt = Caps::default();
    pt.keys.extend(0x110u16..=0x114);
    pt.rel.extend([0u16, 1, 6, 8]);
    pt.abs.insert(0, AbsInfo { min: 0, max: w as i32 - 1, ..AbsInfo::default() });
    pt.abs.insert(1, AbsInfo { min: 0, max: h as i32 - 1, ..AbsInfo::default() });
    evdev::register_virtual(
        DeviceInfo { name: String::from("BitsOS MCP pointer"), phys: String::from("mcp/input1"), bus: evdev::BUS_VIRTUAL, vendor: 0, product: 2, version: 1, caps: pt, uniq: None },
        INJECT_POINTER,
    );
}

/// MC-3's injected device numbers (`mcp::tools::input`).
pub const INJECT_KEYBOARD: u32 = 0xFFFF_FF00;
pub const INJECT_POINTER: u32 = 0xFFFF_FF01;
