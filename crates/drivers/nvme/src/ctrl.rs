//! One controller: bring-up (linux-6.6/drivers/nvme/host/pci.c:1716 `nvme_pci_configure_admin_queue`,
//! core.c:2197 `nvme_disable_ctrl`, :2223 `nvme_enable_ctrl`, pci.c:2257 `nvme_setup_io_queues`), the admin and
//! I/O queue with their commands, recovery after a timeout or a fatal status (reset, set-up again, the command
//! once more), and Quiesce. Read side only (R8a, NV-1): the I/O queue carries Read and Flush, nothing else.
//!
//! Locks: `nvme.io` (Mutex) > `nvme.admin` (Mutex) > `nvme.info` (SpinLock, leaf). A command is placed and
//! waited for under its queue's lock (one outstanding command per queue: the waiting thread is the only one on
//! its vector, as PC-3's `IrqHandle::wait` wants). Quiesce takes no lock.

use crate::cmd::{self, AdminCmd, Cqe, Encode, IoCmd};
use crate::hw::{DmaMem, Hw, IrqMode};
use crate::ident::{self, CtrlId, NsId, Smart};
use crate::prp;
use crate::queue::QueuePair;
use crate::regs::{self, Cap};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use ksync::{Mutex, SpinLock};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// Longest single wait on a vector before the controller status is looked at again (a lost interrupt costs
/// at most this).
const SLICE_NS: u64 = MS;
/// Quiesce bounds (interrupts off, other CPUs parked: bounded spins). Linux waits `shutdown_timeout` (5 s
/// default, core.c:54) for the shutdown and (CAP.TO + 1) / 2 s for RDY (core.c:2219); the same here, the
/// latter at most 10 s.
const QUIESCE_SHUTDOWN_NS: u64 = 5 * SEC;
const QUIESCE_RDY_MAX_NS: u64 = 10 * SEC;

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
        }
    }
    /// Worth a controller reset and one more try.
    fn recoverable(&self) -> bool {
        matches!(self, CmdError::Timeout | CmdError::Fatal)
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
    pub io_depth: u16,
    /// Bytes per Read command (MDTS, the PRP list page and the bounce buffer bound it).
    pub max_transfer: usize,
    pub io_vector: Option<u16>,
    pub starts: u32,
    pub last_error: Option<String>,
}

pub struct Stats {
    /// Commands placed in the admin / I/O queue, by the opcode byte actually written to the slot.
    pub admin: [AtomicU64; 256],
    pub io: [AtomicU64; 256],
    pub errors: AtomicU64,
    pub timeouts: AtomicU64,
    pub resets: AtomicU64,
    /// Completions that matched no waiting command (late ones after a timeout).
    pub stale: AtomicU64,
    pub bytes_read: AtomicU64,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            admin: [const { AtomicU64::new(0) }; 256],
            io: [const { AtomicU64::new(0) }; 256],
            errors: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        }
    }
    /// Media-changing commands ever placed in a queue (R8a: always 0 in NV-1).
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

struct AdminState<M: DmaMem> {
    q: QueuePair<M, AdminCmd>,
    /// One page for Identify and log pages.
    page: M,
    vector: Option<u16>,
}

struct IoState<M: DmaMem> {
    q: QueuePair<M, IoCmd>,
    bounce: M,
    list: M,
    vector: Option<u16>,
}

pub struct Controller<H: Hw> {
    pub name: String,
    pub hw: H,
    cfg: Config,
    state: AtomicU8,
    /// CAP and RTD3E for Quiesce (no lock there).
    cap_raw: AtomicU64,
    stride: AtomicU32,
    /// Changes whenever a restart found other namespaces (the namespace devices compare it).
    layout: AtomicU64,
    quiesce_note: AtomicU32,
    /// A start or a recovery is running (they never overlap; the PCI calls of a start run with no lock held).
    starting: AtomicBool,
    admin: Mutex<Option<AdminState<H::Mem>>>,
    io: Mutex<Option<IoState<H::Mem>>>,
    info: SpinLock<Info>,
    pub stats: Stats,
}

impl<H: Hw> Controller<H> {
    pub fn new(name: String, hw: H, cfg: Config) -> Controller<H> {
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
            io: Mutex::named("nvme.io", None),
            info: SpinLock::named("nvme.info", Info::default()),
            stats: Stats::new(),
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
    fn stride(&self) -> usize {
        self.stride.load(Ordering::Relaxed) as usize
    }
    fn gone(&self) -> bool {
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
            .map(|()| if self.cfg.irqs { self.hw.setup_irqs(2) } else { IrqMode::Polled });
        let mut io = self.io.lock();
        self.start_locked(&mut io, Some(mode))
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
        Ok(cap)
    }

    /// `mode`: the PCI side's result for a start; None for a recovery, which resets only the controller and
    /// keeps the vectors.
    /// A start: the state is Starting already (`start_unlocked`). A recovery: Ready -> Starting here; any other
    /// state (a Quiesce or remove came first) refuses it untouched.
    fn start_locked(&self, io: &mut Option<IoState<H::Mem>>, mode: Option<Result<IrqMode, String>>) -> Result<(), String> {
        if mode.is_none() && self.state.compare_exchange(State::Ready as u8, State::Starting as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(format!("controller {}", self.state().name()));
        }
        let r = match mode {
            Some(Err(e)) => Err(e),
            Some(Ok(m)) => self.still_starting().and_then(|()| self.bring_up(io, m)),
            None => {
                let m = self.info.lock().irq.unwrap_or(IrqMode::Polled);
                self.bring_up(io, m)
            }
        };
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
        let cc = self.hw.rd32(regs::CC);
        let enabled = cc != !0 && cc & regs::CC_EN != 0;
        if enabled {
            self.hw.wr32(regs::CC, cc & !(regs::CC_EN | regs::CC_SHN_MASK));
            let cap = Cap { raw: self.cap_raw.load(Ordering::Acquire) };
            let _ = self.wait_csts(regs::CSTS_RDY, 0, cap.ready_timeout_ns().min(QUIESCE_RDY_MAX_NS), true);
        }
        self.hw.pci_off();
        klog::event!(Warn, "nvme", "ctrl.start_aborted", ctrl = self.name.as_str(), state = now.name(), was_enabled = enabled;
            "a Quiesce or remove came during the start: controller and bus master off again");
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

    fn bring_up(&self, io: &mut Option<IoState<H::Mem>>, mode: IrqMode) -> Result<(), String> {
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
        // Vectors: index 0 serves the admin CQ, index 1 (if granted) the I/O CQ.
        let (admin_vec, io_vec) = match mode {
            IrqMode::MsiX(n) if n >= 2 => (Some(0), Some(1)),
            IrqMode::Polled => (None, None),
            // One vector: the I/O queue waits on it; admin commands (rare) poll, so one thread per index.
            _ => (None, Some(0)),
        };
        a.vector = admin_vec;
        // Identify Controller.
        let page = self.admin_data(a, AdminCmd::identify(cmd::CNS_CTRL, 0, a.page.dev_addr()), regs::PAGE)?;
        let id = ident::parse_ctrl(&page);
        klog::event!(Info, "nvme", "ctrl.identified", ctrl = self.name.as_str(), model = id.model.as_str(), serial = id.serial.as_str(),
            firmware = id.firmware.as_str(), vid = klog::Hex(id.vid as u64), mdts = id.mdts, namespaces = id.nn, cache = id.volatile_cache());
        // One I/O queue pair (per-CPU queues come later).
        let cqe = self.admin_cmd(a, AdminCmd::set_num_queues(1, 1)).map_err(|e| format!("set queue count: {}", e.text()))?;
        let granted = ((cqe.dw0 & 0xFFFF) + 1).min((cqe.dw0 >> 16) + 1);
        if granted == 0 {
            return Err(String::from("no I/O queue granted"));
        }
        let depth = (regs::IO_DEPTH as u32).min(cap.mqes()).max(2) as u16;
        let max_transfer = id.max_transfer(cap.mpsmin_shift()).map_or(usize::MAX, |b| usize::try_from(b).unwrap_or(usize::MAX));
        let want = max_transfer.min(prp::max_bytes() - regs::PAGE).max(regs::PAGE);
        // Kept over restarts (the controller was disabled above, so it no longer reaches them).
        if io.as_ref().is_none_or(|s| s.q.depth != depth) {
            let q = QueuePair::new(1, depth, self.dma(depth as usize * regs::SQE)?, self.dma(depth as usize * regs::CQE)?)
                .ok_or_else(|| String::from("I/O queue memory"))?;
            *io = Some(IoState { q, bounce: self.bounce(want)?, list: self.dma(regs::PAGE)?, vector: None });
        }
        let Some(s) = io.as_mut() else { return Err(String::from("I/O queue missing")) };
        if !s.q.reset() {
            return Err(String::from("I/O queue memory"));
        }
        s.vector = io_vec;
        let iv = io_vec.unwrap_or(0);
        self.admin_cmd(a, AdminCmd::create_cq(1, depth, s.q.cq.dev_addr(), iv, io_vec.is_some()))
            .map_err(|e| format!("create I/O CQ: {}", e.text()))?;
        self.admin_cmd(a, AdminCmd::create_sq(1, depth, s.q.sq.dev_addr(), 1)).map_err(|e| format!("create I/O SQ: {}", e.text()))?;
        let namespaces = self.scan_namespaces(a, version, id.nn);
        drop(adm);
        let xfer = s.bounce.size().min(want);
        let mut info = self.info.lock();
        let changed = info.namespaces.iter().map(|n| (n.nsid, n.id.nsze, n.block_size())).ne(namespaces.iter().map(|n| (n.nsid, n.id.nsze, n.block_size())));
        if changed && restart {
            self.layout.fetch_add(1, Ordering::AcqRel);
        }
        let usable = namespaces.iter().filter(|n| n.unusable.is_none()).count();
        *info = Info { cap, version, ident: Some(id), namespaces, irq: Some(mode), io_depth: depth, max_transfer: xfer, io_vector: io_vec, starts: info.starts, last_error: None };
        drop(info);
        if changed && restart {
            klog::event!(Warn, "nvme", "ns.changed", ctrl = self.name.as_str(); "namespaces differ after the restart: their disks answer Gone");
        }
        klog::event!(Info, "nvme", "ctrl.ready", ctrl = self.name.as_str(), irq = mode.name(), vectors = mode.vectors(), io_depth = depth,
            max_transfer = xfer, namespaces = usable);
        Ok(())
    }

    fn dma(&self, len: usize) -> Result<H::Mem, String> {
        self.hw.alloc_dma(len.max(regs::PAGE).next_multiple_of(regs::PAGE))
    }

    /// The bounce buffer: as large as wanted, halved while memory is short (down to one page).
    fn bounce(&self, want: usize) -> Result<H::Mem, String> {
        let mut len = want.next_multiple_of(regs::PAGE);
        loop {
            match self.hw.alloc_dma(len) {
                Ok(m) => return Ok(m),
                Err(e) if len <= regs::PAGE => return Err(e),
                Err(_) => {
                    len = (len / 2).next_multiple_of(regs::PAGE);
                    klog::event!(Warn, "nvme", "bounce.smaller", ctrl = self.name.as_str(), bytes = len; "DMA memory short: smaller reads");
                }
            }
        }
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

    fn admin_cmd(&self, a: &mut AdminState<H::Mem>, c: AdminCmd) -> Result<Cqe, CmdError> {
        let (cid, tail, op) = a.q.push(&c).map_err(|_| CmdError::NotReady)?;
        self.stats.admin[op as usize].fetch_add(1, Ordering::Relaxed);
        self.hw.wr32(regs::doorbell(0, false, self.stride()), tail as u32);
        let r = self.wait(&mut a.q, cid, a.vector, self.cfg.admin_timeout_ns);
        self.note(r, "admin", op)
    }

    fn note(&self, r: Result<Cqe, CmdError>, queue: &str, op: u8) -> Result<Cqe, CmdError> {
        match r {
            Ok(c) if c.ok() => Ok(c),
            Ok(c) => {
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                let e = CmdError::Status { sct: c.sct(), sc: c.sc(), dnr: c.dnr() };
                klog::event!(Warn, "nvme", "cmd.error", ctrl = self.name.as_str(), queue = queue, opcode = klog::Hex(op as u64), error = e.text());
                Err(e)
            }
            Err(e) => {
                if e == CmdError::Timeout {
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

    fn ready_io<'a>(&self, io: &'a mut Option<IoState<H::Mem>>) -> Result<&'a mut IoState<H::Mem>, CmdError> {
        match self.state() {
            State::Ready => io.as_mut().ok_or(CmdError::NotReady),
            State::Quiesced | State::Removed => Err(CmdError::Gone),
            _ => Err(CmdError::NotReady),
        }
    }

    /// One I/O command under the lock; after a timeout or fatal status the controller is reset and set up
    /// again (with the lock still held) and the command placed once more (B6: each step logged).
    fn io_cmd(&self, io: &mut Option<IoState<H::Mem>>, make: &dyn Fn(&IoState<H::Mem>) -> Result<IoCmd, CmdError>) -> Result<Cqe, CmdError> {
        let mut tried = false;
        loop {
            let s = self.ready_io(io)?;
            let c = make(s)?;
            let (cid, tail, op) = s.q.push(&c).map_err(|_| CmdError::NotReady)?;
            self.stats.io[op as usize].fetch_add(1, Ordering::Relaxed);
            self.hw.wr32(regs::doorbell(1, false, self.stride()), tail as u32);
            let r = self.wait(&mut s.q, cid, s.vector, self.cfg.io_timeout_ns);
            let r = self.note(r, "io", op);
            match r {
                Err(e) if e.recoverable() && !tried && !self.gone() => {
                    tried = true;
                    if self.starting.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                        klog::event!(Warn, "nvme", "ctrl.recover_skipped", ctrl = self.name.as_str(); "a start is running");
                        return Err(e);
                    }
                    self.stats.resets.fetch_add(1, Ordering::Relaxed);
                    klog::event!(Warn, "nvme", "ctrl.recover", ctrl = self.name.as_str(), error = e.text(); "reset and set-up again, then one retry");
                    // The reset stops the controller's DMA before the bounce buffer is used again.
                    let r = self.start_locked(io, None);
                    self.starting.store(false, Ordering::Release);
                    if let Err(why) = r {
                        klog::event!(Error, "nvme", "ctrl.recover_failed", ctrl = self.name.as_str(), error = why.as_str());
                        return Err(e);
                    }
                    klog::event!(Info, "nvme", "ctrl.recovered", ctrl = self.name.as_str());
                }
                r => return r,
            }
        }
    }

    /// Reads `buf.len() / block_size` blocks of `nsid` from `lba`, in transfers of at most `max_transfer`.
    /// `check` = bounds of the namespace are checked (ktests pass false to see the device's own answer).
    pub fn read(&self, nsid: u32, lba: u64, buf: &mut [u8], check: bool) -> Result<(), CmdError> {
        let (bs, blocks, xfer) = {
            let i = self.info.lock();
            let ns = i.namespaces.iter().find(|n| n.nsid == nsid);
            match ns {
                Some(n) => (n.block_size() as usize, n.id.nsze, i.max_transfer),
                None if !check => (512, u64::MAX, i.max_transfer),
                None => return Err(CmdError::Invalid),
            }
        };
        if buf.is_empty() || !buf.len().is_multiple_of(bs) || xfer < bs {
            return Err(CmdError::Invalid);
        }
        let n = (buf.len() / bs) as u64;
        if check && lba.checked_add(n).is_none_or(|e| e > blocks) {
            return Err(CmdError::Invalid);
        }
        let per = (xfer / bs).min(65536);
        let mut io = self.io_lock()?;
        let mut done = 0usize;
        for chunk in buf.chunks_mut(per * bs) {
            let at = lba + (done / bs) as u64;
            let len = chunk.len();
            let make = |s: &IoState<H::Mem>| -> Result<IoCmd, CmdError> {
                let base = s.bounce.dev_addr();
                let p = prp::plan(base, len, s.list.dev_addr()).ok_or(CmdError::Invalid)?;
                for i in 0..p.list_len {
                    if !s.list.wr64(8 * i, prp::list_entry(base, i)) {
                        return Err(CmdError::Invalid);
                    }
                }
                Ok(IoCmd::read(nsid, at, (len / bs) as u32, p.prp1, p.prp2))
            };
            self.io_cmd(&mut io, &make)?;
            let s = self.ready_io(&mut io)?;
            if !s.bounce.copy_out(0, chunk) {
                return Err(CmdError::NotReady);
            }
            self.stats.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
            done += len;
        }
        Ok(())
    }

    /// Reads `bytes` from `lba` on into the bounce buffer only (the `nvme` tool's read speed: no copy).
    /// Returns (bytes read, ns taken).
    pub fn read_speed(&self, nsid: u32, lba: u64, bytes: u64) -> Result<(u64, u64), CmdError> {
        let (bs, blocks, xfer) = {
            let i = self.info.lock();
            let n = i.namespaces.iter().find(|n| n.nsid == nsid && n.unusable.is_none()).ok_or(CmdError::Invalid)?;
            (n.block_size() as u64, n.id.nsze, i.max_transfer as u64)
        };
        let per = (xfer / bs).clamp(1, 65536);
        let total = (bytes / bs).min(blocks.saturating_sub(lba));
        let mut io = self.io_lock()?;
        let t0 = self.hw.now_ns();
        let mut at = lba;
        while at < lba + total {
            let n = per.min(lba + total - at);
            let len = (n * bs) as usize;
            let make = |s: &IoState<H::Mem>| -> Result<IoCmd, CmdError> {
                let base = s.bounce.dev_addr();
                let p = prp::plan(base, len, s.list.dev_addr()).ok_or(CmdError::Invalid)?;
                for i in 0..p.list_len {
                    if !s.list.wr64(8 * i, prp::list_entry(base, i)) {
                        return Err(CmdError::Invalid);
                    }
                }
                Ok(IoCmd::read(nsid, at, n as u32, p.prp1, p.prp2))
            };
            self.io_cmd(&mut io, &make)?;
            self.stats.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
            at += n;
        }
        Ok((total * bs, self.hw.now_ns().saturating_sub(t0)))
    }

    /// The I/O queue's lock, bounded (B4: the block layer's dispatcher is Interactive): a holder needs at most
    /// a command's timeout plus one recovery.
    fn io_lock(&self) -> Result<ksync::MutexGuard<'_, Option<IoState<H::Mem>>>, CmdError> {
        let bound = self.cfg.io_timeout_ns.saturating_mul(2).saturating_add(Cap { raw: self.cap_raw.load(Ordering::Acquire) }.ready_timeout_ns() * 2);
        self.io.lock_timeout(bound).ok_or_else(|| {
            klog::event!(Warn, "nvme", "io.lock_timeout", ctrl = self.name.as_str(), ms = bound / MS);
            CmdError::Timeout
        })
    }

    pub fn flush(&self, nsid: u32) -> Result<(), CmdError> {
        let mut io = self.io_lock()?;
        self.io_cmd(&mut io, &|_| Ok(IoCmd::flush(nsid))).map(|_| ())
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

    fn with_admin<R>(&self, f: impl FnOnce(&mut AdminState<H::Mem>) -> Result<R, String>) -> Result<R, String> {
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
