//! `C: EvdevSink` (owner IN): input events in Linux `input_event` terms, from HID drivers or MCP injection.
//! Events injected by MCP `send_keys`/`send_mouse` carry `injected = true` and can never trigger
//! physical-only actions (R8c/R8f: recovery code, poweroff).
//!
//! IN-2a adds what the Linux input core does between a driver and evdev (linux-6.6/drivers/input/input.c):
//! - **Input devices** (`register`): one per HID application (hid-generic), numbered from 0 with the smallest
//!   free number like evdev minors, with their capabilities (what EVIOCGBIT/EVIOCGABS will answer, IN-4).
//! - **Filtering** (`InputDevice::emit`; `input_get_disposition`, `input_handle_abs_event`, input.c:189-345 and
//!   376-385): unchanged keys, zero relative moves, absolute values inside the fuzz, codes the device does not
//!   have and empty frames never reach the ring.
//! - **Unplug** (`InputDevice::unregister`; `input_dev_release_keys`, input.c:732-750): every key the device
//!   still holds is released at once, then its number is free again.
//! - **Held-key union** (`KeyUnion`; V1 rule, spec usb.md §4.1): with several keyboards a key counts as held
//!   while any keyboard holds it. The sink keeps the union of everything pushed; a reader can keep its own to
//!   turn per-device events into union edges.
//! - **Lock keys and LEDs**: a press of Caps/Num/Scroll Lock (a union edge) toggles the sink's lock state as the
//!   VT does (drivers/tty/vt/keyboard.c `fn_caps_toggle`/`fn_num`); LED listeners (keyboard drivers) hear the
//!   new state, so every keyboard shows the same LEDs. IN-3's VT reads and sets the same state (`locks`,
//!   `set_locks`). The console repeats keys itself (IN-3); evdev readers get Linux's soft repeat (value 2) from
//!   `evrepeat` (IN-4), which never reaches the sink.
//! - **Recent events** for observers that must not take events from the reader (MCP `input` tool, tests).

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const EV_MSC: u16 = 0x04;
pub const EV_LED: u16 = 0x11;
pub const SYN_REPORT: u16 = 0;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_NUMLOCK: u16 = 69;
pub const KEY_SCROLLLOCK: u16 = 70;
pub const LED_NUML: u16 = 0;
pub const LED_CAPSL: u16 = 1;
pub const LED_SCROLLL: u16 = 2;
/// `input_id.bustype` values (include/uapi/linux/input.h).
pub const BUS_USB: u16 = 0x03;
pub const BUS_VIRTUAL: u16 = 0x06;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEvent {
    pub time_ns: u64,
    /// EV_KEY = 1, EV_REL = 2, EV_ABS = 3, EV_SYN = 0.
    pub kind: u16,
    pub code: u16,
    pub value: i32,
    /// Which input device (evdev node number).
    pub device: u32,
    pub injected: bool,
}

/// Called with the lock-LED bits (`1 << LED_*`) whenever they change. Runs in the pusher's context (a USB
/// event thread, an MCP worker): must not sleep, wait, or take a lock a pusher may hold. Two changes on two CPUs
/// can call listeners in either order: a listener that keeps state should read `EvdevSink::locks()` when it acts
/// rather than trust the order of calls (the bits it gets are the state at call time).
pub type LedListener = Arc<dyn Fn(u32) + Send + Sync>;

/// Ring of pending events. Bounded so a reader that stopped reading cannot eat memory (Linux evdev's client
/// buffer, evdev.c:224-248); overflow drops the oldest, counts it, logs it (first drop and each doubling) and
/// raises the resync flag: like SYN_DROPPED, the reader should then rebuild its pressed keys from `held_keys` /
/// `key_held_physical` (`take_resync`), since a dropped key-up would otherwise leave a key held for it.
pub struct EvdevSink {
    q: SpinLock<Pending>,
    wq: WaitQueue,
    dropped: AtomicU64,
    resync: AtomicBool,
    /// Sequence number the next event gets (counts every event ever pushed).
    seq: AtomicU64,
    recent: SpinLock<VecDeque<(u64, InputEvent)>>,
    held: SpinLock<KeyUnion>,
    locks: AtomicU32,
    listeners: SpinLock<Vec<(u64, LedListener)>>,
    next_listener: AtomicU64,
    /// Readers with a queue of their own (the VT; IN-4's evdev file handles), as Linux evdev's clients, by id.
    clients: SpinLock<Vec<(u64, Arc<dyn Listener>)>>,
    next_client: AtomicU64,
    /// EVIOCGRAB: event id of a device -> the listener that alone gets its events (IN-4).
    grabs: SpinLock<BTreeMap<u32, Grab>>,
    /// Someone reads the sink's own ring (`pop`): only then is an overflow worth a warning.
    popped: AtomicBool,
    /// The newest event time queued (IN-6: the sink's ring never goes back in time).
    last_ns: AtomicU64,
    /// Clamps on this sink (IN-6b). The machine's sink also counts them per source in `input.time_clamped.*`;
    /// a private sink (ktests) counts only here, so tests that clamp on purpose leave the machine's numbers alone.
    clamped: AtomicU64,
    /// `push_paced` waits here for a reader to take events (`reader_took`).
    room_wq: WaitQueue,
}

/// Who holds a device's grab, and whether the device moves a pointer (`Caps::moves_pointer`: IN-6b's switch).
#[derive(Clone, Copy)]
struct Grab {
    by: u64,
    pointer: bool,
}

/// The sink's ring and the injected packets still open, under one lock.
struct Pending {
    ring: VecDeque<InputEvent>,
    /// Injected packets without their SYN_REPORT yet: (device, source) -> the time the packet's first event got.
    /// Keyed by source too, so a program's unfinished write and an MCP call on the same device never share a time.
    open: Vec<(u32, Source, u64)>,
}

impl Pending {
    /// IN-6b: the time of an injected event, once per packet (Linux `input_get_timestamp`,
    /// linux-6.6/drivers/input/input.c:2083-2088: the packet's first event stamps it, the SYN_REPORT ends it).
    /// The first event keeps its caller's time (MCP takes one per call) unless that is older than the newest
    /// event queued (`last`; a real report came in after the caller took it) or the event was written to
    /// `/dev/input` (Linux evdev_write never passes the writer's time): then the sink's clock, read here under
    /// its lock, so the packet goes in order instead of being clamped. An open packet whose time fell behind the
    /// newest event (another event came in meanwhile, or a writer never sent its SYN_REPORT) starts again with a
    /// fresh time: its later events go in order instead of every one being clamped (fixer, checker finding 1/8).
    fn packet_time(&mut self, ev: &InputEvent, src: Source, last: u64) -> u64 {
        let end = ev.kind == EV_SYN && ev.code == SYN_REPORT;
        if let Some(i) = self.open.iter().position(|o| o.0 == ev.device && o.1 == src) {
            let t = self.open[i].2;
            if end || t < last {
                self.open.swap_remove(i);
            }
            if t >= last {
                return t;
            }
        }
        let t = if src == Source::Write || ev.time_ns < last {
            if src == Source::Inject {
                RESTAMPED.inc();
            }
            // Never behind the newest queued (an interrupt's stamp may lead this read of the clock by a little).
            time::now_ns().max(last)
        } else {
            ev.time_ns
        };
        // Room stays allocated after the first packet; without it the packet's events take one time each.
        if !end && self.open.try_reserve(1).is_ok() {
            self.open.push((ev.device, src, t));
        }
        t
    }
}

klog::counter!(TIME_CLAMPED, "input.time_clamped");
klog::counter!(CLAMPED_USB, "input.time_clamped.usb");
klog::counter!(CLAMPED_DEVICE, "input.time_clamped.device");
klog::counter!(CLAMPED_INJECT, "input.time_clamped.inject");
klog::counter!(CLAMPED_WRITE, "input.time_clamped.write");
klog::counter!(CLAMPED_READER, "input.time_clamped.reader");
// Injected packets whose caller's time was older than the newest queued event: stamped by the sink instead.
klog::counter!(RESTAMPED, "input.time_restamped");

/// Where a clamped event came from (IN-6b: `input.time_clamped.<source>`, each event counted once).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `InputDevice::emit`/`release` of a device on the USB bus (usb-hid, stamped at the controller interrupt).
    Usb,
    /// Any other registered device (`BUS_VIRTUAL` fakes of the ktests, later PS/2 or Bluetooth), and plain
    /// `push` of an event not marked injected.
    Device,
    /// `push` of an injected event: MCP `send_keys`/`send_mouse`, ktests typing as MCP does.
    Inject,
    /// A program wrote events to `/dev/input/eventN` (`push_written`).
    Write,
    /// An event in order at the sink that met a newer one in a reader's own queue (`/dev/input` soft repeat,
    /// a grab's releases): counted where the reader's queue clamps it.
    Reader,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Usb => "usb",
            Source::Device => "device",
            Source::Inject => "inject",
            Source::Write => "write",
            Source::Reader => "reader",
        }
    }
    fn counter(self) -> &'static klog::metrics::Counter {
        match self {
            Source::Usb => &CLAMPED_USB,
            Source::Device => &CLAMPED_DEVICE,
            Source::Inject => &CLAMPED_INJECT,
            Source::Write => &CLAMPED_WRITE,
            Source::Reader => &CLAMPED_READER,
        }
    }
    /// The machine-wide count of clamps from this source (`input.time_clamped.<name>`).
    pub fn clamped(self) -> u64 {
        self.counter().get()
    }
}

/// Counts one clamp found under a queue's lock; called after the lock is released. Event on the first clamp of
/// a source and on each doubling (B6, rate-limited as `evdev.dropped`).
pub(crate) fn note_clamp(src: Source, behind_ns: u64) {
    TIME_CLAMPED.inc();
    let c = src.counter();
    c.inc();
    let n = c.get();
    if n.is_power_of_two() {
        klog::event!(Info, "input", "evdev.time_clamped", source = src.name(), clamped = n, behind_ns = behind_ns;
            "an event stamped before one already queued got the newer time (counted per source from here on)");
    }
}

/// IN-6: an event stamped before one already handed out (two devices stamped on two CPUs, pushed in the other
/// order) gets the newer time, so every reader sees non-decreasing times as from Linux's one monotonic clock
/// (linux-6.6/drivers/input/input.c input_set_timestamp: one ktime per packet). Called under the queue's lock;
/// returns the event and how far it was behind (0: not clamped), which the caller counts after the lock.
pub(crate) fn monotonic(last: &AtomicU64, ev: InputEvent) -> (InputEvent, u64) {
    let prev = last.load(Ordering::Relaxed);
    if ev.time_ns >= prev {
        last.store(ev.time_ns, Ordering::Relaxed);
        return (ev, 0);
    }
    (InputEvent { time_ns: prev, ..ev }, prev - ev.time_ns)
}

/// Something the sink hands events to (pusher context: an input driver's thread or an MCP worker; no sleeping, no
/// waiting, only short locks): the VT's `Client`, IN-4's `/dev/input/eventN` handles.
pub trait Listener: Send + Sync {
    fn push(&self, ev: InputEvent);
    /// Only the events of this device (its event id, `InputDevice::event_id`); None = every device.
    fn device(&self) -> Option<u32> {
        None
    }
    /// Events that still fit before this reader's queue overflows (`push_paced` waits for it).
    fn room(&self) -> usize {
        usize::MAX
    }
    /// Events this reader's queue threw away so far (overflows).
    fn lost(&self) -> u64 {
        0
    }
}

/// A reader with its own queue (Linux `evdev_client`): every event pushed after `subscribe` lands in it, bounded
/// like the sink's ring (drop the oldest, then `take_resync`).
pub struct Client {
    pub name: &'static str,
    q: SpinLock<VecDeque<InputEvent>>,
    dropped: AtomicU64,
    resync: AtomicBool,
    wq: WaitQueue,
    /// The newest time handed out (IN-6: a reader's timestamps never go back).
    last_ns: AtomicU64,
}

impl Client {
    pub fn pop(&self) -> Option<InputEvent> {
        self.q.lock().pop_front()
    }

    pub fn pending(&self) -> usize {
        self.q.lock().len()
    }
    /// Waits up to `ns` for an event (true when one is there).
    pub fn wait(&self, ns: u64) -> bool {
        self.wq.wait_until_timeout(|| !self.q.lock().is_empty(), ns)
    }
    pub fn take_resync(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Listener for Client {
    fn push(&self, ev: InputEvent) {
        let (full, behind) = {
            let mut q = self.q.lock();
            let (ev, behind) = monotonic(&self.last_ns, ev);
            let full = q.len() >= MAX_PENDING;
            if full {
                q.pop_front();
            }
            q.push_back(ev);
            (full, behind)
        };
        if behind > 0 {
            note_clamp(Source::Reader, behind);
        }
        if full {
            self.resync.store(true, Ordering::Release);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.wq.wake_all();
    }
    fn room(&self) -> usize {
        MAX_PENDING - self.q.lock().len().min(MAX_PENDING)
    }
    fn lost(&self) -> u64 {
        self.dropped()
    }
}

const MAX_PENDING: usize = 4096;
/// `push_paced`: room a packet waits for at most (a quarter of the smallest ring, Linux's 64), how long a
/// reader may take nothing before the call goes on unpaced, and how often the room is looked at meanwhile
/// (a reader of the sink's `Client` queue does not wake the injector).
const MIN_ROOM: usize = 16;
const STALL_NS: u64 = 250_000_000;
const POLL_NS: u64 = 1_000_000;
const STALL_LOOKS: u32 = 1000;
klog::counter!(PACE_STALLS, "input.pace_stalled");
/// Events kept for observers (`recent_since`): a look-back window, not a count of things.
const RECENT: usize = 1024;
const LOCK_BITS: u32 = 1 << LED_NUML | 1 << LED_CAPSL | 1 << LED_SCROLLL;

impl EvdevSink {
    pub const fn new() -> Self {
        EvdevSink {
            q: SpinLock::named("evdev.q", Pending { ring: VecDeque::new(), open: Vec::new() }),
            wq: WaitQueue::new(),
            dropped: AtomicU64::new(0),
            resync: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            recent: SpinLock::named("evdev.recent", VecDeque::new()),
            held: SpinLock::named("evdev.held", KeyUnion::new()),
            locks: AtomicU32::new(0),
            listeners: SpinLock::named("evdev.leds", Vec::new()),
            next_listener: AtomicU64::new(1),
            clients: SpinLock::named("evdev.clients", Vec::new()),
            next_client: AtomicU64::new(1),
            grabs: SpinLock::named("evdev.grabs", BTreeMap::new()),
            popped: AtomicBool::new(false),
            last_ns: AtomicU64::new(0),
            clamped: AtomicU64::new(0),
            room_wq: WaitQueue::new(),
        }
    }

    /// An event from MCP or a ktest (`injected`: one time per packet, the sink's clock when the caller's is
    /// stale, `Source::Inject`) or one not marked injected (its own time, `Source::Device`).
    pub fn push(&self, ev: InputEvent) {
        let src = if ev.injected { Source::Inject } else { Source::Device };
        if self.push_quiet(ev, src) {
            self.notify();
        }
    }

    /// An event a program wrote to `/dev/input/eventN` (injected; the sink's clock per packet).
    pub fn push_written(&self, ev: InputEvent) {
        if self.push_quiet(InputEvent { injected: true, ..ev }, Source::Write) {
            self.notify();
        }
    }

    /// Clamps on this sink so far (IN-6b; see the field).
    pub fn clamped(&self) -> u64 {
        self.clamped.load(Ordering::Relaxed)
    }

    /// Queues the event; true when it toggled a lock (the caller notifies, outside its own locks).
    fn push_quiet(&self, ev: InputEvent, src: Source) -> bool {
        let edge = self.held.lock().apply(&ev);
        let (ev, dropped, behind) = {
            let mut q = self.q.lock();
            let ev = match src {
                Source::Inject | Source::Write => {
                    InputEvent { time_ns: q.packet_time(&ev, src, self.last_ns.load(Ordering::Relaxed)), ..ev }
                }
                _ => ev,
            };
            let (ev, behind) = monotonic(&self.last_ns, ev);
            let full = q.ring.len() >= MAX_PENDING;
            if full {
                q.ring.pop_front();
            }
            q.ring.push_back(ev);
            // IN-6b: the clients' lock is taken before the ring's is let go, so every reader gets the events in
            // the sink's order with the sink's times (two pushers on two CPUs used to reach a reader the other
            // way round, and each reader clamped and counted the same event again).
            let clients = self.clients.lock();
            drop(q);
            // The grab is read under the clients' lock (order evdev.q -> evdev.clients -> evdev.grabs): a grab
            // applies from the next event on, as evdev_events reads dev->grab and delivers in one RCU section.
            let grab = self.grabs.lock().get(&ev.device).map(|g| g.by);
            for (id, c) in clients.iter() {
                if c.device().is_some_and(|d| d != ev.device) || grab.is_some_and(|g| g != *id) {
                    continue;
                }
                c.push(ev);
            }
            (ev, full, behind)
        };
        if behind > 0 {
            self.clamped.fetch_add(1, Ordering::Relaxed);
            if core::ptr::eq(self, sink()) {
                note_clamp(src, behind);
            }
        }
        // The sink's own ring only counts as dropping once something reads it (`pop`: ktests, observers); in the
        // running kernel nothing does (the VT and /dev/input have queues of their own), it is just a full window.
        if dropped && self.popped.load(Ordering::Acquire) {
            self.resync.store(true, Ordering::Release);
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                klog::event!(Warn, "input", "evdev.dropped", dropped = n, pending = MAX_PENDING;
                    "the reader is not reading: oldest events dropped, reader must resync its held keys");
            }
        }
        {
            let mut r = self.recent.lock();
            let seq = self.seq.fetch_add(1, Ordering::AcqRel);
            if r.len() >= RECENT {
                r.pop_front();
            }
            r.push_back((seq, ev));
        }
        self.wq.wake_all();
        let bit = match (edge, ev.kind, ev.code) {
            (Some(true), EV_KEY, KEY_CAPSLOCK) => 1 << LED_CAPSL,
            (Some(true), EV_KEY, KEY_NUMLOCK) => 1 << LED_NUML,
            (Some(true), EV_KEY, KEY_SCROLLLOCK) => 1 << LED_SCROLLL,
            _ => return false,
        };
        self.locks.fetch_xor(bit, Ordering::AcqRel);
        true
    }

    pub fn pop(&self) -> Option<InputEvent> {
        self.popped.store(true, Ordering::Release);
        self.q.lock().ring.pop_front()
    }

    /// A reader with its own queue (the sink's ring stays for `pop`).
    pub fn subscribe(&self, name: &'static str) -> Arc<Client> {
        let c = Arc::new(Client {
            name,
            q: SpinLock::named("evdev.client", VecDeque::new()),
            dropped: AtomicU64::new(0),
            resync: AtomicBool::new(false),
            wq: WaitQueue::new(),
            last_ns: AtomicU64::new(0),
        });
        self.attach(c.clone());
        c
    }
    pub fn unsubscribe(&self, c: &Arc<Client>) {
        let me = Arc::as_ptr(c) as *const ();
        let ids: Vec<u64> = self.clients.lock().iter().filter(|(_, x)| Arc::as_ptr(x) as *const () == me).map(|(i, _)| *i).collect();
        for id in ids {
            self.detach(id);
        }
    }
    /// Adds a listener; returns its id (for `detach`, `grab`).
    pub fn attach(&self, l: Arc<dyn Listener>) -> u64 {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.clients.lock().push((id, l));
        id
    }
    /// Removes a listener and the grabs it held.
    pub fn detach(&self, id: u64) {
        {
            let mut g = self.grabs.lock();
            g.retain(|_, g| g.by != id);
            self.pointer_switch(&g);
        }
        self.clients.lock().retain(|(i, _)| *i != id);
    }

    /// IN-6b (WY-1a change request 2, IN-6 change request 4): while a program holds the grab of a pointer device
    /// on the machine's sink it draws its own pointer, so the console draws none over programs. Derived from the
    /// grabs themselves (no count to drift); called under the grab lock so switches come in order. Switching off
    /// also takes the arrow off the screen (`pointer_over::clear`, which may wait): only `grab` can switch off,
    /// and it runs that after the lock.
    fn pointer_switch(&self, grabs: &BTreeMap<u32, Grab>) -> bool {
        core::ptr::eq(self, &SINK) && crate::pointer_over::set_grabbed(grabs.values().any(|g| g.pointer))
    }

    /// EVIOCGRAB: from now on only listener `id` gets the events of device `event_id` (evdev.c:341-355: EBUSY
    /// while another holds it). Keys the device holds right now are released for every listener that follows all
    /// devices (the VT), so none of them keeps a key held or repeating it will never see let go.
    pub fn grab(&self, event_id: u32, id: u64) -> Result<(), i64> {
        let pointer = DEVICES.lock().iter().find(|d| d.event_id() == event_id).is_some_and(|d| d.info.caps.moves_pointer());
        let off = {
            let mut g = self.grabs.lock();
            if g.contains_key(&event_id) {
                return Err(uapi::errno::EBUSY);
            }
            g.insert(event_id, Grab { by: id, pointer });
            self.pointer_switch(&g)
        };
        if off {
            crate::pointer_over::clear();
        }
        let held = self.held.lock().holders_of(event_id);
        if !held.is_empty() {
            // The releases take the sink's newest time under its ring lock, then the clients' lock is taken before
            // the ring's is let go (as `push_quiet`): no push gets between, and an event stamped at its interrupt
            // before now but after the newest queued one still goes in order (fixer, checker finding 2).
            let q = self.q.lock();
            let t = match self.last_ns.load(Ordering::Relaxed) {
                0 => time::now_ns(),
                t => t,
            };
            self.last_ns.fetch_max(t, Ordering::Relaxed);
            let clients = self.clients.lock();
            drop(q);
            for (cid, c) in clients.iter() {
                if *cid == id || c.device().is_some() {
                    continue;
                }
                for &(code, injected) in &held {
                    c.push(InputEvent { time_ns: t, kind: EV_KEY, code, value: 0, device: event_id, injected });
                }
                c.push(InputEvent { time_ns: t, kind: EV_SYN, code: SYN_REPORT, value: 0, device: event_id, injected: held[0].1 });
            }
        }
        Ok(())
    }
    /// EVIOCGRAB(0): EINVAL unless `id` holds the grab (evdev.c:357-370).
    pub fn ungrab(&self, event_id: u32, id: u64) -> Result<(), i64> {
        let mut g = self.grabs.lock();
        if g.get(&event_id).map(|g| g.by) != Some(id) {
            return Err(uapi::errno::EINVAL);
        }
        g.remove(&event_id);
        self.pointer_switch(&g);
        Ok(())
    }
    /// `f` over the readers that get the events of `device` now (their device filter and the grab applied).
    fn fold_readers<T>(&self, device: u32, init: T, mut f: impl FnMut(T, &dyn Listener) -> T) -> T {
        let clients = self.clients.lock();
        let grab = self.grabs.lock().get(&device).map(|g| g.by);
        let mut acc = init;
        for (id, c) in clients.iter() {
            if c.device().is_some_and(|d| d != device) || grab.is_some_and(|g| g != *id) {
                continue;
            }
            acc = f(acc, &**c);
        }
        acc
    }
    /// Events the readers of `device` threw away so far (their queues overflowed). The sink's own ring is not a
    /// reader: in the running kernel nothing `pop`s it, so it is full and drops all the time.
    pub fn readers_lost(&self, device: u32) -> u64 {
        self.fold_readers(device, 0u64, |a, c| a.saturating_add(c.lost()))
    }
    /// The smallest room left in the queues of the readers of `device` (usize::MAX: no reader).
    pub fn readers_room(&self, device: u32) -> usize {
        self.fold_readers(device, usize::MAX, |a, c| a.min(c.room()))
    }
    /// Injected events (MCP `send_keys`/`send_mouse`, one call's worth) at the pace of a keyboard: each packet
    /// (up to its SYN_REPORT) goes in only when every reader of its device has room for it, so a burst of a
    /// few hundred events never overflows a reader's 64-event ring (Linux evdev's size for a keyboard,
    /// evdev.c:461) with SYN_DROPPED and loses a chunk of what was typed. A real keyboard cannot send faster
    /// than a reader keeps up; an injector that pushed a whole call at once could. A reader that takes nothing
    /// for `STALL_NS` (a hung program) no longer holds the call back: the rest goes in unpaced and that
    /// reader's ring overflows as on Linux. Returns the events the readers of the device lost meanwhile.
    pub fn push_paced(&self, events: &[InputEvent]) -> u64 {
        let Some(device) = events.first().map(|e| e.device) else { return 0 };
        let before = self.readers_lost(device);
        let mut stalled = false;
        let mut i = 0;
        while i < events.len() {
            let end = events[i..].iter().position(|e| e.kind == EV_SYN && e.code == SYN_REPORT).map_or(events.len(), |p| i + p + 1);
            // Bounded: a packet longer than any ring waits for an empty ring at most.
            let need = (end - i).min(MIN_ROOM);
            if !stalled {
                stalled = !self.wait_room(device, need);
            }
            // Stamped when it goes in, as a keyboard stamps its report: a packet that waited must not carry the
            // call's start time (its press would look held since then, and the soft repeat would fire at once).
            let now = time::now_ns();
            for e in &events[i..end] {
                self.push(InputEvent { time_ns: e.time_ns.max(now), ..*e });
            }
            i = end;
        }
        self.readers_lost(device).saturating_sub(before)
    }

    /// Waits until the readers of `device` have room for `need` events; false when none took anything for
    /// `STALL_NS` (given up: the call goes on unpaced).
    fn wait_room(&self, device: u32, need: usize) -> bool {
        let mut room = self.readers_room(device);
        let mut since = time::now_ns();
        // Looks without progress: the bound when the clock does not run (host tests). A look may end long before
        // POLL_NS (any reader's read wakes it), so with a clock only the time counts.
        let mut idle = 0u32;
        while room < need {
            self.room_wq.wait_until_timeout(|| self.readers_room(device) != room, POLL_NS);
            let now = time::now_ns();
            let r = self.readers_room(device);
            idle += 1;
            if r > room {
                (since, idle) = (now, 0);
            } else if now.saturating_sub(since) >= STALL_NS || (now == since && idle >= STALL_LOOKS) {
                PACE_STALLS.inc();
                klog::event!(Warn, "input", "evdev.pace_stalled", device = device, room = r, need = need;
                    "a reader of the injected device took nothing for 250 ms: the rest of the call goes in unpaced");
                return false;
            }
            room = r;
        }
        true
    }

    /// A reader took events from its queue: an injector waiting for room looks again.
    pub fn reader_took(&self) {
        self.room_wq.wake_all();
    }

    /// Who holds the grab of a device.
    pub fn grabbed_by(&self, event_id: u32) -> Option<u64> {
        self.grabs.lock().get(&event_id).map(|g| g.by)
    }
    /// A copy of the held-key union (a reader rebuilding its state after dropped events).
    pub fn union_snapshot(&self) -> KeyUnion {
        self.held.lock().clone()
    }
    pub fn wait_queue(&self) -> &WaitQueue {
        &self.wq
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    /// True once after events were dropped (Linux SYN_DROPPED): the reader rebuilds its held keys from
    /// `held_keys`/`key_held_physical` instead of trusting the events it saw.
    pub fn take_resync(&self) -> bool {
        self.resync.swap(false, Ordering::AcqRel)
    }

    /// The sequence number the next pushed event will get.
    pub fn next_seq(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
    /// Events with a sequence number >= `seq` still in the look-back window, oldest first. Nothing is taken from
    /// the reader's ring.
    pub fn recent_since(&self, seq: u64) -> Vec<(u64, InputEvent)> {
        self.recent.lock().iter().filter(|(s, _)| *s >= seq).copied().collect()
    }

    /// Whether any device (MCP injection included) holds `code`.
    pub fn key_held(&self, code: u16) -> bool {
        self.held.lock().held(code)
    }
    /// Whether a real device (not MCP injection) holds `code` (R8c: physical-key-only actions).
    pub fn key_held_physical(&self, code: u16) -> bool {
        self.held.lock().held_physical(code)
    }
    /// Every key any device holds, ascending.
    pub fn held_keys(&self) -> Vec<u16> {
        self.held.lock().keys()
    }

    /// Lock-LED bits (`1 << LED_NUML | 1 << LED_CAPSL | 1 << LED_SCROLLL`).
    pub fn locks(&self) -> u32 {
        self.locks.load(Ordering::Acquire)
    }
    /// Sets the lock state (the VT, a console command); listeners hear about a change.
    pub fn set_locks(&self, bits: u32) {
        let bits = bits & LOCK_BITS;
        if self.locks.swap(bits, Ordering::AcqRel) != bits {
            self.notify();
        }
    }
    /// Registers a lock-LED listener; returns the id for `remove_led_listener`.
    pub fn add_led_listener(&self, f: LedListener) -> u64 {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, f));
        id
    }
    pub fn remove_led_listener(&self, id: u64) {
        self.listeners.lock().retain(|(i, _)| *i != id);
    }
    /// Tells the listeners the lock state as it is now (not as the caller changed it: another CPU may have
    /// changed it since, and the last call must carry the latest state).
    fn notify(&self) {
        let all: Vec<LedListener> = self.listeners.lock().iter().map(|(_, f)| f.clone()).collect();
        for f in all {
            f(self.locks());
        }
    }
}

impl Default for EvdevSink {
    fn default() -> Self {
        Self::new()
    }
}

static SINK: EvdevSink = EvdevSink::new();

pub fn sink() -> &'static EvdevSink {
    &SINK
}

// ---- held-key union

/// Which devices hold which keys. A key is held while any device holds it; a device's release ends the union
/// only when no other device still holds the key (V1: "a key counts as released only when every keyboard
/// released it"). A holder is (device number, injected): MCP injection never shares a holder with a real
/// device, whatever number it uses.
#[derive(Clone, Debug, Default)]
pub struct KeyUnion {
    by_key: BTreeMap<u16, Vec<(u32, bool)>>,
}

impl KeyUnion {
    pub const fn new() -> Self {
        KeyUnion { by_key: BTreeMap::new() }
    }
    /// Takes one event; `Some(true)` when the union of its key went from released to held, `Some(false)` when
    /// it went back, `None` otherwise (other kinds, repeats, a key another device holds too).
    pub fn apply(&mut self, ev: &InputEvent) -> Option<bool> {
        if ev.kind != EV_KEY || ev.value == 2 {
            return None;
        }
        if ev.value == 0 {
            let holders = self.by_key.get_mut(&ev.code)?;
            let before = holders.len();
            holders.retain(|&h| h != (ev.device, ev.injected));
            if holders.len() == before || !holders.is_empty() {
                return None;
            }
            self.by_key.remove(&ev.code);
            return Some(false);
        }
        let holders = self.by_key.entry(ev.code).or_default();
        if holders.contains(&(ev.device, ev.injected)) {
            return None;
        }
        holders.push((ev.device, ev.injected));
        (holders.len() == 1).then_some(true)
    }
    pub fn held(&self, code: u16) -> bool {
        self.by_key.contains_key(&code)
    }
    pub fn held_physical(&self, code: u16) -> bool {
        self.by_key.get(&code).is_some_and(|h| h.iter().any(|(_, injected)| !injected))
    }
    pub fn keys(&self) -> Vec<u16> {
        self.by_key.keys().copied().collect()
    }
    /// The keys `device` holds, each with the holder's injected flag.
    pub fn holders_of(&self, device: u32) -> Vec<(u16, bool)> {
        let mut v = Vec::new();
        for (&k, h) in &self.by_key {
            for &(d, inj) in h {
                if d == device {
                    v.push((k, inj));
                }
            }
        }
        v
    }
}

// ---- input devices

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// What a device can report (Linux `keybit`/`relbit`/`absbit`/`mscbit`/`ledbit`, `EV_REP`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    pub keys: BTreeSet<u16>,
    pub rel: BTreeSet<u16>,
    pub abs: BTreeMap<u16, AbsInfo>,
    pub msc: BTreeSet<u16>,
    pub leds: BTreeSet<u16>,
    pub repeat: bool,
}

impl Caps {
    /// Has keys below BTN_MISC (Linux's VT keyboard handler takes such devices, keyboard.c `kbd_match`).
    pub fn is_keyboard(&self) -> bool {
        self.keys.range(1..0x100).next().is_some()
    }
    /// Moves a pointer: relative axes, or absolute X and Y.
    pub fn is_pointer(&self) -> bool {
        !self.rel.is_empty() || (self.abs.contains_key(&0) && self.abs.contains_key(&1))
    }
    /// Drives the screen's pointer as Linux's mousedev takes a device (linux-6.6/drivers/input/mousedev.c:1002-1046):
    /// REL_X and REL_Y with BTN_LEFT, or ABS_X and ABS_Y with BTN_TOUCH, BTN_TOOL_FINGER, BTN_TOOL_PEN or BTN_LEFT.
    /// A gamepad's or joystick's sticks (ABS_X/ABS_Y with BTN_GAMEPAD or BTN_JOYSTICK only) do not (IN-6b grab switch).
    pub fn moves_pointer(&self) -> bool {
        const BTN_LEFT: u16 = 0x110;
        const BTN_TOOL_PEN: u16 = 0x140;
        const BTN_TOOL_FINGER: u16 = 0x145;
        const BTN_TOUCH: u16 = 0x14a;
        let rel = self.rel.contains(&0) && self.rel.contains(&1) && self.keys.contains(&BTN_LEFT);
        let abs = self.abs.contains_key(&0)
            && self.abs.contains_key(&1)
            && [BTN_LEFT, BTN_TOOL_PEN, BTN_TOOL_FINGER, BTN_TOUCH].iter().any(|b| self.keys.contains(b));
        rel || abs
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// "USB HID 046d:c52b Keyboard"-style name (Linux `input_dev->name`).
    pub name: String,
    /// Where it sits, e.g. "xhci0-4/input0" (Linux `phys`; empty = none, EVIOCGPHYS answers ENOENT).
    pub phys: String,
    /// Linux `uniq` (a USB HID device's serial string, empty for most); None = none (EVIOCGUNIQ: ENOENT).
    pub uniq: Option<String>,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub caps: Caps,
}

#[derive(Default)]
struct DevState {
    keys: BTreeSet<u16>,
    abs: BTreeMap<u16, i32>,
    leds: BTreeSet<u16>,
    /// Something passed since the last SYN_REPORT.
    pending: bool,
    gone: bool,
}

/// One input device (one future `/dev/input/eventN`).
pub struct InputDevice {
    pub number: u32,
    pub info: DeviceInfo,
    /// A virtual device whose events reach the sink with this device number (MCP's injected keyboard and
    /// pointer, `INJECT_KEYBOARD`/`INJECT_POINTER`): its `/dev/input/eventN` shows those events (IN-4).
    pub alias: Option<u32>,
    /// EVIOCGREP/EVIOCSREP: (delay ms, period ms).
    rep: SpinLock<(u32, u32)>,
    /// EVIOCGABS/EVIOCSABS: the axes' info as programs set it (starts as `info.caps.abs`).
    absinfo: SpinLock<BTreeMap<u16, AbsInfo>>,
    sink: &'static EvdevSink,
    st: SpinLock<DevState>,
    events: AtomicU64,
    first_logged: AtomicBool,
}

klog::counter!(EVENTS, "input.events");
klog::counter!(RELEASED, "input.released_on_unplug");

static DEVICES: SpinLock<Vec<Arc<InputDevice>>> = SpinLock::named("evdev.devices", Vec::new());

/// Registers an input device feeding `sink` (the kernel's is `sink()`); it gets the smallest free number. Devices
/// of the kernel's sink get their `/dev/input/eventN` (IN-4, `evdevfs`).
pub fn register(sink: &'static EvdevSink, info: DeviceInfo) -> Arc<InputDevice> {
    register_as(sink, info, None)
}

/// A virtual device for events that reach the sink with device number `alias` (MCP injection), so programs can
/// read them from its `/dev/input/eventN` like any keyboard or mouse.
pub fn register_virtual(info: DeviceInfo, alias: u32) -> Arc<InputDevice> {
    register_as(&SINK, info, Some(alias))
}

fn register_as(sink: &'static EvdevSink, info: DeviceInfo, alias: Option<u32>) -> Arc<InputDevice> {
    let dev = {
        let mut l = DEVICES.lock();
        let used: BTreeSet<u32> = l.iter().map(|d| d.number).collect();
        let number = (0u32..).find(|n| !used.contains(n)).unwrap_or(u32::MAX);
        let abs = info.caps.abs.clone();
        let d = Arc::new(InputDevice {
            number,
            info,
            alias,
            rep: SpinLock::named("evdev.rep", (crate::repeat::DELAY_NS as u32 / 1_000_000, crate::repeat::RATE_NS as u32 / 1_000_000)),
            absinfo: SpinLock::named("evdev.absinfo", abs),
            sink,
            st: SpinLock::named("evdev.dev", DevState::default()),
            events: AtomicU64::new(0),
            first_logged: AtomicBool::new(false),
        });
        l.push(d.clone());
        d
    };
    let c = &dev.info.caps;
    klog::event!(Info, "input", "device.registered", number = dev.number, name = dev.info.name.as_str(),
        phys = dev.info.phys.as_str(), keys = c.keys.len(), rel = c.rel.len(), abs = c.abs.len(), leds = c.leds.len());
    if core::ptr::eq(sink, &SINK) {
        crate::evdevfs::changed();
    }
    dev
}

/// Every registered device, by number.
pub fn devices() -> Vec<Arc<InputDevice>> {
    let mut v = DEVICES.lock().clone();
    v.sort_by_key(|d| d.number);
    v
}

impl InputDevice {
    /// Hands events (type, code, value) to the input core: what the device has and what changed goes into the
    /// sink with this device's number and `injected = false`. Returns how many passed. After `release_all` or
    /// `unregister` nothing passes.
    pub fn emit(&self, time_ns: u64, events: &[(u16, u16, i32)]) -> usize {
        let mut toggled = false;
        let mut passed = 0;
        {
            let mut st = self.st.lock();
            if st.gone {
                return 0;
            }
            for &(kind, code, value) in events {
                let Some(value) = self.disposition(&mut st, kind, code, value) else { continue };
                if kind != EV_SYN {
                    st.pending = true;
                }
                let ev = InputEvent { time_ns, kind, code, value, device: self.number, injected: false };
                // Pushed under the device's lock, so an unplug's releases always come after the presses.
                toggled |= self.sink.push_quiet(ev, self.source());
                passed += 1;
            }
        }
        if toggled {
            self.sink.notify();
        }
        if passed > 0 {
            EVENTS.add(passed as u64);
            self.events.fetch_add(passed as u64, Ordering::Relaxed);
            if !self.first_logged.swap(true, Ordering::AcqRel) {
                klog::event!(Info, "input", "device.first_event", number = self.number, name = self.info.name.as_str());
            }
        }
        passed
    }

    /// Linux `input_get_disposition` + `input_handle_abs_event`: the value to pass, or None to drop it.
    fn disposition(&self, st: &mut DevState, kind: u16, code: u16, value: i32) -> Option<i32> {
        let caps = &self.info.caps;
        match kind {
            EV_SYN if code == SYN_REPORT => core::mem::replace(&mut st.pending, false).then_some(value),
            EV_KEY if caps.keys.contains(&code) => {
                if value == 2 {
                    return Some(value);
                }
                if !toggle(&mut st.keys, code, value != 0) {
                    return None;
                }
                Some(value)
            }
            EV_REL if caps.rel.contains(&code) && value != 0 => Some(value),
            EV_ABS => {
                let info = caps.abs.get(&code)?;
                let old = st.abs.get(&code).copied().unwrap_or(0);
                let v = defuzz(value, old, info.fuzz);
                if v == old {
                    return None;
                }
                st.abs.insert(code, v);
                Some(v)
            }
            EV_MSC if caps.msc.contains(&code) => Some(value),
            EV_LED if caps.leds.contains(&code) => toggle(&mut st.leds, code, value != 0).then_some(value),
            _ => None,
        }
    }

    /// Releases every key this device holds (key-ups and one SYN_REPORT) and lets nothing pass afterwards.
    /// Returns the number of keys released.
    pub fn release_all(&self, time_ns: u64) -> usize {
        self.release(time_ns, true)
    }

    /// What the sink counts a clamp of this device's events as (IN-6b).
    fn source(&self) -> Source {
        if self.info.bus == BUS_USB { Source::Usb } else { Source::Device }
    }

    /// Releases every key this device holds and keeps the device open: its reports stopped for a while (a pipe
    /// the controller gave up, restarted by the driver), so nothing may stay held meanwhile. Returns the number
    /// of keys released.
    pub fn release_keys(&self, time_ns: u64) -> usize {
        self.release(time_ns, false)
    }

    fn release(&self, time_ns: u64, gone: bool) -> usize {
        let mut toggled = false;
        let n = {
            let mut st = self.st.lock();
            if st.gone {
                return 0;
            }
            st.gone = gone;
            let keys: Vec<u16> = core::mem::take(&mut st.keys).into_iter().collect();
            let mut push = |kind, code| {
                let ev = InputEvent { time_ns, kind, code, value: 0, device: self.number, injected: false };
                toggled |= self.sink.push_quiet(ev, self.source());
            };
            for &k in &keys {
                push(EV_KEY, k);
            }
            if !keys.is_empty() {
                push(EV_SYN, SYN_REPORT);
            }
            st.pending = false;
            keys.len()
        };
        if toggled {
            self.sink.notify();
        }
        n
    }

    /// The device is gone: its keys released at once, its number freed (and its `/dev/input/eventN` removed).
    pub fn unregister(&self, time_ns: u64) {
        let released = self.release_all(time_ns);
        DEVICES.lock().retain(|d| !core::ptr::eq(&**d, self));
        if let Some(g) = self.sink.grabbed_by(self.event_id()) {
            let _ = self.sink.ungrab(self.event_id(), g);
        }
        if core::ptr::eq(self.sink, &SINK) {
            crate::evdevfs::changed();
        }
        RELEASED.add(released as u64);
        klog::event!(Info, "input", "device.removed", number = self.number, name = self.info.name.as_str(),
            released = released, events = self.events.load(Ordering::Relaxed));
    }

    pub fn held_keys(&self) -> Vec<u16> {
        match self.alias {
            Some(a) => self.sink.held.lock().holders_of(a).into_iter().filter(|h| h.1).map(|h| h.0).collect(),
            None => self.st.lock().keys.iter().copied().collect(),
        }
    }
    /// The device number its events carry in the sink (`number`, or the alias of a virtual device).
    pub fn event_id(&self) -> u32 {
        self.alias.unwrap_or(self.number)
    }
    /// The LEDs it has that are lit now (the console's lock state, shared by every keyboard).
    pub fn leds_on(&self) -> Vec<u16> {
        let locks = self.sink.locks();
        self.info.caps.leds.iter().copied().filter(|&l| l <= LED_SCROLLL && locks & (1 << l) != 0).collect()
    }
    pub fn sink(&self) -> &'static EvdevSink {
        self.sink
    }
    /// EVIOCGABS: (value, info) of an axis the device has.
    pub fn abs(&self, code: u16) -> Option<(i32, AbsInfo)> {
        let info = *self.absinfo.lock().get(&code)?;
        Some((self.abs_value(code).unwrap_or(0), info))
    }
    /// EVIOCSABS on an axis the device has (false otherwise): its info and its current value.
    pub fn set_abs(&self, code: u16, value: i32, a: AbsInfo) -> bool {
        match self.absinfo.lock().get_mut(&code) {
            Some(x) => *x = a,
            None => return false,
        }
        self.st.lock().abs.insert(code, value);
        true
    }
    /// EVIOCGREP / EVIOCSREP (milliseconds).
    pub fn repeat(&self) -> (u32, u32) {
        *self.rep.lock()
    }
    pub fn set_repeat(&self, delay: u32, period: u32) {
        *self.rep.lock() = (delay, period);
    }
    pub fn abs_value(&self, code: u16) -> Option<i32> {
        self.st.lock().abs.get(&code).copied()
    }
    /// Events that passed the filter so far.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }
    pub fn is_gone(&self) -> bool {
        self.st.lock().gone
    }
}

/// Sets `code` in `set` to `on`; true if that changed it.
fn toggle(set: &mut BTreeSet<u16>, code: u16, on: bool) -> bool {
    if on { set.insert(code) } else { set.remove(&code) }
}

/// Linux `input_defuzz_abs_event` (input.c:68-84).
fn defuzz(value: i32, old: i32, fuzz: i32) -> i32 {
    if fuzz > 0 {
        let (v, o, f) = (value as i64, old as i64, fuzz as i64);
        if v > o - f / 2 && v < o + f / 2 {
            return old;
        }
        if v > o - f && v < o + f {
            return ((o * 3 + v) / 4) as i32;
        }
        if v > o - f * 2 && v < o + f * 2 {
            return ((o + v) / 2) as i32;
        }
    }
    value
}

#[cfg(test)]
mod tests;
