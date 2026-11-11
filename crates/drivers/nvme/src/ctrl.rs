//! One controller: bring-up (linux-7/drivers/nvme/host/pci.c `nvme_pci_configure_admin_queue`, core.c
//! `nvme_disable_ctrl`, `nvme_enable_ctrl`, pci.c:2960 `nvme_setup_io_queues`, :2426 `nvme_create_io_queues`), the
//! admin queue, the I/O queues (`mq`, BL-1: one per CPU or per vector, many commands in flight; the I/O path is in
//! `io`), recovery after a timeout or a fatal status, and Quiesce. Media-changing commands only inside the
//! armed window (R8a, NV-2p).
//!
//! Locks: `nvme.admin` (Mutex, an admin command is placed and waited for under it) > `nvme.info`/`nvme.window`
//! (SpinLock, leaves); `nvme.io` (RwSpinLock: the current queue set, held only to clone it, never with another
//! nvme lock). An I/O queue has `nvme.sq` (slot copy + doorbell) and `nvme.cq` (IrqSpinLock, one pass over the ring),
//! the interrupt hook `nvme.hooks` > `nvme.cq`; no lock is held across a device wait. Quiesce takes no lock.

use crate::cmd::{self, AdminCmd, Cqe, Encode, WriteWindow};
use crate::hw::{DmaMem, Hw, IrqMode};
use crate::ident::{self, CtrlId, NsId, Smart};
use crate::hw::IrqHook;
use crate::mq::{HookTable, IoQueue, IoSet};
use crate::prp;
use crate::queue::QueuePair;
use crate::regs::{self, Cap};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use ksync::{Mutex, RwSpinLock, SpinLock};

pub(crate) const MS: u64 = 1_000_000;
pub(crate) const SEC: u64 = 1_000_000_000;
/// Longest single wait on a vector before the controller status is looked at again (a lost interrupt costs
/// at most this).
pub(crate) const SLICE_NS: u64 = MS;
/// Quiesce bounds (interrupts off, other CPUs parked: bounded spins). Linux waits `shutdown_timeout` (5 s
/// default, core.c:54) for the shutdown and (CAP.TO + 1) / 2 s for RDY (core.c:2219); the same here, the
/// latter at most 10 s.
pub(crate) const QUIESCE_SHUTDOWN_NS: u64 = 5 * SEC;
pub(crate) const QUIESCE_RDY_MAX_NS: u64 = 10 * SEC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Down = 0,
    Starting = 1,
    Ready = 2,
    Failed = 3,
    Quiesced = 4,
    Removed = 5,
}

impl State {
    fn from(v: u8) -> State {
        match v {
            1 => State::Starting,
            2 => State::Ready,
            3 => State::Failed,
            4 => State::Quiesced,
            5 => State::Removed,
            _ => State::Down,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::Down => "down",
            State::Starting => "starting",
            State::Ready => "ready",
            State::Failed => "failed",
            State::Quiesced => "quiesced",
            State::Removed => "removed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmdError {
    /// The controller answered with an error status.
    Status { sct: u8, sc: u8, dnr: bool },
    Timeout,
    /// All ones from the registers, or the controller was quiesced or removed.
    Gone,
    /// CSTS.CFS: the controller hit a fatal error.
    Fatal,
    /// Not ready (failed bring-up), queue full or DMA memory problem.
    NotReady,
    /// R8a, NV-2p: a media-changing request outside the armed window, or with none armed. No command was built.
    OutsideWindow,
    /// The request itself is invalid (outside the namespace, wrong length).
    Invalid,
}

impl CmdError {
    pub fn text(&self) -> String {
        match *self {
            CmdError::Status { sct, sc, dnr } => format!("status {sct:x}/{sc:02x} ({}){}", cmd::status_text(sct, sc), if dnr { " dnr" } else { "" }),
            CmdError::Timeout => String::from("timeout"),
            CmdError::Gone => String::from("gone"),
            CmdError::Fatal => String::from("controller fatal status"),
            CmdError::NotReady => String::from("not ready"),
            CmdError::Invalid => String::from("invalid request"),
            CmdError::OutsideWindow => String::from("outside the write window"),
        }
    }
    pub fn to_dev(self) -> devmodel::DevError {
        use devmodel::DevError;
        match self {
            CmdError::Status { sct, sc, .. } if cmd::status_is_invalid(sct, sc) => DevError::InvalidArgument,
            CmdError::Status { .. } | CmdError::Fatal => DevError::Io,
            CmdError::Timeout => DevError::Timeout,
            CmdError::Gone | CmdError::NotReady => DevError::Gone,
            CmdError::Invalid => DevError::InvalidArgument,
            CmdError::OutsideWindow => DevError::Refused,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Use MSI-X/MSI (CONFIG `nvme_irq=off`: polled).
    pub irqs: bool,
    /// Linux's defaults: admin 60 s, I/O 30 s (core.c `admin_timeout`, `io_timeout`).
    pub admin_timeout_ns: u64,
    pub io_timeout_ns: u64,
    /// Spin on the completion queue this long before waiting on the vector (polled first).
    pub spin_ns: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config { irqs: true, admin_timeout_ns: 60 * SEC, io_timeout_ns: 30 * SEC, spin_ns: 10_000 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Namespace {
    pub nsid: u32,
    pub id: NsId,
    /// Why the block layer does not get it (None: it is registered).
    pub unusable: Option<String>,
}

impl Namespace {
    pub fn block_size(&self) -> u32 {
        self.id.block_size()
    }
}

/// What the bring-up learned (MCP, ktests, the namespace devices).
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub cap: Cap,
    pub version: u32,
    pub ident: Option<CtrlId>,
    pub namespaces: Vec<Namespace>,
    pub irq: Option<IrqMode>,
    /// Entries per I/O queue (each has one tag fewer: commands in flight at most).
    pub io_depth: u16,
    /// Bytes per Read command (MDTS and one PRP list page bound it).
    pub max_transfer: usize,
    /// The first I/O queue's vector index.
    pub io_vector: Option<u16>,
    /// BL-1: the I/O queues of this start.
    pub queues: Vec<QueueInfo>,
    pub starts: u32,
    pub last_error: Option<String>,
}

/// One I/O queue as the bring-up made it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueInfo {
    pub qid: u16,
    pub vector: Option<u16>,
    /// The CPU its vector was given.
    pub cpu: Option<u32>,
}

pub struct Stats {
    /// Commands placed in the admin / I/O queue, by the opcode byte actually written to the slot.
    pub admin: [AtomicU64; 256],
    pub io: [AtomicU64; 256],
    pub errors: AtomicU64,
    pub timeouts: AtomicU64,
    pub resets: AtomicU64,
    /// Completions that matched no waiting command (late ones after a timeout, hostile CIDs); shared with the
    /// interrupt hook.
    pub stale: Arc<AtomicU64>,
    /// BL-1: Abort commands sent for timed-out I/O commands (Linux `nvme_timeout`).
    pub aborts: AtomicU64,
    pub bytes_read: AtomicU64,
    pub bytes_written: AtomicU64,
    /// Write or deallocate requests the controller refused because they left the window (NV-2p).
    pub window_refused: AtomicU64,
    /// BL-0: I/O commands placed and not yet completed now, and the most at once (the I/O queue's depth in use).
    pub inflight: AtomicU64,
    pub inflight_max: AtomicU64,
}

// BL-0: the bounce buffer's copy into the caller's buffer (per CPU), and the deepest I/O queue of any controller.
vfs::pcpu_counter!(pub(crate) COPY_NVME_BOUNCE, "block.copy_bytes.nvme_bounce", vfs::pcpu::BYTES_BATCH);
// BL-1: bytes the device moved straight to or from the caller's memory (no bounce, no copy).
vfs::pcpu_counter!(pub(crate) NVME_DIRECT, "nvme.direct_bytes", vfs::pcpu::BYTES_BATCH);
klog::counter!(INFLIGHT_MAX, "nvme.inflight_max");
static INFLIGHT_MAX_SEEN: AtomicU64 = AtomicU64::new(0);

impl Stats {
    /// BL-0: an I/O command was placed (`io_cmd`).
    pub(crate) fn placed(&self) {
        let now = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        if now > self.inflight_max.load(Ordering::Relaxed) {
            self.inflight_max.fetch_max(now, Ordering::Relaxed);
            let seen = INFLIGHT_MAX_SEEN.fetch_max(now, Ordering::Relaxed);
            if now > seen {
                INFLIGHT_MAX.add(now - seen);
            }
        }
    }
    /// BL-0: the command's wait ended (completed, failed or abandoned).
    pub(crate) fn ended(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }
    fn new() -> Stats {
        Stats {
            inflight: AtomicU64::new(0),
            inflight_max: AtomicU64::new(0),
            admin: [const { AtomicU64::new(0) }; 256],
            io: [const { AtomicU64::new(0) }; 256],
            errors: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            stale: Arc::new(AtomicU64::new(0)),
            aborts: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            window_refused: AtomicU64::new(0),
        }
    }
    /// Media-changing commands ever placed in a queue (R8a: 0 unless a window was armed, NV-2p).
    pub fn media_writes(&self) -> u64 {
        let io: u64 = cmd::MEDIA_WRITE_IO_OPS.iter().map(|&o| self.io[o as usize].load(Ordering::Relaxed)).sum();
        let ad: u64 = cmd::MEDIA_WRITE_ADMIN_OPS.iter().map(|&o| self.admin[o as usize].load(Ordering::Relaxed)).sum();
        io + ad
    }
    /// (opcode, count) of every opcode used, admin then I/O.
    pub fn used(&self) -> (Vec<(u8, u64)>, Vec<(u8, u64)>) {
        let f = |a: &[AtomicU64; 256]| (0..256).filter_map(|i| Some((i as u8, a[i].load(Ordering::Relaxed))).filter(|x| x.1 != 0)).collect();
        (f(&self.admin), f(&self.io))
    }
}

pub(crate) struct AdminState<M: DmaMem> {
    q: QueuePair<M, AdminCmd>,
    /// One page for Identify and log pages.
    page: M,
    vector: Option<u16>,
}

pub struct Controller<H: Hw> {
    pub name: String,
    pub hw: H,
    pub(crate) cfg: Config,
    state: AtomicU8,
    /// CAP and RTD3E for Quiesce (no lock there).
    cap_raw: AtomicU64,
    stride: AtomicU32,
    /// Changes whenever a restart found other namespaces (the namespace devices compare it).
    layout: AtomicU64,
    quiesce_note: AtomicU32,
    /// A start or a recovery is running (they never overlap; the PCI calls of a start run with no lock held).
    pub(crate) starting: AtomicBool,
    admin: Mutex<Option<AdminState<H::Mem>>>,
    /// BL-1: the I/O queues of this start (None while down); a recovery or restart replaces the set.
    pub(crate) io: RwSpinLock<Option<Arc<IoSet<H::Mem>>>>,
    /// What the vectors' interrupt hook reaps.
    pub(crate) hooks: Arc<HookTable<H::Mem, H::Bell>>,
    /// Counts each stop of the device's DMA (a reset that reached RDY = 0, bus master off, a Quiesce). A command
    /// given up returns its pages, and its caller's memory, only once this moved past its value at placing.
    pub(crate) stop_gen: AtomicU64,
    pub(crate) info: SpinLock<Info>,
    /// NV-2p: the only blocks Write and Dataset Management may name; None: nothing is ever written.
    window: SpinLock<Option<WriteWindow>>,
    pub stats: Stats,
}

impl<H: Hw> Controller<H> {
    pub fn new(name: String, hw: H, cfg: Config) -> Controller<H> {
        let stats = Stats::new();
        let hooks = Arc::new(HookTable { set: ksync::IrqSpinLock::named("nvme.hooks", None), bell: hw.bell(), stale: stats.stale.clone() });
        Controller {
            name,
            hw,
            cfg,
            state: AtomicU8::new(State::Down as u8),
            cap_raw: AtomicU64::new(0),
            stride: AtomicU32::new(4),
            layout: AtomicU64::new(0),
            quiesce_note: AtomicU32::new(0),
            starting: AtomicBool::new(false),
            admin: Mutex::named("nvme.admin", None),
            io: RwSpinLock::named("nvme.io", None),
            hooks,
            stop_gen: AtomicU64::new(0),
            info: SpinLock::named("nvme.info", Info::default()),
            window: SpinLock::named("nvme.window", None),
            stats,
        }
    }

    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub fn info(&self) -> Info {
        self.info.lock().clone()
    }
    pub fn layout(&self) -> u64 {
        self.layout.load(Ordering::Acquire)
    }
    /// What the last Quiesce did: 0 = none; bit 31 set, bit 0 shutdown completed, bit 1 RDY reached 0, bits
    /// 8..15 the state before.
    pub fn quiesce_note(&self) -> u32 {
        self.quiesce_note.load(Ordering::Acquire)
    }
    pub(crate) fn cap_raw(&self) -> u64 {
        self.cap_raw.load(Ordering::Acquire)
    }
    fn stride(&self) -> usize {
        self.stride.load(Ordering::Relaxed) as usize
    }
    pub(crate) fn gone(&self) -> bool {
        matches!(self.state(), State::Quiesced | State::Removed)
    }

    /// Brings the controller up (first start and restarts): reset, admin queue, enable, Identify, the I/O queue
    /// pair, namespaces. Sleeps; call from a thread.
    pub fn start(&self) -> Result<(), String> {
        if self.state() == State::Removed {
            return Err(String::from("removed"));
        }
        if self.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(String::from("a start or recovery is already running"));
        }
        let r = self.start_unlocked();
        self.starting.store(false, Ordering::Release);
        r
    }

    /// Any state but Removed -> Starting (a restart from Quiesced or Failed included). From here on a Quiesce or
    /// remove shows as a state other than Starting, and the start stops and undoes what it turned on.
    fn enter_starting(&self) -> Result<(), String> {
        let mut cur = self.state.load(Ordering::Acquire);
        loop {
            if cur == State::Removed as u8 {
                return Err(String::from("removed"));
            }
            match self.state.compare_exchange(cur, State::Starting as u8, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(()),
                Err(c) => cur = c,
            }
        }
    }
    fn still_starting(&self) -> Result<(), String> {
        match self.state() {
            State::Starting => Ok(()),
            s => Err(format!("{} during the start", s.name())),
        }
    }

    fn start_unlocked(&self) -> Result<(), String> {
        self.enter_starting()?;
        let note = self.quiesce_note.swap(0, Ordering::AcqRel);
        if note != 0 {
            // Quiesce cannot log (interrupts off, no locks): its note is told here.
            klog::event!(Info, "nvme", "ctrl.quiesced_before", ctrl = self.name.as_str(), shutdown_done = note & 1 != 0, rdy_zero = note & 2 != 0,
                state_before = State::from((note >> 8) as u8).name());
        }
        // The PCI side first, with no driver lock held: `pci.enable` is outer to every subsystem lock (ksync
        // order.rs). Old vectors go first (their device side is off already). Memory decode alone, the
        // controller off, and only then bus master: a controller the firmware or the previous kernel left
        // enabled must not DMA into memory that is ours now (its queues lay in memory it no longer owns).
        self.hw.free_irqs();
        let mode = self
            .hw
            .enable(false)
            .and_then(|()| self.disable_ctrl())
            .and_then(|_| self.still_starting())
            .and_then(|()| self.hw.enable(true))
            .map(|()| if self.cfg.irqs { self.setup_irqs() } else { IrqMode::Polled });
        self.start_locked(Some(mode))
    }

    /// BL-1: one vector per CPU for the I/O queues plus the admin one (Linux `nvme_setup_irqs`: the queue count
    /// plus one pre-vector); the PCI core gives fewer when the controller or the CPUs have fewer, and the queues
    /// follow (`bring_up`). Each runs the hook that reaps its queues.
    fn setup_irqs(&self) -> IrqMode {
        let want = (self.hw.cpus().max(1) + 1).min(u16::MAX as usize) as u16;
        let t = self.hooks.clone();
        let hook: IrqHook = Arc::new(move |i| t.irq(i));
        let m = self.hw.setup_irqs(want, hook);
        if m.vectors() < want {
            klog::event!(Info, "nvme", "irq.fewer", ctrl = self.name.as_str(), wanted = want, got = m.vectors(), mode = m.name();
                "fewer vectors than CPUs + 1: the I/O queues share or poll");
        }
        m
    }

    fn bump_stop(&self) {
        self.stop_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// CAP read and checked, CC.EN cleared (no shutdown notification, pci.c:1730), RDY = 0 awaited.
    fn disable_ctrl(&self) -> Result<Cap, String> {
        let cap = Cap { raw: self.read64(regs::CAP) };
        if cap.raw == u64::MAX {
            return Err(String::from("CAP reads all ones: no device"));
        }
        if !cap.css_nvm() {
            return Err(format!("CAP {:#x}: no NVM command set", cap.raw));
        }
        if cap.mpsmin_shift() > 12 {
            return Err(format!("minimum page size 2^{} above the host's 4 KiB", cap.mpsmin_shift()));
        }
        self.cap_raw.store(cap.raw, Ordering::Release);
        let cc = self.hw.rd32(regs::CC);
        let csts = self.hw.rd32(regs::CSTS);
        // What the firmware, the previous kernel's Quiesce or a timed-out command left (once per start or recovery).
        klog::event!(Info, "nvme", "ctrl.found", ctrl = self.name.as_str(), cc = klog::Hex(cc as u64), csts = klog::Hex(csts as u64),
            enabled = cc & regs::CC_EN != 0, shutdown_done = csts & regs::CSTS_SHST_MASK == regs::CSTS_SHST_CMPLT);
        if cc & regs::CC_EN != 0 {
            self.hw.wr32(regs::CC, cc & !(regs::CC_EN | regs::CC_SHN_MASK));
        }
        self.wait_csts(regs::CSTS_RDY, 0, cap.ready_timeout_ns(), true)
            .map_err(|c| format!("RDY stayed 1 after disable (CSTS {c:#x}, {} ms)", cap.ready_timeout_ns() / MS))?;
        // The controller is reset: every command it had is gone, it reaches no memory of the old queues.
        self.bump_stop();
        Ok(cap)
    }

    /// `mode`: the PCI side's result for a start; None for a recovery, which resets only the controller and
    /// keeps the vectors.
    /// A start: the state is Starting already (`start_unlocked`). A recovery: Ready -> Starting here; any other
    /// state (a Quiesce or remove came first) refuses it untouched.
    pub(crate) fn start_locked(&self, mode: Option<Result<IrqMode, String>>) -> Result<(), String> {
        if mode.is_none() && self.state.compare_exchange(State::Ready as u8, State::Starting as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(format!("controller {}", self.state().name()));
        }
        // The old queues take no command any more; their waiters retry on the new ones once the device stopped.
        let old = self.take_set();
        let snap = self.stop_gen.load(Ordering::Acquire);
        let r = match mode {
            Some(Err(e)) => Err(e),
            Some(Ok(m)) => self.still_starting().and_then(|()| self.bring_up(m)),
            None => {
                let m = self.info.lock().irq.unwrap_or(IrqMode::Polled);
                self.bring_up(m)
            }
        };
        // The old queue memory and the pages of its commands go back only with the device stopped since.
        if old.is_some() && self.stop_gen.load(Ordering::Acquire) == snap {
            self.stop_device();
        }
        drop(old);
        let end = if r.is_ok() { State::Ready } else { State::Failed };
        // A Quiesce or remove at any point of the start wins: its state stays, and what the start turned on
        // after the Quiesce stopped the controller (bus master, MSI-X, CC.EN) goes off again here. A Quiesce
        // after this exchange stops everything itself.
        let r = match self.state.compare_exchange(State::Starting as u8, end as u8, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => r,
            Err(now) => {
                self.undo_start(State::from(now));
                Err(format!("{} during the start", State::from(now).name()))
            }
        };
        let mut info = self.info.lock();
        info.starts += 1;
        match &r {
            Ok(()) => info.last_error = None,
            Err(e) => {
                info.last_error = Some(e.clone());
                drop(info);
                klog::event!(Error, "nvme", "ctrl.failed", ctrl = self.name.as_str(), error = e.as_str());
            }
        }
        r
    }

    /// The controller off again after a start that a Quiesce or remove overtook (the thread may sleep here).
    fn undo_start(&self, now: State) {
        let enabled = self.stop_device();
        klog::event!(Warn, "nvme", "ctrl.start_aborted", ctrl = self.name.as_str(), state = now.name(), was_enabled = enabled;
            "a Quiesce or remove came during the start: controller and bus master off again");
    }

    /// CC.EN off, RDY = 0 awaited (bounded), bus master and MSI-X off: nothing of the controller's reaches memory any
    /// more. Returns whether it was enabled. The thread may sleep here.
    pub(crate) fn stop_device(&self) -> bool {
        let cc = self.hw.rd32(regs::CC);
        let enabled = cc != !0 && cc & regs::CC_EN != 0;
        if enabled {
            self.hw.wr32(regs::CC, cc & !(regs::CC_EN | regs::CC_SHN_MASK));
            let cap = Cap { raw: self.cap_raw.load(Ordering::Acquire) };
            let _ = self.wait_csts(regs::CSTS_RDY, 0, cap.ready_timeout_ns().min(QUIESCE_RDY_MAX_NS), true);
        }
        self.hw.pci_off();
        self.bump_stop();
        enabled
    }

    /// The current queue set out of reach (submitters, the interrupt hook), and dead.
    fn take_set(&self) -> Option<Arc<IoSet<H::Mem>>> {
        let old = self.io.write().take();
        self.hooks.publish(None);
        if let Some(o) = &old {
            o.kill();
        }
        old
    }

    /// A command is given up with no reset behind it: it may still run on the controller, and it may fetch its PRP
    /// list or range late, from pages the next command would write. So no next command: the device stops and stays
    /// Failed until a real restart (Linux: a second timeout disables the device, pci.c `nvme_timeout`). A state other
    /// than Ready means someone else has it: a Quiesce or remove stopped it, a start resets it.
    pub(crate) fn abandon(&self, e: &CmdError) {
        if self.state.compare_exchange(State::Ready as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let old = self.take_set();
        let was_enabled = self.stop_device();
        drop(old);
        let why = format!("a command was given up: {}", e.text());
        klog::event!(Error, "nvme", "ctrl.abandoned", ctrl = self.name.as_str(), error = why.as_str(), was_enabled = was_enabled; "controller stopped, it stays down until a restart");
        self.info.lock().last_error = Some(why);
    }

    fn wait_csts(&self, mask: u32, want: u32, timeout_ns: u64, may_sleep: bool) -> Result<(), u32> {
        let end = self.hw.now_ns().saturating_add(timeout_ns);
        loop {
            let c = self.hw.rd32(regs::CSTS);
            if c == !0 {
                return Err(c);
            }
            if c & mask == want {
                return Ok(());
            }
            if mask & regs::CSTS_CFS != 0 && c & regs::CSTS_CFS != 0 {
                return Err(c);
            }
            if self.hw.now_ns() >= end {
                return Err(c);
            }
            // Linux: usleep_range(1000, 2000) (core.c:2183).
            self.hw.pause_us(if may_sleep { 1000 } else { 10 }, may_sleep);
        }
    }

    fn bring_up(&self, mode: IrqMode) -> Result<(), String> {
        let cap = self.disable_ctrl()?;
        let version = self.hw.rd32(regs::VS);
        // Linux nvme_remap_bar (pci.c): the doorbells at this stride must lie in BAR0, else every doorbell
        // write would be dropped and each command time out with no word on why.
        let need = regs::doorbell(1, true, cap.stride()) + 4;
        if need > self.hw.window() {
            let e = format!("doorbell stride {} needs {need:#x} bytes of BAR0, it has {:#x}", cap.stride(), self.hw.window());
            klog::event!(Error, "nvme", "ctrl.bar_too_small", ctrl = self.name.as_str(), stride = cap.stride(), need = need, bar = self.hw.window());
            return Err(e);
        }
        self.stride.store(cap.stride() as u32, Ordering::Relaxed);
        let restart = self.info.lock().starts > 0;
        klog::event!(Info, "nvme", "ctrl.reset", ctrl = self.name.as_str(), cap = klog::Hex(cap.raw), version = regs::version_text(version),
            timeout_ms = cap.ready_timeout_ns() / MS, restart = restart);
        // Admin queue.
        let mut adm = self.admin.lock();
        if adm.is_none() {
            let d = regs::ADMIN_DEPTH;
            let q = QueuePair::new(0, d, self.dma(d as usize * regs::SQE)?, self.dma(d as usize * regs::CQE)?)
                .ok_or_else(|| String::from("admin queue memory"))?;
            *adm = Some(AdminState { q, page: self.dma(regs::PAGE)?, vector: None });
        }
        let Some(a) = adm.as_mut() else { return Err(String::from("admin queue missing")) };
        if !a.q.reset() {
            return Err(String::from("admin queue memory"));
        }
        let aqa = (a.q.depth as u32 - 1) | ((a.q.depth as u32 - 1) << 16);
        self.hw.wr32(regs::AQA, aqa);
        self.write64(regs::ASQ, a.q.sq.dev_addr());
        self.write64(regs::ACQ, a.q.cq.dev_addr());
        let cfg = regs::cc_config();
        self.hw.wr32(regs::CC, cfg);
        let _ = self.hw.rd32(regs::CC);
        self.still_starting()?;
        self.hw.wr32(regs::CC, cfg | regs::CC_EN);
        self.wait_csts(regs::CSTS_RDY | regs::CSTS_CFS, regs::CSTS_RDY, cap.ready_timeout_ns(), true)
            .map_err(|c| format!("not ready after enable (CSTS {c:#x}, {} ms)", cap.ready_timeout_ns() / MS))?;
        self.still_starting()?;
        // Vectors: index 0 serves the admin CQ, index q (if granted) I/O queue q (`make_queues`).
        let admin_vec = match mode {
            IrqMode::MsiX(n) if n >= 2 => Some(0),
            // One vector: the I/O queue is reaped on it; admin commands (rare) poll.
            _ => None,
        };
        a.vector = admin_vec;
        // Identify Controller.
        let page = self.admin_data(a, AdminCmd::identify(cmd::CNS_CTRL, 0, a.page.dev_addr()), regs::PAGE)?;
        let id = ident::parse_ctrl(&page);
        klog::event!(Info, "nvme", "ctrl.identified", ctrl = self.name.as_str(), model = id.model.as_str(), serial = id.serial.as_str(),
            firmware = id.firmware.as_str(), vid = klog::Hex(id.vid as u64), mdts = id.mdts, namespaces = id.nn, cache = id.volatile_cache());
        let depth = (regs::IO_DEPTH as u32).min(cap.mqes()).max(2) as u16;
        let max_transfer = id.max_transfer(cap.mpsmin_shift()).map_or(usize::MAX, |b| usize::try_from(b).unwrap_or(usize::MAX));
        // One PRP list page bounds a command, with one page more for a start inside a page.
        let xfer = max_transfer.min(prp::max_bytes() - regs::PAGE).max(regs::PAGE);
        let queues = self.make_queues(a, cap, mode, depth, xfer)?;
        let namespaces = self.scan_namespaces(a, version, id.nn);
        drop(adm);
        let qinfo: Vec<QueueInfo> = queues.iter().map(|q| QueueInfo { qid: q.qid, vector: q.vector, cpu: q.cpu }).collect();
        let io_vec = qinfo.first().and_then(|q| q.vector);
        let set = Arc::new(IoSet::new(queues, self.hw.cpus()));
        *self.io.write() = Some(set.clone());
        self.hooks.publish(Some(set));
        let mut info = self.info.lock();
        let changed = info.namespaces.iter().map(|n| (n.nsid, n.id.nsze, n.block_size())).ne(namespaces.iter().map(|n| (n.nsid, n.id.nsze, n.block_size())));
        if changed && restart {
            self.layout.fetch_add(1, Ordering::AcqRel);
            // Other namespaces, other blocks: the checked window no longer means anything.
            *self.window.lock() = None;
        }
        let usable = namespaces.iter().filter(|n| n.unusable.is_none()).count();
        let nq = qinfo.len();
        *info = Info {
            cap,
            version,
            ident: Some(id),
            namespaces,
            irq: Some(mode),
            io_depth: depth,
            max_transfer: xfer,
            io_vector: io_vec,
            queues: qinfo,
            starts: info.starts,
            last_error: None,
        };
        drop(info);
        if changed && restart {
            klog::event!(Warn, "nvme", "ns.changed", ctrl = self.name.as_str(); "namespaces differ after the restart: their disks answer Gone");
        }
        klog::event!(Info, "nvme", "ctrl.ready", ctrl = self.name.as_str(), irq = mode.name(), vectors = mode.vectors(), queues = nq,
            io_depth = depth, tags = depth - 1, max_transfer = xfer, namespaces = usable);
        Ok(())
    }

    /// BL-1 (linux-7/drivers/nvme/host/pci.c:2960 `nvme_setup_io_queues`, :2426 `nvme_create_io_queues`): one I/O
    /// queue pair per CPU, fewer when the controller grants fewer (Set Features Number of Queues), when there are
    /// fewer vectors (one per queue; a single vector or MSI: one queue on it, admin polls) or doorbells in BAR0, or
    /// when the memory or a Create command for a later queue fails. Each step down is an event.
    fn make_queues(&self, a: &mut AdminState<H::Mem>, cap: Cap, mode: IrqMode, depth: u16, xfer: usize) -> Result<Vec<IoQueue<H::Mem>>, String> {
        let cpus = self.hw.cpus().max(1);
        // NVMe counts at most 65535 queues (0-based 16 bits).
        let want = cpus.min(0xFFFF) as u16;
        let cqe = self.admin_cmd(a, AdminCmd::set_num_queues(want, want)).map_err(|e| format!("set queue count: {}", e.text()))?;
        let granted = (((cqe.dw0 & 0xFFFF) + 1).min((cqe.dw0 >> 16) + 1)) as usize;
        let mut nq = (want as usize).min(granted);
        if nq < want as usize {
            klog::event!(Info, "nvme", "queues.granted", ctrl = self.name.as_str(), wanted = want, granted = granted; "the controller grants fewer queues than CPUs: CPUs share");
        }
        let by_vectors = match mode {
            IrqMode::MsiX(n) if n >= 2 => n as usize - 1,
            IrqMode::Polled => usize::MAX,
            _ => 1,
        };
        if nq > by_vectors {
            klog::event!(Info, "nvme", "queues.vectors", ctrl = self.name.as_str(), queues = nq, vectors = mode.vectors(), mode = mode.name();
                "one queue per I/O vector");
            nq = by_vectors;
        }
        // Linux nvme_remap_bar: the highest queue's doorbells must lie in BAR0.
        let stride = cap.stride();
        let fit = self.hw.window().saturating_sub(regs::DOORBELLS + 4 + stride) / (2 * stride);
        if nq > fit {
            klog::event!(Warn, "nvme", "queues.bar", ctrl = self.name.as_str(), queues = nq, fit = fit; "BAR0 holds fewer doorbells");
            nq = fit;
        }
        let keep = xfer / regs::PAGE + 2;
        let mut out: Vec<IoQueue<H::Mem>> = Vec::with_capacity(nq);
        for qid in 1..=nq as u16 {
            let vector = match mode {
                IrqMode::MsiX(n) if n >= 2 => Some(qid),
                IrqMode::Polled => None,
                _ => Some(0),
            };
            let cpu = vector.and_then(|v| self.hw.irq_cpu(v));
            let dbs = (regs::doorbell(qid, false, stride), regs::doorbell(qid, true, stride));
            let q = match (self.dma(depth as usize * regs::SQE), self.dma(depth as usize * regs::CQE)) {
                (Ok(sq), Ok(cq)) => IoQueue::new(qid, depth, sq, cq, vector, cpu, dbs, keep, self.cfg.io_timeout_ns.max(1)),
                _ => None,
            };
            let made = match q {
                None => Err(String::from("queue memory")),
                Some(q) => {
                    let (sq_addr, cq_addr) = (q.sq.lock().mem.dev_addr(), q.cq.lock().mem.dev_addr());
                    self.admin_cmd(a, AdminCmd::create_cq(qid, depth, cq_addr, vector.unwrap_or(0), vector.is_some()))
                        .map_err(|e| format!("create I/O CQ {qid}: {}", e.text()))
                        .and_then(|_| {
                            // A CQ without its SQ never gets a completion: its memory may be freed.
                            self.admin_cmd(a, AdminCmd::create_sq(qid, depth, sq_addr, qid)).map_err(|e| format!("create I/O SQ {qid}: {}", e.text()))
                        })
                        .map(|_| q)
                }
            };
            match made {
                Ok(q) => out.push(q),
                Err(e) if qid == 1 => return Err(e),
                Err(e) => {
                    klog::event!(Warn, "nvme", "queues.fewer", ctrl = self.name.as_str(), made = qid - 1, wanted = nq, error = e.as_str(); "going on with the queues made");
                    break;
                }
            }
        }
        Ok(out)
    }

    fn dma(&self, len: usize) -> Result<H::Mem, String> {
        self.hw.alloc_dma(len.max(regs::PAGE).next_multiple_of(regs::PAGE))
    }

    fn read64(&self, off: usize) -> u64 {
        let lo = self.hw.rd32(off) as u64;
        let hi = self.hw.rd32(off + 4) as u64;
        lo | hi << 32
    }
    /// Low half first (Linux `lo_hi_writeq`).
    fn write64(&self, off: usize, v: u64) {
        self.hw.wr32(off, v as u32);
        self.hw.wr32(off + 4, (v >> 32) as u32);
    }

    fn scan_namespaces(&self, a: &mut AdminState<H::Mem>, version: u32, nn: u32) -> Vec<Namespace> {
        let mut ids: Vec<u32> = Vec::new();
        let mut listed = version >= 0x0001_0100;
        if listed {
            let mut after = 0u32;
            loop {
                match self.admin_data(a, AdminCmd::identify(cmd::CNS_NS_ACTIVE_LIST, after, a.page.dev_addr()), regs::PAGE) {
                    Ok(p) => {
                        let got = ident::parse_ns_list(&p, after);
                        let full = got.len() == regs::PAGE / 4;
                        ids.extend_from_slice(&got);
                        match got.last() {
                            Some(&l) if full => after = l,
                            _ => break,
                        }
                    }
                    Err(e) => {
                        klog::event!(Warn, "nvme", "ns.list_failed", ctrl = self.name.as_str(), error = e.as_str(); "falling back to NSIDs 1..=NN");
                        listed = false;
                        ids.clear();
                        break;
                    }
                }
            }
        }
        if !listed {
            ids = (1..=nn).collect();
        }
        let mut out = Vec::new();
        for nsid in ids {
            let p = match self.admin_data(a, AdminCmd::identify(cmd::CNS_NS, nsid, a.page.dev_addr()), regs::PAGE) {
                Ok(p) => p,
                Err(e) => {
                    klog::event!(Warn, "nvme", "ns.identify_failed", ctrl = self.name.as_str(), nsid = nsid, error = e.as_str());
                    continue;
                }
            };
            let id = ident::parse_ns(&p);
            // An inactive NSID answers zeros (the NN fallback walks every NSID).
            if id.nsze == 0 && !listed {
                continue;
            }
            let unusable = id.unusable();
            klog::event!(Info, "nvme", "ns.found", ctrl = self.name.as_str(), nsid = nsid, blocks = id.nsze, block_size = id.block_size(),
                usable = unusable.is_none(), why = unusable.clone().unwrap_or_default());
            out.push(Namespace { nsid, id, unusable });
        }
        out
    }

    /// An admin command that fills the page; returns its first `len` bytes.
    fn admin_data(&self, a: &mut AdminState<H::Mem>, c: AdminCmd, len: usize) -> Result<Vec<u8>, String> {
        if !a.page.zero() {
            return Err(String::from("identify page memory"));
        }
        self.admin_cmd(a, c).map_err(|e| format!("opcode {:02x}h: {}", c.opcode(), e.text()))?;
        let mut v = vec![0u8; len];
        if !a.page.copy_out(0, &mut v) {
            return Err(String::from("identify page memory"));
        }
        Ok(v)
    }

    pub(crate) fn admin_cmd(&self, a: &mut AdminState<H::Mem>, c: AdminCmd) -> Result<Cqe, CmdError> {
        let (cid, tail, op) = a.q.push(&c).map_err(|_| CmdError::NotReady)?;
        self.stats.admin[op as usize].fetch_add(1, Ordering::Relaxed);
        self.hw.wr32(regs::doorbell(0, false, self.stride()), tail as u32);
        let r = self.wait(&mut a.q, cid, a.vector, self.cfg.admin_timeout_ns);
        self.note(r, "admin", op)
    }

    pub(crate) fn note(&self, r: Result<Cqe, CmdError>, queue: &str, op: u8) -> Result<Cqe, CmdError> {
        match r {
            Ok(c) if c.ok() => Ok(c),
            Ok(c) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                let e = CmdError::Status { sct: c.sct(), sc: c.sc(), dnr: c.dnr() };
                klog::event!(Warn, "nvme", "cmd.error", ctrl = self.name.as_str(), queue = queue, opcode = klog::Hex(op as u64), error = e.text());
                Err(e)
            }
            Err(e) => {
                // An I/O command's timeouts are counted where its wait ran out (`io::finish`), once per wait.
                if e == CmdError::Timeout && queue == "admin" {
                    self.stats.timeouts.fetch_add(1, Ordering::Relaxed);
                }
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                if !self.gone() {
                    klog::event!(Warn, "nvme", "cmd.failed", ctrl = self.name.as_str(), queue = queue, opcode = klog::Hex(op as u64), error = e.text());
                }
                Err(e)
            }
        }
    }

    /// Waits for `cid`'s completion: spins `spin_ns`, then waits on the vector in slices (or polls).
    fn wait<C: Encode>(&self, q: &mut QueuePair<H::Mem, C>, cid: u16, vector: Option<u16>, timeout_ns: u64) -> Result<Cqe, CmdError> {
        let start = self.hw.now_ns();
        let deadline = start.saturating_add(timeout_ns);
        let spin_until = start.saturating_add(self.cfg.spin_ns);
        loop {
            match q.pop() {
                Err(_) => return Err(CmdError::NotReady),
                Ok(Some((cqe, head))) => {
                    self.hw.wr32(regs::doorbell(q.qid, true, self.stride()), head as u32);
                    if cqe.cid == cid && cqe.sq_id == q.qid {
                        return Ok(cqe);
                    }
                    self.stats.stale.fetch_add(1, Ordering::Relaxed);
                    // Bounded even if a broken controller floods the ring with strangers.
                    if self.hw.now_ns() >= deadline {
                        return Err(CmdError::Timeout);
                    }
                    continue;
                }
                Ok(None) => {}
            }
            if self.gone() {
                return Err(CmdError::Gone);
            }
            let now = self.hw.now_ns();
            if now < spin_until {
                core::hint::spin_loop();
                continue;
            }
            let csts = self.hw.rd32(regs::CSTS);
            if csts == !0 {
                return Err(CmdError::Gone);
            }
            if csts & regs::CSTS_CFS != 0 {
                return Err(CmdError::Fatal);
            }
            if now >= deadline {
                return Err(CmdError::Timeout);
            }
            let slice = (deadline - now).min(SLICE_NS);
            if !vector.is_some_and(|v| self.hw.wait_irq(v, slice)) {
                self.hw.relax();
                self.hw.pause_us(10, false);
            }
        }
    }

    /// NV-2p: `block::gpt::window`'s checked range is the only one this controller writes from now on.
    pub fn arm_window(&self, nsid: u32, first: u64, blocks: u64) {
        klog::event!(Info, "nvme", "window.armed", ctrl = self.name.as_str(), nsid = nsid, first = first, blocks = blocks);
        *self.window.lock() = Some(WriteWindow::new(nsid, first, blocks));
    }

    /// The boot ESP's range beside the armed window of `nsid` (fs-fat's /boot there, through `block::LowerWindow`).
    /// False: no window of that namespace is armed, or the ranges overlap.
    pub fn open_boot_window(&self, nsid: u32, first: u64, blocks: u64) -> bool {
        let mut w = self.window.lock();
        let Some(next) = w.filter(|w| w.nsid() == nsid).and_then(|w| w.with_boot(first, blocks)) else { return false };
        *w = Some(next);
        drop(w);
        klog::event!(Info, "nvme", "window.boot_opened", ctrl = self.name.as_str(), nsid = nsid, first = first, blocks = blocks);
        true
    }

    pub fn close_boot_window(&self) {
        let mut w = self.window.lock();
        if let Some(x) = *w
            && x.boot().is_some()
        {
            *w = Some(x.without_boot());
            drop(w);
            klog::event!(Info, "nvme", "window.boot_closed", ctrl = self.name.as_str());
        }
    }

    pub fn disarm_window(&self) {
        if self.window.lock().take().is_some() {
            klog::event!(Info, "nvme", "window.disarmed", ctrl = self.name.as_str());
        }
    }

    pub fn window(&self) -> Option<WriteWindow> {
        *self.window.lock()
    }

    pub(crate) fn outside(&self, what: &str, nsid: u32, lba: u64, blocks: u64) -> CmdError {
        let n = self.stats.window_refused.fetch_add(1, Ordering::Relaxed);
        // A bug above may send many: the first ones are enough to find it, the counter keeps the rest.
        if n < 16 {
            klog::event!(Error, "nvme", "window.refused", ctrl = self.name.as_str(), op = what, nsid = nsid, lba = lba, blocks = blocks; "R8a: outside the write window, no command was built");
        }
        CmdError::OutsideWindow
    }

    /// The SMART / Health log (global, NSID FFFFFFFFh).
    pub fn smart(&self) -> Result<Smart, String> {
        self.log_page(cmd::LOG_SMART, 512).map(|b| ident::parse_smart(&b))
    }

    /// Get Log Page of `len` bytes (at most one page).
    pub fn log_page(&self, lid: u8, len: usize) -> Result<Vec<u8>, String> {
        let len = len.clamp(4, regs::PAGE) & !3;
        self.with_admin(|a| self.admin_data(a, AdminCmd::get_log_page(lid, 0xFFFF_FFFF, (len / 4) as u32, a.page.dev_addr()), len))
    }

    /// Get Features; returns completion dword 0.
    pub fn get_feature(&self, fid: u8) -> Result<u32, String> {
        self.with_admin(|a| self.admin_cmd(a, AdminCmd::get_features(fid)).map(|c| c.dw0).map_err(|e| e.text()))
    }

    pub(crate) fn with_admin<R>(&self, f: impl FnOnce(&mut AdminState<H::Mem>) -> Result<R, String>) -> Result<R, String> {
        if self.state() != State::Ready {
            return Err(format!("controller {}", self.state().name()));
        }
        let mut g = self.admin.lock();
        let a = g.as_mut().ok_or_else(|| String::from("no admin queue"))?;
        f(a)
    }

    /// Stops the controller for Reload, reboot, a panic or a remove: lock-free, no sleep, no klog, bounded
    /// spins. Normal shutdown first (CC.SHN, wait SHST complete: what Linux's `nvme_shutdown` does at reboot
    /// and kexec, pci.c:3100, so the SSD records a clean stop), then CC.EN = 0 and wait RDY = 0, then MSI-X and
    /// bus master off.
    pub fn quiesce(&self) {
        let was = State::from(self.state.swap(State::Quiesced as u8, Ordering::AcqRel));
        let mut note = 0x8000_0000 | (was as u32) << 8;
        let cap = Cap { raw: self.cap_raw.load(Ordering::Acquire) };
        let cc = self.hw.rd32(regs::CC);
        if cc != !0 && cc & regs::CC_EN != 0 {
            self.hw.wr32(regs::CC, (cc & !regs::CC_SHN_MASK) | regs::CC_SHN_NORMAL);
            if self.wait_csts(regs::CSTS_SHST_MASK, regs::CSTS_SHST_CMPLT, QUIESCE_SHUTDOWN_NS, false).is_ok() {
                note |= 1;
            }
            self.hw.wr32(regs::CC, cc & !(regs::CC_EN | regs::CC_SHN_MASK));
            if self.wait_csts(regs::CSTS_RDY, 0, cap.ready_timeout_ns().min(QUIESCE_RDY_MAX_NS), false).is_ok() {
                note |= 2;
            }
        } else if cc != !0 && self.wait_csts(regs::CSTS_RDY, 0, cap.ready_timeout_ns().min(QUIESCE_RDY_MAX_NS), false).is_ok() {
            note |= 2;
        }
        self.hw.pci_off();
        self.bump_stop();
        self.quiesce_note.store(note, Ordering::Release);
    }

    /// Quiesced or failed -> up again (ktests, MCP). Sleeps.
    pub fn restart(&self) -> Result<(), String> {
        if self.state() == State::Removed {
            return Err(String::from("removed"));
        }
        self.start()
    }

    /// The function goes away: stop for good.
    pub fn remove(&self) {
        self.quiesce();
        self.state.store(State::Removed as u8, Ordering::Release);
        self.hw.free_irqs();
    }
}
