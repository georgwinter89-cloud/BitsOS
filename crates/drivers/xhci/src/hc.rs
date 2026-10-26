//! One xHCI controller: bring-up in V1's order, the command ring with its abort path, event handling, health
//! watch and `Quiesce`. Generic over `Env` (registers, DMA memory, clock, bus master) so the host tests drive the
//! same code against a fake controller model; `driver.rs` supplies the kernel's `Env`.
//!
//! Bring-up order (V1 `xhcipci.asm:77-110`, `xhcireset.asm`, `xhcicaps.asm`; spec `usb.md` §3.1): BIOS handoff
//! (USBLEGSUP, 1 s, then forced, SMIs off) -> halt (RS/INTE/HSEE off, HCH within 20 ms) -> HCRST, **1 ms before
//! any read** (Intel hangs otherwise, Linux quirk) -> HCRST and CNR clear within 1 s -> bus master on -> PAGESIZE
//! 4K required -> MaxSlotsEn -> DCBAA with the scratchpad array in entry 0 -> command ring (CRCR, RCS 1) -> event
//! ring (ERSTSZ, ERDP, **ERSTBA last**: writing it makes the controller read the table) -> interrupter 0 (IMOD,
//! IMAN.IP cleared, IE off until MSI-X) -> port power if PPC -> RS, HCH clear within 20 ms -> HSE must be clear.
//! A controller that fails anywhere is parked: RS off, bus master off, the reason kept and logged.
//! Commands are in `command.rs`, event handling in `events.rs` (further `impl` blocks of `Controller`).

use crate::mem::DmaMem;
use crate::quirks::Quirks;
use crate::regs::{self, cmd, crcr, iman, legacy, portsc, sts, Caps, ExtCaps};
use crate::ring::{EventRing, ProducerRing, RingError};
use crate::slot::SlotEntry;
use alloc::sync::Arc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use ksync::{SpinLock, WaitQueue};

pub const MS: u64 = 1_000_000;
/// Halt and start: the spec allows 16 ms (V1 `XH_HALT_MS`).
pub const HALT_NS: u64 = 20 * MS;
pub const RESET_NS: u64 = 1000 * MS;
/// After writing HCRST nothing is read for 1 ms (Linux `xhci_reset`: some Intel controllers hang otherwise).
pub const HCRST_QUIET_NS: u64 = MS;
pub const BIOS_NS: u64 = 1000 * MS;
/// A command waits at most this long, then the command ring is aborted (V1 `XH_CMD_MS`).
pub const CMD_NS: u64 = 1000 * MS;
pub const ABORT_NS: u64 = 1000 * MS;
/// How long a caller waits for its turn on the command ring (one command at a time, each bounded).
pub const CMD_GATE_NS: u64 = 3 * CMD_NS + ABORT_NS;
/// A waiter drains the event ring itself when the event thread has not answered within this time (early boot,
/// a thread that is not scheduled); normally the event thread does it.
pub const FALLBACK_NS: u64 = 20 * MS;
/// ERDP is advanced inside a long batch every this many events, so the controller never sees the ring full.
pub const ERDP_EVERY: usize = 64;
/// Segment parameters (sizes of rings, not limits on counts of things; R1).
pub const SEGMENT_BYTES: usize = 4096;
pub const CMD_SEGMENTS: usize = 1;
pub const EVENT_SEGMENTS_WANTED: usize = 2;
/// Rings must not cross a 64 KiB boundary (xHCI §6, TRB rings and ERST).
pub const DMA_BOUNDARY: u64 = 0x1_0000;

klog::counter!(FAILURES, "usb.xhci.controller_failures");
klog::counter!(RESTARTS, "usb.xhci.restarts");

/// What the controller needs from its surroundings.
pub trait Env: Send + Sync + 'static {
    type Mem: DmaMem;
    /// A register dword at `off` in BAR0; all ones for a gone device or an offset outside the BAR.
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
    fn bar_len(&self) -> usize;
    /// Zeroed DMA memory that does not cross a 64 KiB boundary and lies below `limit`.
    fn alloc(&self, len: usize, align: u64, limit: u64) -> Option<Self::Mem>;
    /// Transfer data memory below `limit`: page-aligned, any boundary (TRBs are cut at 64 KiB, `queue.rs`).
    fn alloc_data(&self, len: usize, limit: u64) -> Option<Self::Mem>;
    fn now_ns(&self) -> u64;
    /// Busy delay (the 1 ms after HCRST).
    fn delay_ns(&self, ns: u64);
    /// Sleeps the calling thread (port resets, power-good and recovery times).
    fn sleep_ns(&self, ns: u64);
    /// One step of a register poll: yield to other threads when `may_sleep`, else spin.
    fn relax(&self, may_sleep: bool);
    /// Sleeps on `wq` until `cond` or `timeout_ns` (host: advances the fake clock).
    fn wait(&self, wq: &WaitQueue, cond: &mut dyn FnMut() -> bool, timeout_ns: u64);
    /// On: bus master on the function and every bridge above it (PCI `enable_device(true)`). Off: quiesce-safe.
    fn bus_master(&self, on: bool) -> Result<(), String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Down = 0,
    Running = 1,
    /// Stopped by `Quiesce`; stays stopped until `restart`.
    Quiesced = 2,
    /// Parked after an error (reason in `reason()`).
    Failed = 3,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Running => "running",
            State::Quiesced => "quiesced",
            State::Failed => "failed",
        }
    }
    fn from(v: u8) -> State {
        match v {
            1 => State::Running,
            2 => State::Quiesced,
            3 => State::Failed,
            _ => State::Down,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmdCompletion {
    pub code: u8,
    pub slot: u8,
    /// Command Completion Parameter (status bits 23:0).
    pub param: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CmdError {
    NotRunning,
    /// Another command held the ring longer than `CMD_GATE_NS`.
    Busy,
    Ring(RingError),
    /// No answer within `CMD_NS`: the ring was aborted and set up afresh; only this command failed.
    Timeout,
    /// The controller answered with this completion code.
    Failed { code: u8, slot: u8 },
    /// The controller halted, reported HSE or did not stop its ring: it is parked.
    ControllerFailed,
}

/// Where an event batch was handled (the event thread normally; a waiter only as a fallback).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Thread,
    Waiter,
}

#[derive(Default)]
pub struct Stats {
    pub commands: AtomicU64,
    pub timeouts: AtomicU64,
    pub events: AtomicU64,
    pub by_thread: AtomicU64,
    pub by_waiter: AtomicU64,
    pub cmd_completions: AtomicU64,
    pub stray_completions: AtomicU64,
    /// Command Aborted events for the command our own abort stopped.
    pub aborted_completions: AtomicU64,
    pub port_events: AtomicU64,
    pub bad_port_events: AtomicU64,
    pub transfer_events: AtomicU64,
    pub ring_full: AtomicU64,
    pub other_events: AtomicU64,
    /// Short transfers reported as Success with a residual (AMD, `quirks::Quirks::trust_tx_length`).
    pub success_residue: AtomicU64,
    /// US-ev: wake-ups of the completion thread by the event thread, and the time from each wake-up to the
    /// thread's round (ns): what the split costs a chained stick read per stage.
    pub done_wakes: AtomicU64,
    pub done_lag_sum: AtomicU64,
    pub done_lag_max: AtomicU64,
}

pub(crate) struct CmdState<M: DmaMem> {
    pub(crate) ring: ProducerRing<M>,
    pub(crate) pending: Option<u64>,
    pub(crate) done: Option<CmdCompletion>,
    /// Set while `abort` runs: the pending command's Command Aborted event is expected then (and nowhere else).
    pub(crate) aborting: bool,
}

struct BaseMem<M: DmaMem> {
    dcbaa: M,
    _scratch_array: Option<M>,
    _scratch: Vec<M>,
}

pub struct Controller<E: Env> {
    pub(crate) name: String,
    pub(crate) env: E,
    pub(crate) caps: Caps,
    pub(crate) ext: ExtCaps,
    pub(crate) quirks: Quirks,
    pub(crate) state: AtomicU8,
    reason: SpinLock<String>,
    pub(crate) cmd: SpinLock<Option<CmdState<E::Mem>>>,
    pub(crate) ev: SpinLock<Option<EventRing<E::Mem>>>,
    base: SpinLock<Option<BaseMem<E::Mem>>>,
    pub(crate) cmd_busy: AtomicBool,
    pub(crate) cmd_wq: WaitQueue,
    /// The event thread sleeps here; the poller (later the MSI-X handler) wakes it.
    pub ev_wq: WaitQueue,
    pub(crate) thread_alive: AtomicBool,
    restarting: AtomicBool,
    /// Ports with a change event not yet taken by the hub logic (US-2).
    pub(crate) port_changes: SpinLock<Vec<u8>>,
    /// Ports that showed a device when the driver took the controller over (US-2's late-device wait).
    ports_at_takeover: SpinLock<Vec<u8>>,
    /// Quiesce does not log (panic path); it leaves a note here that the next event-thread round logs.
    quiesce_note: AtomicU8,
    pub stats: Stats,
    /// Device slots by ID (`max_slots + 1` entries, the controller's own table size).
    pub(crate) slots: SpinLock<SlotTable<E::Mem>>,
    /// Slots whose Disable Slot was not confirmed while the controller ran: kept, never freed, until a reset.
    pub(crate) leaked: SpinLock<Vec<Arc<SlotEntry<E::Mem>>>>,
    /// Bumped by every bring-up: handles from before a restart are stale.
    epoch: AtomicU32,
    /// The hotplug worker sleeps here; port changes, hub reports and pipe errors wake it.
    pub hotplug_wq: WaitQueue,
    kicked: AtomicBool,
    /// No port waits for a debounce, no change is unhandled (ktests wait for it).
    pub(crate) settled: AtomicBool,
    /// Pipe reports, errors and stops in the order they were made (each pushed under the lock that decided it),
    /// handed on by the event thread (`xfer::Controller::hand_on`).
    pub(crate) reports: crate::xfer::Lane,
    /// Transfer ends (URB completions and their callbacks), handed on by the completion thread or a waiter.
    pub(crate) completions: crate::xfer::Lane,
    /// The completion thread sleeps here; the event thread wakes it when transfer ends are queued.
    pub done_wq: WaitQueue,
    pub(crate) done_alive: AtomicBool,
    /// When the event thread last woke the completion thread (0: its round took it; `Stats::done_lag_*`).
    pub(crate) done_woke: AtomicU64,
    /// When the interrupt not yet answered by a drain was first noticed (0: none; `events::interrupt`).
    pub(crate) irq_at: AtomicU64,
    /// The interrupt time of the latest drain, never going back: the stamp of the pipe events it finds.
    pub(crate) drain_at: AtomicU64,
}

/// Device slots by ID; `None` = free.
pub(crate) type SlotTable<M> = Vec<Option<Arc<SlotEntry<M>>>>;

pub(crate) enum Wait {
    Timeout,
    Gone,
}

impl<E: Env> Controller<E> {
    pub fn new(name: String, env: E, caps: Caps, ext: ExtCaps) -> Self {
        Controller {
            name,
            env,
            caps,
            ext,
            quirks: Quirks::default(),
            state: AtomicU8::new(State::Down as u8),
            reason: SpinLock::named("xhci.reason", String::new()),
            cmd: SpinLock::named("xhci.cmd", None),
            ev: SpinLock::named("xhci.ev", None),
            base: SpinLock::named("xhci.base", None),
            cmd_busy: AtomicBool::new(false),
            cmd_wq: WaitQueue::new(),
            ev_wq: WaitQueue::new(),
            thread_alive: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            port_changes: SpinLock::named("xhci.ports", Vec::new()),
            ports_at_takeover: SpinLock::named("xhci.ports", Vec::new()),
            quiesce_note: AtomicU8::new(0),
            stats: Stats::default(),
            slots: SpinLock::named("xhci.slots", Vec::new()),
            leaked: SpinLock::named("xhci.leaked", Vec::new()),
            epoch: AtomicU32::new(0),
            hotplug_wq: WaitQueue::new(),
            kicked: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            reports: crate::xfer::Lane::new("xhci.deliver"),
            completions: crate::xfer::Lane::new("xhci.complete"),
            done_wq: WaitQueue::new(),
            done_alive: AtomicBool::new(false),
            done_woke: AtomicU64::new(0),
            irq_at: AtomicU64::new(0),
            drain_at: AtomicU64::new(0),
        }
    }

    /// The quirks of this controller's PCI ID (`quirks::for_id`); none by default.
    pub fn with_quirks(mut self, q: Quirks) -> Self {
        self.quirks = q;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn quirks(&self) -> Quirks {
        self.quirks
    }
    pub fn env(&self) -> &E {
        &self.env
    }
    pub fn caps(&self) -> &Caps {
        &self.caps
    }
    pub fn ext(&self) -> &ExtCaps {
        &self.ext
    }
    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub fn reason(&self) -> String {
        self.reason.lock().clone()
    }
    pub fn usbcmd(&self) -> u32 {
        self.rd(self.caps.op(regs::USBCMD))
    }
    pub fn usbsts(&self) -> u32 {
        self.rd(self.caps.op(regs::USBSTS))
    }
    /// (command ring wraps, event ring wraps, event ring segments).
    pub fn ring_info(&self) -> (u64, u64, usize) {
        let c = self.cmd.lock().as_ref().map(|c| c.ring.wraps()).unwrap_or(0);
        let e = self.ev.lock().as_ref().map(|e| (e.wraps(), e.segments())).unwrap_or((0, 0));
        (c, e.0, e.1)
    }
    pub fn take_port_changes(&self) -> Vec<u8> {
        core::mem::take(&mut *self.port_changes.lock())
    }
    pub fn ports_at_takeover(&self) -> Vec<u8> {
        self.ports_at_takeover.lock().clone()
    }
    pub(crate) fn has_port_changes(&self) -> bool {
        !self.port_changes.lock().is_empty()
    }
    pub fn epoch(&self) -> u32 {
        self.epoch.load(Ordering::Acquire)
    }
    /// True when no port waits for its debounce and no change is unhandled.
    pub fn settled(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }
    /// Wakes the hotplug worker.
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        self.hotplug_wq.wake_one();
    }
    pub fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
    pub(crate) fn set_dcbaa(&self, slot: u8, addr: u64) -> bool {
        self.base.lock().as_ref().is_some_and(|b| b.dcbaa.wr64(slot as usize * 8, addr))
    }
    pub(crate) fn dcbaa(&self, slot: u8) -> Option<u64> {
        let b = self.base.lock();
        let m = &b.as_ref()?.dcbaa;
        Some(m.rd32(slot as usize * 8)? as u64 | (m.rd32(slot as usize * 8 + 4)? as u64) << 32)
    }
    /// Called once the event thread is spawned: from then on waiters leave the ring to it (and sleep, so a
    /// cooperative scheduler gets to run it) and drain it themselves only after `FALLBACK_NS`.
    pub fn set_thread_alive(&self) {
        self.thread_alive.store(true, Ordering::Release);
    }
    /// Called once the completion thread is spawned: from then on the event thread leaves transfer ends to it.
    pub fn set_done_alive(&self) {
        self.done_alive.store(true, Ordering::Release);
    }

    pub(crate) fn rd(&self, off: usize) -> u32 {
        self.env.read32(off)
    }
    pub(crate) fn wr(&self, off: usize, v: u32) {
        self.env.write32(off, v)
    }
    /// 64-bit registers are written low dword first, then high (xHCI §5.1; works without 64-bit MMIO).
    pub(crate) fn wr64(&self, off: usize, v: u64) {
        self.wr(off, v as u32);
        self.wr(off + 4, (v >> 32) as u32);
    }
    pub(crate) fn now(&self) -> u64 {
        self.env.now_ns()
    }

    /// Polls `(reg & mask) == want` until `timeout_ns`; one more read after the deadline (V1 `xhci_until`).
    pub(crate) fn poll(&self, off: usize, mask: u32, want: u32, timeout_ns: u64, may_sleep: bool) -> Result<(), Wait> {
        let deadline = self.now().saturating_add(timeout_ns);
        loop {
            let v = self.rd(off);
            if v == u32::MAX {
                return Err(Wait::Gone);
            }
            if v & mask == want {
                return Ok(());
            }
            if self.now() >= deadline {
                let v = self.rd(off);
                return if v != u32::MAX && v & mask == want { Ok(()) } else { Err(Wait::Timeout) };
            }
            self.env.relax(may_sleep);
        }
    }

    /// As `poll`, sleeping `step_ns` between reads (port resets: hundreds of ms).
    pub(crate) fn poll_sleep(&self, off: usize, mask: u32, want: u32, timeout_ns: u64, step_ns: u64) -> Result<(), Wait> {
        let deadline = self.now().saturating_add(timeout_ns);
        loop {
            let v = self.rd(off);
            if v == u32::MAX {
                return Err(Wait::Gone);
            }
            if v & mask == want {
                return Ok(());
            }
            if self.now() >= deadline {
                return Err(Wait::Timeout);
            }
            self.env.sleep_ns(step_ns);
        }
    }

    fn poll_or(&self, off: usize, mask: u32, want: u32, timeout_ns: u64, what: &str) -> Result<(), String> {
        match self.poll(off, mask, want, timeout_ns, true) {
            Ok(()) => Ok(()),
            Err(Wait::Gone) => Err(format!("{what}: registers read all ones (device gone)")),
            Err(Wait::Timeout) => Err(format!("{what}: not within {} ms", timeout_ns / MS)),
        }
    }

    // ---------------------------------------------------------------------------------------------------
    // Bring-up

    /// Takes the controller from whatever state over to Running. Used at probe and by `restart`.
    pub fn start(&self) -> Result<(), String> {
        self.state.store(State::Down as u8, Ordering::Release);
        let t0 = self.now();
        let r = self.bring_up();
        match r {
            Ok(()) => {
                if self.state.compare_exchange(State::Down as u8, State::Running as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
                    // A Quiesce came in while starting: it wins.
                    self.park();
                    return Err(format!("stopped during bring-up (state {})", self.state().as_str()));
                }
                let (_, _, segs) = self.ring_info();
                // New epoch: the hotplug worker drops the old devices and scans the ports.
                self.settled.store(false, Ordering::Release);
                self.kick();
                klog::event!(Info, "usb", "xhci.up", ctrl = self.name.as_str(), version = klog::Hex(self.caps.version as u64),
                    slots = self.caps.max_slots, ports = self.caps.max_ports, context_bytes = self.caps.csz.bytes(),
                    scratchpads = self.caps.scratchpads, event_segments = segs, ac64 = self.caps.ac64,
                    took_us = (self.now().saturating_sub(t0)) / 1000);
                Ok(())
            }
            Err(e) => {
                self.fail(&e);
                if self.state() == State::Quiesced {
                    // Quiesce ran during the bring-up; the step that noticed it may have followed one last write.
                    self.park();
                    klog::event!(Warn, "usb", "xhci.start_stopped", ctrl = self.name.as_str(), reason = e.as_str());
                }
                Err(e)
            }
        }
    }

    /// A Quiesce that arrives during the bring-up wins: checked between the steps, and before and after the two
    /// that switch DMA on (bus master, RS). A Quiesce landing between a check and its write leaves at most that
    /// one write behind, which the check after it catches; `start` then parks the controller again.
    fn still_wanted(&self) -> Result<(), String> {
        match self.state() {
            State::Down => Ok(()),
            s => Err(format!("stopped during bring-up (state {})", s.as_str())),
        }
    }

    fn bring_up(&self) -> Result<(), String> {
        self.record_connected_ports();
        self.bios_handoff();
        self.still_wanted()?;
        let t = self.now();
        self.halt()?;
        let h = self.now();
        klog::event!(Debug, "usb", "xhci.halted", ctrl = self.name.as_str(), took_us = h.saturating_sub(t) / 1000);
        self.still_wanted()?;
        self.reset()?;
        klog::event!(Debug, "usb", "xhci.reset_done", ctrl = self.name.as_str(), took_us = self.now().saturating_sub(h) / 1000);
        // Bus master only after the reset (V1 `xhcipci.asm:117-124`): an old ring must DMA nowhere.
        self.still_wanted()?;
        self.env.bus_master(true).map_err(|e| format!("bus master: {e}"))?;
        self.still_wanted()?;
        self.program()?;
        self.still_wanted()?;
        self.run()?;
        self.still_wanted()
    }

    fn record_connected_ports(&self) {
        let v: Vec<u8> = (1..=self.caps.max_ports).filter(|&p| self.rd(self.caps.portsc(p)) & portsc::CCS != 0).collect();
        if !v.is_empty() {
            klog::event!(Debug, "usb", "xhci.ports_at_takeover", ctrl = self.name.as_str(), ports = format!("{v:?}"));
        }
        *self.ports_at_takeover.lock() = v;
    }

    /// USBLEGSUP: if the BIOS owns the controller, ask for it; a BIOS that does not let go within 1 s loses it
    /// anyway (Linux `quirk_usb_handoff_xhci`). Then the BIOS's SMIs off and their events cleared.
    fn bios_handoff(&self) {
        let Some(off) = self.ext.legacy else { return };
        let v = self.rd(off);
        if v == u32::MAX {
            return;
        }
        if v & legacy::BIOS_OWNED != 0 {
            self.wr(off, v | legacy::OS_OWNED);
            match self.poll(off, legacy::BIOS_OWNED, 0, BIOS_NS, true) {
                Ok(()) => klog::event!(Info, "usb", "xhci.bios_handoff", ctrl = self.name.as_str(), result = "released"),
                Err(_) => {
                    self.wr(off, (self.rd(off) | legacy::OS_OWNED) & !legacy::BIOS_OWNED);
                    klog::event!(Warn, "usb", "xhci.bios_handoff", ctrl = self.name.as_str(), result = "forced";
                        "BIOS kept ownership for 1 s, taken anyway");
                }
            }
        } else {
            klog::event!(Debug, "usb", "xhci.bios_handoff", ctrl = self.name.as_str(), result = "not_bios_owned");
        }
        let c = self.rd(off + 4);
        self.wr(off + 4, (c & legacy::CTL_KEEP) | legacy::CTL_EVENTS);
    }

    fn halt(&self) -> Result<(), String> {
        let c = self.caps.op(regs::USBCMD);
        let v = self.rd(c);
        if v == u32::MAX {
            return Err("halt: registers read all ones (device gone)".into());
        }
        self.wr(c, v & !(cmd::RS | cmd::INTE | cmd::HSEE));
        self.poll_or(self.caps.op(regs::USBSTS), sts::HCH, sts::HCH, HALT_NS, "halt (HCH)")
    }

    fn reset(&self) -> Result<(), String> {
        self.wr(self.caps.op(regs::USBCMD), cmd::HCRST);
        self.env.delay_ns(HCRST_QUIET_NS);
        let start = self.now();
        self.poll_or(self.caps.op(regs::USBCMD), cmd::HCRST, 0, RESET_NS, "reset (HCRST)")?;
        let left = RESET_NS.saturating_sub(self.now().saturating_sub(start));
        self.poll_or(self.caps.op(regs::USBSTS), sts::CNR, 0, left, "reset (CNR)")
    }

    fn limit(&self) -> u64 {
        if self.caps.ac64 { u64::MAX } else { 1 << 32 }
    }

    fn alloc(&self, len: usize, align: u64, what: &str) -> Result<E::Mem, String> {
        self.env.alloc(len, align, self.limit()).ok_or_else(|| format!("no DMA memory for {what} ({len} bytes)"))
    }

    /// Everything between the reset and RS: slots, DCBAA, scratchpads, command ring, event ring, interrupter 0,
    /// port power. The new memory replaces the old only now, after HCRST made the controller forget the old.
    fn program(&self) -> Result<(), String> {
        let op = |r| self.caps.op(r);
        if self.rd(op(regs::PAGESIZE)) & 1 == 0 {
            return Err(format!("PAGESIZE {:#x} has no 4 KiB pages", self.rd(op(regs::PAGESIZE))));
        }
        let cfg = self.rd(op(regs::CONFIG));
        self.wr(op(regs::CONFIG), (cfg & !0xFF) | self.caps.max_slots as u32);

        // HCRST made the controller forget every slot: their memory goes, handles from before go stale.
        let old = core::mem::replace(&mut *self.slots.lock(), (0..=self.caps.max_slots).map(|_| None).collect());
        for e in old.iter().flatten() {
            e.gone.store(true, Ordering::Release);
            let mut x = e.xfer.lock();
            let mut ended = Vec::new();
            x.stop_all(usb_core::UsbError::NotRunning, &mut ended);
            self.defer(ended);
            drop(x);
            e.wq.wake_all();
        }
        drop(old);
        self.deliver_pending();
        self.leaked.lock().clear();
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let dcbaa = self.alloc((self.caps.max_slots as usize + 1) * 8, 64, "DCBAA")?;
        let n = self.caps.scratchpads as usize;
        let (array, pages) = if n > 0 {
            let array = self.alloc(n * 8, 64, "scratchpad array")?;
            let mut pages = Vec::with_capacity(n);
            for i in 0..n {
                let p = self.alloc(4096, 4096, "scratchpad page")?;
                if !array.wr64(i * 8, p.dev_addr()) {
                    return Err("scratchpad array write".into());
                }
                pages.push(p);
            }
            if !dcbaa.wr64(0, array.dev_addr()) {
                return Err("DCBAA write".into());
            }
            (Some(array), pages)
        } else {
            (None, Vec::new())
        };
        self.wr64(op(regs::DCBAAP), dcbaa.dev_addr());

        let mut segs = Vec::with_capacity(CMD_SEGMENTS);
        for _ in 0..CMD_SEGMENTS {
            segs.push(self.alloc(SEGMENT_BYTES, 64, "command ring")?);
        }
        let ring = ProducerRing::new(segs).map_err(|e| format!("command ring: {e:?}"))?;
        self.wr64(op(regs::CRCR), ring.base() | crcr::RCS as u64);

        let nseg = self.caps.event_segments(EVENT_SEGMENTS_WANTED);
        let mut esegs = Vec::with_capacity(nseg);
        for _ in 0..nseg {
            esegs.push(self.alloc(SEGMENT_BYTES, 64, "event ring")?);
        }
        let erst = self.alloc(nseg * 16, 64, "ERST")?;
        let er = EventRing::new(esegs, erst).map_err(|e| format!("event ring: {e:?}"))?;
        let ir = |r| self.caps.ir(0, r);
        let sz = self.rd(ir(regs::ERSTSZ));
        self.wr(ir(regs::ERSTSZ), (sz & !0xFFFF) | nseg as u32);
        self.wr64(ir(regs::ERDP), er.dequeue_ptr());
        // Last: writing ERSTBA makes the controller read the segment table.
        self.wr64(ir(regs::ERSTBA), er.erst_base());
        // Interrupter 0 prepared for MSI-X (PC-3): moderation set, pending cleared, IE still off (polled).
        self.wr(ir(regs::IMOD), regs::IMOD_DEFAULT);
        self.wr(ir(regs::IMAN), iman::IP);

        if self.caps.ppc {
            for p in 1..=self.caps.max_ports {
                let v = self.rd(self.caps.portsc(p));
                self.wr(self.caps.portsc(p), (v & portsc::KEEP) | portsc::PP);
            }
        }
        *self.cmd.lock() = Some(CmdState { ring, pending: None, done: None, aborting: false });
        *self.ev.lock() = Some(er);
        *self.base.lock() = Some(BaseMem { dcbaa, _scratch_array: array, _scratch: pages });
        Ok(())
    }

    fn run(&self) -> Result<(), String> {
        let c = self.caps.op(regs::USBCMD);
        let v = self.rd(c);
        self.wr(c, (v & !(cmd::INTE | cmd::HSEE | cmd::HCRST)) | cmd::RS);
        self.poll_or(self.caps.op(regs::USBSTS), sts::HCH, 0, HALT_NS, "run (HCH clear)")?;
        let s = self.usbsts();
        if s & sts::HSE != 0 {
            return Err("HSE right after start: DMA does not reach memory (bridge bus master?)".into());
        }
        Ok(())
    }

    /// RS off without waiting, bus master off. Lock-free (also used on the failure paths).
    fn park(&self) {
        let c = self.caps.op(regs::USBCMD);
        let v = self.rd(c);
        if v != u32::MAX {
            self.wr(c, v & !(cmd::RS | cmd::INTE | cmd::HSEE));
        }
        let _ = self.env.bus_master(false);
    }

    /// Parks the controller and records why (any state but Quiesced, which stays).
    pub(crate) fn fail(&self, why: &str) {
        let s = self.state.load(Ordering::Acquire);
        if s == State::Quiesced as u8 || s == State::Failed as u8 {
            return;
        }
        if self.state.compare_exchange(s, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let st = self.usbsts();
        self.park();
        self.settled.store(false, Ordering::Release);
        FAILURES.inc();
        *self.reason.lock() = String::from(why);
        klog::event!(Error, "usb", "xhci.failed", ctrl = self.name.as_str(), usbsts = klog::Hex(st as u64),
            hch = st & sts::HCH != 0 && st != u32::MAX, hse = st & sts::HSE != 0 && st != u32::MAX, reason = why);
        self.cmd_wq.wake_all();
        self.kick();
    }

    /// Checks USBSTS of a running controller: gone, HSE, HCE or halted means it is parked.
    pub fn check_health(&self) {
        if self.state() != State::Running {
            return;
        }
        let s = self.usbsts();
        let why = if s == u32::MAX {
            "registers read all ones (device gone)"
        } else if s & sts::HSE != 0 {
            "host system error (HSE): DMA does not reach memory (bridge bus master?)"
        } else if s & sts::HCE != 0 {
            "host controller error (HCE)"
        } else if s & sts::HCH != 0 {
            "halted by itself (HCH)"
        } else {
            return;
        };
        self.fail(why);
    }

    // ---------------------------------------------------------------------------------------------------
    // Quiesce and restart

    /// Stops DMA: RS off, wait for HCH (spinning, bounded), bus master off. Lock-free and without klog (the panic
    /// path may hold klog's locks); a note is left for the event thread. The controller stays stopped until
    /// `restart`.
    pub fn quiesce(&self) {
        let was = self.state.swap(State::Quiesced as u8, Ordering::AcqRel);
        // The worker drops the devices at its next round (no wake-up here: panic path).
        self.settled.store(false, Ordering::Release);
        let c = self.caps.op(regs::USBCMD);
        let v = self.rd(c);
        let mut halted = false;
        if v != u32::MAX {
            self.wr(c, v & !(cmd::RS | cmd::INTE | cmd::HSEE));
            halted = self.poll(self.caps.op(regs::USBSTS), sts::HCH, sts::HCH, HALT_NS, false).is_ok();
        }
        let _ = self.env.bus_master(false);
        self.quiesce_note.store(0x80 | if halted { 0x40 } else { 0 } | (was & 0x3F), Ordering::Release);
        // No wake-up here: waking takes the scheduler's lock, which the panicking CPU may hold. A command
        // waiter sleeps in slices of at most `FALLBACK_NS` and then sees the state.
    }

    pub fn log_quiesce_note(&self) {
        let n = self.quiesce_note.swap(0, Ordering::AcqRel);
        if n & 0x80 != 0 {
            klog::event!(Info, "usb", "xhci.quiesce", ctrl = self.name.as_str(), was = State::from(n & 0x3F).as_str(),
                halted = n & 0x40 != 0);
        }
    }

    /// Brings a quiesced or failed controller back (ktests, Reload's undo path). One restart at a time.
    pub fn restart(&self, why: &str) -> Result<(), String> {
        if self.restarting.swap(true, Ordering::AcqRel) {
            return Err("a restart of this controller is already running".into());
        }
        self.log_quiesce_note();
        RESTARTS.inc();
        klog::event!(Warn, "usb", "xhci.restart", ctrl = self.name.as_str(), reason = why, was = self.state().as_str());
        let r = self.start();
        self.restarting.store(false, Ordering::Release);
        r
    }
}

impl<E: Env> devmodel::Quiesce for Controller<E> {
    fn name(&self) -> &str {
        &self.name
    }
    fn quiesce(&self) {
        Controller::quiesce(self)
    }
}
