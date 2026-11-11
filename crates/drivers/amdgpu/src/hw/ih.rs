//! GX-7n: the IH ring as the CPU reads it. One message per vector (Linux's one MSI, amdgpu_irq.c:279); the hard-IRQ side
//! is the PCI core's count and wake, nothing here. The `amdgpu-ih` thread reads the ring as `amdgpu_ih_process` does
//! (amdgpu_ih.c:208): write pointer with overflow check, entries up to it, read pointer back by doorbell (with
//! RPTR_REARM that is what lets the chip send the next message), write pointer once more. It only counts, wakes and calls
//! hooks: jobs end in the job thread as before.
//!
//! The thread is Interactive: its work is bounded per pass (`PASS_ENTRIES`, then it lets others run) and takes no lock of
//! the chain, so a stage holding the chain does not hold interrupts up, and the MCP (KernelRt) stays ahead of any flood.
//!
//! Flood control is per source, in software, on top of the chip's own (`gpucore::ih::start`) and the PCI core's storm
//! guard: past `FloodCfg::per_window` a source's entries are only counted for `hold_ns`, no wake, no hook. The job
//! thread's look still sees whatever a held source meant (fences and fault registers are read there).
//!
//! The display's sources are named and dropped (GX-D9): a hook comes from DS, once it has turned one on itself.

use super::heap::Window;
use crate::gpucore::ih::{IhAddrs, WB_OFFSET};
use crate::ipmod::ih6::{self, Iv, Source, f};
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use drm::fence::{FenceCb, Waiter};
use ksync::{Mutex, MutexGuard, SpinLock};

/// How long the thread sleeps with no message: long, since the job thread has its own look.
pub const IDLE_NS: u64 = 1_000_000_000;
/// Entries per pass before it lets others run (the whole ring is 8192).
pub const PASS_ENTRIES: u32 = 2048;
/// `AMDGPU_IH_MAX_NUM_IVS`: entries between two read pointer updates.
const BATCH: u32 = 32;
/// Entries of a look from the job thread: enough to move the read pointer, which is what re-arms the message. The rest is
/// the IH thread's, whose message that brings: a Normal thread must not hold the reader's lock against it for long.
const LOOK_ENTRIES: u32 = BATCH;
/// How long `ring_off` waits for a pass on its way (bounded, `PASS_ENTRIES` at most).
const RING_OFF_WAIT_NS: u64 = 1_000_000_000;

/// The ring as this thread reaches it, past the chain (no lock a stage may hold for long).
pub trait IhIo: Send + Sync {
    /// The entry at byte `off` of the ring.
    fn entry(&self, off: u32, out: &mut [u32; 8]) -> bool;
    /// The write-back slot.
    fn wptr_wb(&self) -> Option<u32>;
    /// IH_RB_WPTR.
    fn wptr_reg(&self) -> Option<u32>;
    fn cntl(&self) -> Option<u32>;
    fn set_cntl(&self, v: u32) -> bool;
    /// The doorbell (Linux's WDOORBELL32).
    fn set_rptr(&self, v: u32) -> bool;
}

/// The registers and the doorbell page the ring takes, as `LivePorts` reaches them.
pub trait IhPorts: Send + Sync {
    fn read(&self, byte: u32) -> Option<u32>;
    fn write(&self, byte: u32, v: u32) -> bool;
    /// `byte` from BAR2's start.
    fn doorbell(&self, byte: usize, v: u32) -> bool;
}

/// The ring past the chain: the carve-out through its own window, the rest by `IhPorts`.
pub struct PortIo<P> {
    pub ports: P,
    pub a: IhAddrs,
    pub win: Arc<dyn Window>,
}

impl<P: IhPorts> IhIo for PortIo<P> {
    fn entry(&self, off: u32, out: &mut [u32; 8]) -> bool {
        // Four single loads: the memory is uncached, and `rep movsb` would go byte by byte.
        for i in 0..4 {
            let Some(q) = self.win.read64(u64::from(off) + i as u64 * 8) else { return false };
            out[i * 2] = q as u32;
            out[i * 2 + 1] = (q >> 32) as u32;
        }
        true
    }
    fn wptr_wb(&self) -> Option<u32> {
        self.win.read64(WB_OFFSET).map(|q| q as u32)
    }
    fn wptr_reg(&self) -> Option<u32> {
        self.ports.read(self.a.wptr)
    }
    fn cntl(&self) -> Option<u32> {
        self.ports.read(self.a.cntl)
    }
    fn set_cntl(&self, v: u32) -> bool {
        self.ports.write(self.a.cntl, v)
    }
    fn set_rptr(&self, v: u32) -> bool {
        self.ports.doorbell(ih6::DOORBELL_DW as usize * 4, v)
    }
}

/// What the thread waits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    Message,
    /// A polled look of the PCI core (a held storm, a released set).
    Look,
    Timeout,
}

pub trait IrqLine: Send + Sync {
    fn wait(&self, ns: u64) -> Woke;
    /// Ends every wait from now on (the set released, or the test's).
    fn release(&self);
    /// Messages so far.
    fn messages(&self) -> u64;
    /// Where it arrives, for the tool.
    fn describe(&self) -> String;
}

/// Limits per source.
#[derive(Clone, Copy, Debug)]
pub struct FloodCfg {
    pub per_window: u32,
    pub window_ns: u64,
    pub hold_ns: u64,
}

impl Default for FloodCfg {
    fn default() -> FloodCfg {
        // 100000 entries a second of one source is no program's normal; a hold costs a look of the job thread's.
        FloodCfg { per_window: 1000, window_ns: 10_000_000, hold_ns: 100_000_000 }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Flood {
    window_at: u64,
    in_window: u32,
    held_until: u64,
    held: u64,
}

impl Flood {
    /// `(passes, started, ended with this many held)`.
    fn note(&mut self, now: u64, c: &FloodCfg) -> (bool, bool, Option<u64>) {
        let mut ended = None;
        if self.held_until != 0 {
            if now < self.held_until {
                self.held += 1;
                return (false, false, None);
            }
            ended = Some(self.held);
            *self = Flood::default();
        }
        if now.saturating_sub(self.window_at) >= c.window_ns {
            self.window_at = now;
            self.in_window = 0;
        }
        self.in_window = self.in_window.saturating_add(1);
        if self.in_window > c.per_window {
            self.held_until = now.saturating_add(c.hold_ns).max(1);
            self.held = 1;
            return (false, true, ended);
        }
        (true, false, ended)
    }
}

/// A source's hook (DS): runs under the reader's lock, in the IH thread or a look of the job thread's. So it is bounded,
/// takes no chain, and a slow one holds the ring back, and `until=stop`'s `ring_off` with it.
pub type Hook = Arc<dyn Fn(&Iv) + Send + Sync>;

/// What the IH side and the job side share; it outlives neither's thread.
pub struct IrqFeed {
    counts: [AtomicU64; Source::COUNT],
    hooks: SpinLock<[Option<Hook>; Source::COUNT]>,
    waiter: SpinLock<Option<Arc<Waiter>>>,
    /// EOP and traps so far.
    pub fence_events: AtomicU64,
    pub fault_events: AtomicU64,
    pub entries: AtomicU64,
    pub overflows: AtomicU64,
    pub flood_holds: AtomicU64,
    pub flood_held: AtomicU64,
    /// Entries found with no message: a look found what a message should have said.
    pub late_looks: AtomicU64,
    pub bad_reads: AtomicU64,
    /// Fences seen moved with no interrupt behind them, and how often the job thread went back to polling.
    pub lost: AtomicU64,
    pub fell_back: AtomicU64,
    /// `until=irq_poll`: the job thread polls whatever the watch says.
    pub force_poll: AtomicBool,
    /// The ring is read.
    pub running: AtomicBool,
    owner: SpinLock<Weak<Ih>>,
}

impl IrqFeed {
    pub fn new() -> Arc<IrqFeed> {
        Arc::new(IrqFeed {
            counts: core::array::from_fn(|_| AtomicU64::new(0)),
            hooks: SpinLock::named("amdgpu.ih.hooks", core::array::from_fn(|_| None)),
            waiter: SpinLock::named("amdgpu.ih.waiter", None),
            fence_events: AtomicU64::new(0),
            fault_events: AtomicU64::new(0),
            entries: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            flood_holds: AtomicU64::new(0),
            flood_held: AtomicU64::new(0),
            late_looks: AtomicU64::new(0),
            bad_reads: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            fell_back: AtomicU64::new(0),
            force_poll: AtomicBool::new(false),
            running: AtomicBool::new(false),
            owner: SpinLock::named("amdgpu.ih.owner", Weak::new()),
        })
    }

    pub fn count(&self, s: Source) -> u64 {
        self.counts[s.index()].load(Ordering::Acquire)
    }

    /// The thread woken at every fence and fault from now on.
    pub fn attach(&self, w: Arc<Waiter>) {
        *self.waiter.lock() = Some(w);
    }
    pub fn detach(&self) {
        *self.waiter.lock() = None;
    }

    fn wake(&self) {
        let w = self.waiter.lock().clone();
        if let Some(w) = w {
            w.call();
        }
    }

    /// DS: `hook` for a source it has turned on itself; `None` drops the entries again.
    pub fn set_hook(&self, s: Source, hook: Option<Hook>) {
        self.hooks.lock()[s.index()] = hook;
    }

    fn hook(&self, s: Source) -> Option<Hook> {
        self.hooks.lock()[s.index()].clone()
    }

    /// The job thread saw a fence with no interrupt: a short pass from its side, unless the IH thread is reading anyway.
    pub fn look_now(&self) {
        let ih = self.owner.lock().upgrade();
        if let Some(ih) = ih
            && ih.look().entries > 0
        {
            self.late_looks.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The tool's lines.
    pub fn describe(&self) -> Vec<String> {
        let c = |o: &AtomicU64| o.load(Ordering::Acquire);
        let list: Vec<String> = Source::ALL.iter().filter(|s| self.count(**s) != 0).map(|s| format!("{} {}", s.name(), self.count(*s))).collect();
        let display: u64 = Source::ALL.iter().filter(|s| s.is_display()).map(|s| self.count(*s)).sum();
        let line = self.owner.lock().upgrade().map_or(String::from("no line"), |o| format!("{}, {} messages", o.line.describe(), o.line.messages()));
        alloc::vec![
            format!("backend interrupts: {}, {line}: {} entries ({})", if self.running.load(Ordering::Acquire) { "ring read" } else { "ring off" }, c(&self.entries), if list.is_empty() { String::from("none") } else { list.join(", ") }),
            format!(
                "backend interrupts: {display} display entries dropped (the display's sources stay masked), {} overflows, {} flood holds with {} entries held, {} looks found what no message said, {} fences with no interrupt, {} falls back to polling",
                c(&self.overflows),
                c(&self.flood_holds),
                c(&self.flood_held),
                c(&self.late_looks),
                c(&self.lost),
                c(&self.fell_back),
            ),
        ]
    }

    /// Host tests have no thread: the job thread's round reads the ring if a message came.
    #[cfg(test)]
    pub fn host_pump(&self) {
        let ih = self.owner.lock().upgrade();
        if let Some(ih) = ih
            && ih.line.wait(0) != Woke::Timeout
        {
            ih.pass_now();
        }
    }
}

/// The reader's own state.
struct Reader {
    rptr: u32,
    mask: u32,
    flood: [Flood; Source::COUNT],
    dead_said: bool,
}

/// What a pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pass {
    pub entries: u32,
    pub more: bool,
}

impl Reader {
    fn new() -> Reader {
        Reader { rptr: 0, mask: ih6::RING_BYTES - 1, flood: [Flood::default(); Source::COUNT], dead_said: false }
    }

    /// ih_v6_0_get_wptr. Entries lie on 32 bytes: anything else a chip reports is cut down, never followed.
    fn wptr(&mut self, io: &dyn IhIo, feed: &IrqFeed) -> u32 {
        let Some(mut w) = io.wptr_wb() else {
            feed.bad_reads.fetch_add(1, Ordering::Relaxed);
            return self.rptr;
        };
        if w & f::RB_OVERFLOW != 0 {
            let Some(r) = io.wptr_reg().filter(|&r| r != u32::MAX) else {
                feed.bad_reads.fetch_add(1, Ordering::Relaxed);
                if !self.dead_said {
                    self.dead_said = true;
                    klog::event!(Error, "gx", "ih.wptr_unreadable"; "the write-back says overflow and IH_RB_WPTR does not read: the ring is left alone");
                }
                return self.rptr;
            };
            w = r;
            if w & f::RB_OVERFLOW != 0 {
                w &= !f::RB_OVERFLOW;
                // "start parsing interrupt from the last not overwritten vector (wptr + 32)".
                let next = w.wrapping_add(ih6::ENTRY_BYTES) & self.mask;
                feed.overflows.fetch_add(1, Ordering::Relaxed);
                klog::event!(Warn, "gx", "ih.overflow", wptr = klog::Hex(u64::from(w)), rptr = klog::Hex(u64::from(self.rptr)), next = klog::Hex(u64::from(next)); "IH ring buffer overflow");
                self.rptr = next & !(ih6::ENTRY_BYTES - 1);
                if let Some(c) = io.cntl() {
                    let _ = io.set_cntl(c | f::WPTR_OVERFLOW_CLEAR);
                }
            }
        }
        w & self.mask & !(ih6::ENTRY_BYTES - 1)
    }

    fn dispatch(&mut self, iv: &Iv, now: u64, cfg: &FloodCfg, feed: &IrqFeed, wake: &mut bool) {
        let s = ih6::route(iv.client, iv.src);
        feed.counts[s.index()].fetch_add(1, Ordering::Relaxed);
        feed.entries.fetch_add(1, Ordering::Relaxed);
        let (passes, started, ended) = self.flood[s.index()].note(now, cfg);
        if started {
            feed.flood_holds.fetch_add(1, Ordering::Relaxed);
            klog::event!(Warn, "gx", "ih.flood", source = s.name(), per_window = cfg.per_window, hold_ms = cfg.hold_ns / 1_000_000; "only counted for the hold");
        }
        if let Some(n) = ended {
            feed.flood_held.fetch_add(n, Ordering::Relaxed);
            klog::event!(Info, "gx", "ih.flood_end", source = s.name(), held = n);
            *wake = true;
        }
        match s {
            Source::Eop | Source::SdmaTrap => {
                feed.fence_events.fetch_add(1, Ordering::Release);
                *wake |= passes;
            }
            Source::GfxFault | Source::MmhubFault => {
                feed.fault_events.fetch_add(1, Ordering::Release);
                *wake |= passes;
            }
            Source::PrivReg | Source::PrivInstr => {
                if passes {
                    klog::event!(Warn, "gx", "ih.privileged", source = s.name(), ring_id = iv.ring_id, vmid = iv.vmid; "a program's packet the CP refused; its job's timeout handles it");
                }
                *wake |= passes;
            }
            _ => {
                if passes {
                    if let Some(h) = feed.hook(s) {
                        h(iv);
                    } else if s == Source::Unknown && self.flood[s.index()].in_window == 1 {
                        klog::event!(Debug, "gx", "ih.unrouted", client = iv.client, src = iv.src, ring_id = iv.ring_id);
                    }
                }
            }
        }
    }

    /// `amdgpu_ih_process`, up to `limit` entries.
    fn pass(&mut self, io: &dyn IhIo, feed: &IrqFeed, cfg: &FloodCfg, now: u64, limit: u32) -> Pass {
        let mut out = Pass::default();
        let mut wake = false;
        let mut w = self.wptr(io, feed);
        loop {
            // The entries were written before the pointer that shows them.
            fence(Ordering::Acquire);
            let mut batch = BATCH;
            while self.rptr != w && batch > 0 && out.entries < limit {
                let mut d = [0u32; 8];
                if !io.entry(self.rptr, &mut d) {
                    // Nothing more this pass: the next message or look tries again.
                    feed.bad_reads.fetch_add(1, Ordering::Relaxed);
                    if wake {
                        feed.wake();
                    }
                    return out;
                }
                self.dispatch(&Iv::decode(&d), now, cfg, feed, &mut wake);
                self.rptr = self.rptr.wrapping_add(ih6::ENTRY_BYTES) & self.mask;
                batch -= 1;
                out.entries += 1;
            }
            // Also with nothing read, as Linux: this write is what re-arms the message.
            if !io.set_rptr(self.rptr) {
                feed.bad_reads.fetch_add(1, Ordering::Relaxed);
            }
            // Make sure the write pointer has not moved while the read pointer was on its way.
            w = self.wptr(io, feed);
            if w == self.rptr {
                break;
            }
            if out.entries >= limit {
                out.more = true;
                break;
            }
        }
        if wake {
            feed.wake();
        }
        out
    }

    /// A hold ended with nothing behind it: the next look says so.
    fn end_holds(&mut self, now: u64, feed: &IrqFeed) -> bool {
        let mut any = false;
        for (i, fl) in self.flood.iter_mut().enumerate() {
            if fl.held_until != 0 && now >= fl.held_until {
                feed.flood_held.fetch_add(fl.held, Ordering::Relaxed);
                klog::event!(Info, "gx", "ih.flood_end", source = Source::ALL[i].name(), held = fl.held);
                *fl = Flood::default();
                any = true;
            }
        }
        any
    }
}

/// The running reader of one ring.
pub struct Ih {
    pub feed: Arc<IrqFeed>,
    pub(crate) line: Arc<dyn IrqLine>,
    io: Arc<dyn IhIo>,
    cfg: FloodCfg,
    reader: Mutex<Reader>,
    stop: AtomicBool,
    thread: SpinLock<Option<sched::JoinHandle>>,
}

impl Ih {
    /// The thread runs, the ring is not read yet: the chip's side comes after this (`gpucore::ih::start`), so no message
    /// finds nobody, then `ring_on`.
    pub fn start(feed: Arc<IrqFeed>, io: Arc<dyn IhIo>, line: Arc<dyn IrqLine>, cfg: FloodCfg) -> Result<Arc<Ih>, String> {
        let ih = Arc::new(Ih { feed, line, io, cfg, reader: Mutex::named("amdgpu.ih.reader", Reader::new()), stop: AtomicBool::new(false), thread: SpinLock::named("amdgpu.ih.thread", None) });
        *ih.feed.owner.lock() = Arc::downgrade(&ih);
        ih.spawn()?;
        klog::event!(Info, "gx", "ih.up", line = ih.line.describe().as_str());
        Ok(ih)
    }

    #[cfg(not(test))]
    fn spawn(self: &Arc<Self>) -> Result<(), String> {
        let me = self.clone();
        let t = sched::spawn("amdgpu-ih", sched::Class::Interactive, move || me.thread_main()).map_err(|e| format!("the IH thread: {e:?}"))?;
        *self.thread.lock() = Some(t);
        Ok(())
    }
    /// Host tests have no threads: `IrqFeed::host_pump` does a pass.
    #[cfg(test)]
    fn spawn(self: &Arc<Self>) -> Result<(), String> {
        Ok(())
    }

    #[cfg(not(test))]
    fn thread_main(&self) {
        let mut more = false;
        while !self.stop.load(Ordering::Acquire) {
            let woke = if more { Woke::Message } else { self.line.wait(IDLE_NS) };
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            let p = self.pass_now();
            if woke == Woke::Timeout && p.entries > 0 {
                self.feed.late_looks.fetch_add(1, Ordering::Relaxed);
                klog::event!(Warn, "gx", "ih.late", entries = p.entries; "the ring held entries and no message came");
            }
            more = p.more;
            if more {
                sched::yield_now();
            }
        }
        klog::event!(Debug, "gx", "ih.thread_end");
    }

    /// The chip's side stands: the write-back means something now.
    pub fn ring_on(&self) {
        self.feed.running.store(true, Ordering::Release);
    }

    /// Before the chip's side goes off: its write-back means nothing any more. A pass on its way could still write the
    /// doorbell and IH_RB_CNTL (the overflow's read-modify-write, which would set a ring just switched off going again):
    /// past this wait none can, since `pass_held` looks at `running` once more under the same lock.
    pub fn ring_off(&self) {
        self.feed.running.store(false, Ordering::Release);
        if self.reader.lock_timeout(RING_OFF_WAIT_NS).is_none() {
            klog::event!(Error, "gx", "ih.pass_stuck"; "a pass on the IH ring did not end: the stop goes on without it");
        }
    }

    /// The reader's lock taken, as a pass of the other thread has it.
    pub(crate) fn hold_reader(&self) -> impl Drop + '_ {
        self.reader.lock()
    }

    /// One pass, whatever woke it.
    pub fn pass_now(&self) -> Pass {
        self.pass_at(super::clock_ns())
    }

    /// The same at a given time (the tests' flood).
    pub(crate) fn pass_at(&self, now: u64) -> Pass {
        if !self.feed.running.load(Ordering::Acquire) {
            return Pass::default();
        }
        self.pass_held(self.reader.lock(), now, PASS_ENTRIES)
    }

    /// The job thread's look: never waits behind the IH thread, which does the same work.
    fn look(&self) -> Pass {
        match self.reader.try_lock() {
            Some(r) => self.pass_held(r, super::clock_ns(), LOOK_ENTRIES),
            None => Pass::default(),
        }
    }

    fn pass_held(&self, mut r: MutexGuard<'_, Reader>, now: u64, limit: u32) -> Pass {
        // Under the lock: `ring_off` relies on it.
        if !self.feed.running.load(Ordering::Acquire) {
            return Pass::default();
        }
        let p = r.pass(&*self.io, &self.feed, &self.cfg, now, limit);
        let ended = r.end_holds(now, &self.feed);
        drop(r);
        if ended {
            self.feed.wake();
        }
        p
    }

    /// The thread ends and the line is let go. The caller has switched the chip's side off first (the ring may hold
    /// entries: they are of jobs that have ended by now).
    pub fn stop(&self) {
        self.ring_off();
        self.stop.store(true, Ordering::Release);
        self.line.release();
        let t = self.thread.lock().take();
        if let Some(t) = t
            && !t.join_timeout(5_000_000_000)
        {
            klog::event!(Error, "gx", "ih.thread_stuck"; "the IH thread did not end");
        }
    }
}
