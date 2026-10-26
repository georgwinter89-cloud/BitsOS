//! One virtio-net card: bring-up, the `NetDevice` data path (polled), link watch, restart, `Quiesce`.

use crate::caps::{self, feat, VirtioCaps};
use crate::queue::{ring_bytes, QueueError, VirtQueue};
use crate::transport::{status, Transport, NO_VECTOR};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use devmodel::net::{LinkState, MacAddr, NetDevice, NetStats};
use devmodel::{DevError, Quiesce};
use ksync::SpinLock;
use mm::dma::{DmaBuffer, DmaCache, DmaConstraints};

/// `struct virtio_net_hdr` with VERSION_1 (num_buffers included): 12 bytes in front of every frame.
pub const NET_HDR: usize = 12;
/// One buffer per frame: header + 1514-byte frame fit (no mergeable buffers, no TSO).
pub const BUF_SIZE: usize = 2048;
/// Ring size used when the device allows more. A ring-size parameter (spec §3: "ring sizes as parameters"),
/// not a limit on a count of things (R1).
pub const DEFAULT_RING: u16 = 256;
const RX_Q: u16 = 0;
const TX_Q: u16 = 1;
const RESET_LIMIT_NS: u64 = 100_000_000;
const WATCH_EVERY_NS: u64 = 100_000_000;
/// Restart back-off after a failure: 1 s, doubling, at most 60 s (the cable may be the only way in).
const RESTART_FIRST_NS: u64 = 1_000_000_000;
const RESTART_MAX_NS: u64 = 60_000_000_000;
/// The back-off falls back to `RESTART_FIRST_NS` only after the card ran this long without a failure, so a card
/// that comes up and breaks again at once is not reset every second forever.
const STABLE_NS: u64 = 10_000_000_000;
/// How long an explicit `restart` waits for a restart the poller is running (two resets take at most 200 ms).
const RESTART_WAIT_NS: u64 = 1_000_000_000;

// Device configuration offsets (virtio 1.2 §5.1.4).
const CFG_MAC: usize = 0;
const CFG_STATUS: usize = 6;
const CFG_MTU: usize = 10;
const CFG_SPEED: usize = 12;
const CFG_DUPLEX: usize = 16;
const LINK_UP: u16 = 1;

/// NW-4: with two MSI-X vectors, configuration changes (the link bit) go to 0 and both queues to 1 (the stack
/// serves receive and transmit in one round); with one vector everything goes to 0.
const IRQ_POLLED: u8 = 0;
const IRQ_ONE: u8 = 1;
const IRQ_TWO: u8 = 2;
const VEC_CONFIG: u16 = 0;

klog::counter!(RX_FRAMES, "net.virtio.rx_frames");
klog::counter!(TX_FRAMES, "net.virtio.tx_frames");
klog::counter!(RX_BYTES, "net.virtio.rx_bytes");
klog::counter!(TX_BYTES, "net.virtio.tx_bytes");
klog::counter!(RX_DROPPED, "net.virtio.rx_dropped");
klog::counter!(TX_RING_FULL, "net.virtio.tx_ring_full");
klog::counter!(POLL_TX_DROPPED, "net.virtio.poll_tx_dropped");
klog::counter!(RESTARTS, "net.virtio.restarts");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Down = 0,
    Running = 1,
    /// Stopped by `Quiesce` (Reload, reboot, panic): stays stopped until `restart` is called explicitly.
    Quiesced = 2,
    /// The device or the driver found an error; the poller restarts it with back-off.
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

struct Queue {
    vq: VirtQueue<DmaBuffer>,
    notify_off: usize,
    /// RX buffers given back to the device since the last notify (RX kicks are batched, see `receive`).
    kick_pending: u32,
}

impl Queue {
    fn alloc(index: u16, size: u16) -> Result<Queue, String> {
        let (d, a, u) = ring_bytes(size as usize);
        // Modern virtio takes 64-bit addresses: no reason to use up DMA32 memory. WriteBack (snooped), so
        // the notify needs no `write_barrier`; spelled out, not defaulted.
        let c = DmaConstraints { limit: u64::MAX, cache: DmaCache::WriteBack, ..DmaConstraints::default() };
        let get = |n: usize| DmaBuffer::alloc(n, c).map_err(|e| format!("DMA {n} bytes: {e:?}"));
        let vq = VirtQueue::new(index, size, get(d)?, get(a)?, get(u)?, get(size as usize * BUF_SIZE)?, BUF_SIZE)
            .map_err(|e| format!("queue {index}: {e:?}"))?;
        Ok(Queue { vq, notify_off: 0, kick_pending: 0 })
    }
}

#[derive(Default)]
struct Stats {
    rx_frames: AtomicU64,
    tx_frames: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_dropped: AtomicU64,
    tx_dropped: AtomicU64,
}

pub struct VirtioNet {
    name: String,
    pci: pci::PciFunction,
    t: Transport,
    rx: SpinLock<Queue>,
    tx: SpinLock<Queue>,
    state: AtomicU8,
    features: AtomicU64,
    /// MAC in the low 48 bits (byte 0 lowest).
    mac: AtomicU64,
    mtu: AtomicUsize,
    speed_mbps: AtomicU32,
    full_duplex: AtomicBool,
    link_up: AtomicBool,
    next_watch: AtomicU64,
    next_restart: AtomicU64,
    restart_delay: AtomicU64,
    /// When the card last reached Running (for resetting the back-off after `STABLE_NS`).
    up_since: AtomicU64,
    /// Held while a restart runs: one restart at a time (poller, ktests, Reload's undo path).
    restarting: AtomicBool,
    /// Quiesce does not log (it may run on the panic path, where klog's locks may be held); it leaves a note
    /// here that the next `poll` or `restart` logs. Bit 7 = note present, bit 6 = reset finished, low bits = old state.
    quiesce_note: AtomicU8,
    rx_bad_logged: AtomicBool,
    stats: Stats,
    /// PC-3's interrupt set of the last bring-up (dropped outside the lock: dropping may sleep).
    irqs: SpinLock<Option<pci::IrqSet>>,
    irq_mode: AtomicU8,
    /// Vectors that fired since the last look (bit = index), set by the hard-IRQ hook.
    pending: Arc<AtomicU32>,
    /// What `net::netdev::set_irq_driven` was last told (0 nothing, 1 polled, 2 interrupts).
    irq_reported: AtomicU8,
}

fn now() -> u64 {
    time::now_ns()
}

fn mac_str(m: [u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

impl VirtioNet {
    /// Maps the card, learns its queue sizes, allocates the rings and brings it up.
    pub fn probe(f: &pci::PciFunction, caps: &VirtioCaps, index: usize) -> Result<VirtioNet, String> {
        // MSI-X and bus master off before anything (PC-2 `disable_device`): a ring a previous kernel left running
        // must write nowhere. Then `enable_device(false)`: D0, memory decoding, INTx off, bus master still off.
        let _ = f.disable_device();
        f.enable_device(false).map_err(|e| format!("enable_device(false): {e}"))?;
        let t = Transport::map(f, caps)?;
        if !t.reset(RESET_LIMIT_NS) {
            return Err("device did not finish its reset".into());
        }
        negotiate(&t)?;
        if t.num_queues() < 2 {
            return Err(format!("device has {} queues, needs RX and TX", t.num_queues()));
        }
        let size = |q: u16| -> Result<u16, String> {
            let max = t.queue_max(q).map_err(|e| format!("queue {q} size: {e:?}"))?;
            let s = max.min(DEFAULT_RING);
            if s == 0 {
                return Err(format!("queue {q} is not available"));
            }
            // Split rings want a power of two; take the largest that fits.
            Ok(1 << (15 - s.leading_zeros().min(15)))
        };
        let (rxs, txs) = (size(RX_Q)?, size(TX_Q)?);
        let d = VirtioNet {
            name: format!("virtio-net{index}"),
            pci: f.clone(),
            t,
            rx: SpinLock::named("virtio-net.rx", Queue::alloc(RX_Q, rxs)?),
            tx: SpinLock::named("virtio-net.tx", Queue::alloc(TX_Q, txs)?),
            state: AtomicU8::new(State::Down as u8),
            features: AtomicU64::new(0),
            mac: AtomicU64::new(0),
            mtu: AtomicUsize::new(1500),
            speed_mbps: AtomicU32::new(0),
            full_duplex: AtomicBool::new(true),
            link_up: AtomicBool::new(false),
            next_watch: AtomicU64::new(0),
            next_restart: AtomicU64::new(0),
            restart_delay: AtomicU64::new(RESTART_FIRST_NS),
            up_since: AtomicU64::new(0),
            restarting: AtomicBool::new(false),
            quiesce_note: AtomicU8::new(0),
            rx_bad_logged: AtomicBool::new(false),
            stats: Stats::default(),
            irqs: SpinLock::named("virtio-net.irqs", None),
            irq_mode: AtomicU8::new(IRQ_POLLED),
            pending: Arc::new(AtomicU32::new(0)),
            irq_reported: AtomicU8::new(0),
        };
        d.bring_up()?;
        Ok(d)
    }

    pub fn state(&self) -> State {
        State::from(self.state.load(Ordering::Acquire))
    }
    pub fn features(&self) -> u64 {
        self.features.load(Ordering::Relaxed)
    }
    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }
    /// The device status register (for checks: 0 after a reset / quiesce).
    pub fn device_status(&self) -> u8 {
        self.t.status()
    }
    /// (RX size, RX buffers with the device, TX size, TX buffers free).
    pub fn ring_info(&self) -> (u16, usize, u16, usize) {
        let rx = self.rx.lock();
        let tx = self.tx.lock();
        (rx.vq.size(), rx.vq.in_flight_count(), tx.vq.size(), tx.vq.free_count())
    }

    /// Ktests: fails the card as a broken ring would.
    pub(crate) fn fail_for_test(&self) {
        self.fail("ktest", "injected failure".into());
    }
    /// Ktests: (current back-off, next restart time).
    pub(crate) fn backoff(&self) -> (u64, u64) {
        (self.restart_delay.load(Ordering::Relaxed), self.next_restart.load(Ordering::Relaxed))
    }

    /// Runs `f` while the TX lock is held, as a panic in the middle of `transmit` would leave it (ktests).
    pub(crate) fn with_tx_locked<R>(&self, f: impl FnOnce() -> R) -> R {
        let _g = self.tx.lock();
        f()
    }

    /// Reset, feature negotiation, queues, RX buffers, bus master, DRIVER_OK. Used at probe and restart; the
    /// caller has claimed the card (state Down). On any error the device is reset and bus master is cleared
    /// again, so a half-initialised card with armed RX rings cannot DMA during the back-off (Linux does the same
    /// in virtio_dev_probe's error path).
    fn bring_up(&self) -> Result<(), String> {
        let r = self.bring_up_steps();
        if r.is_err() {
            let _ = self.t.reset(RESET_LIMIT_NS);
            let _ = self.pci.disable_device();
        }
        r
    }

    fn bring_up_steps(&self) -> Result<(), String> {
        self.link_up.store(false, Ordering::Release);
        let _ = self.pci.disable_device();
        self.pci.enable_device(false).map_err(|e| format!("enable_device(false): {e}"))?;
        if !self.t.reset(RESET_LIMIT_NS) {
            return Err("device did not finish its reset".into());
        }
        let f = negotiate(&self.t)?;
        self.features.store(f, Ordering::Relaxed);
        self.read_config(f);
        // Bus master on the card and every bridge above it (PC-2; the PCI core logs the change), before the
        // interrupt vectors (an MSI-X message is a DMA write; PC-3 wants it first). The device touches no ring
        // before DRIVER_OK.
        self.pci.enable_device(true).map_err(|e| format!("enable_device(true): {e}"))?;
        let (cfg_vec, q_vec) = self.setup_irqs();
        for (q, lock) in [(RX_Q, &self.rx), (TX_Q, &self.tx)] {
            let mut g = lock.lock();
            g.vq.reset().map_err(|e| format!("queue {q} reset: {e:?}"))?;
            let max = self.t.queue_max(q).map_err(|e| format!("queue {q}: {e:?}"))?;
            if max < g.vq.size() {
                return Err(format!("queue {q}: device now allows {max} entries, ring has {}", g.vq.size()));
            }
            // TX completions need no interrupt: a full ring is reclaimed by the next send and by the poller.
            g.vq.set_interrupts(q == RX_Q);
            let (d, a, u) = g.vq.addrs();
            g.notify_off = self.t.setup_queue(q, g.vq.size(), d, a, u, q_vec).map_err(|e| format!("queue {q} setup: {e:?}"))?;
        }
        if cfg_vec != NO_VECTOR && !self.t.config_vector(cfg_vec).unwrap_or(false) {
            klog::event!(Warn, "net", "virtio.config_vector_refused", ifname = self.name.as_str(); "link changes are seen by the 100 ms watch only");
        }
        {
            let mut rx = self.rx.lock();
            while let Some(id) = rx.vq.take_free() {
                rx.vq.submit(id, BUF_SIZE as u32, true).map_err(|e| format!("RX fill: {e:?}"))?;
            }
        }
        self.t.add_status(status::DRIVER_OK).map_err(|e| format!("DRIVER_OK: {e:?}"))?;
        let st = self.t.status();
        if st & (status::FAILED | status::NEEDS_RESET) != 0 {
            return Err(format!("device status {st:#x} after DRIVER_OK"));
        }
        let mut rx = self.rx.lock();
        self.t.notify(rx.notify_off, RX_Q);
        rx.kick_pending = 0;
        let (rxs, txs) = (rx.vq.size(), self.tx.lock().vq.size());
        drop(rx);
        // A Quiesce that ran meanwhile wins: the card stays stopped (the caller's error path resets it again).
        if self.state.compare_exchange(State::Down as u8, State::Running as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(format!("stopped during bring-up (state {})", self.state().as_str()));
        }
        self.up_since.store(now(), Ordering::Relaxed);
        klog::event!(Info, "net", "virtio.up", ifname = self.name.as_str(), mac = mac_str(self.mac_bytes()),
            features = klog::Hex(f), rx_ring = rxs, tx_ring = txs, mtu = self.mtu.load(Ordering::Relaxed),
            irq = self.irq_setup());
        self.watch_link(true);
        self.report_irq();
        Ok(())
    }

    /// PC-3 `alloc_irqs` (NW-4): MSI-X with two vectors (configuration, queues) or one; else polled (logged).
    /// Returns the (configuration, queue) table entries to program, NO_VECTOR when polled. The set of the bring-up
    /// before (released by `disable_device`) is freed first, outside the lock.
    fn setup_irqs(&self) -> (u16, u16) {
        let old = self.irqs.lock().take();
        drop(old);
        self.pending.store(0, Ordering::Release);
        self.irq_mode.store(IRQ_POLLED, Ordering::Release);
        if net::config("net_irq").is_some_and(|v| v.trim() == "off") {
            return (NO_VECTOR, NO_VECTOR);
        }
        let p = self.pending.clone();
        let hook: pci::IrqHook = Arc::new(move |i: u16| {
            p.fetch_or(1u32 << (i & 31), Ordering::AcqRel);
            net::netdev::irq_wake();
        });
        // Virtio's MSI-X only: a plain MSI has no per-queue routing in the virtio-pci interface.
        // A virtio queue has no interrupt moderation: a burst from QEMU (1,000 connections closing at once: 1,000
        // FIN/ACKs) gives one interrupt per frame while the stack keeps up, more than PC-3's default 100 per ms. A
        // stuck line delivers far beyond 1,000 per ms, which is still caught.
        let storm = pci::StormLimit { per_window: 1000, window_ns: 1_000_000 };
        let req = pci::IrqRequest::new("virtio-net", 1, 2).modes(true, false, false).storm(Some(storm)).hook(hook);
        match self.pci.alloc_irqs(&req) {
            Ok(set) if set.mode() == pci::IrqMode::MsiX && !set.is_empty() => {
                let two = set.len() >= 2;
                *self.irqs.lock() = Some(set);
                self.irq_mode.store(if two { IRQ_TWO } else { IRQ_ONE }, Ordering::Release);
                if two { (VEC_CONFIG, 1) } else { (0, 0) }
            }
            Ok(_) => (NO_VECTOR, NO_VECTOR),
            Err(e) => {
                klog::event!(Warn, "net", "virtio.irq_fallback", ifname = self.name.as_str(), error = format!("{e}");
                    "no MSI-X vectors: polled");
                (NO_VECTOR, NO_VECTOR)
            }
        }
    }

    /// How the card's interrupts arrive now.
    pub fn irq_setup(&self) -> &'static str {
        match self.irq_mode.load(Ordering::Acquire) {
            IRQ_TWO => "msix2",
            IRQ_ONE => "msix1",
            _ => "polled",
        }
    }

    /// The interrupt set's per-index numbers (ktests).
    pub fn irq_stats(&self) -> Option<(alloc::vec::Vec<pci::IrqStats>, bool)> {
        let g = self.irqs.lock();
        let set = g.as_ref()?;
        Some(((0..set.len()).filter_map(|i| set.stats(i)).collect(), set.is_released()))
    }

    /// Tells the stack whether this card wakes it by interrupt (on change only): running, MSI-X armed, the PCI
    /// core still delivering (not released, no storm hold). Thread context.
    pub fn report_irq(&self) {
        let degraded = self.irqs.lock().as_ref().is_none_or(|s| s.is_released() || (0..s.len()).any(|i| s.stats(i).is_some_and(|x| x.stormed)));
        let driven = self.state() == State::Running && self.irq_mode.load(Ordering::Acquire) != IRQ_POLLED && !degraded;
        let now = if driven { 2 } else { 1 };
        if self.irq_reported.swap(now, Ordering::AcqRel) != now {
            net::netdev::set_irq_driven(&self.name, driven);
        }
    }

    /// The look an interrupt asked for, before the RX ring is read: receive interrupts off while the ring is
    /// drained (NAPI), and a configuration interrupt (two vectors) reads the link bit at once.
    fn irq_service(&self, rx: &mut Queue) {
        if self.pending.load(Ordering::Acquire) == 0 {
            return;
        }
        let p = self.pending.swap(0, Ordering::AcqRel);
        if self.irq_mode.load(Ordering::Acquire) == IRQ_POLLED {
            return;
        }
        rx.vq.set_interrupts(false);
        if self.irq_mode.load(Ordering::Acquire) == IRQ_ONE || p & (1 << VEC_CONFIG) != 0 {
            self.watch_link(false);
        }
    }

    /// The RX ring ran empty: receive interrupts on again; if the device used a buffer meanwhile, the stack is
    /// woken to look once more (no lost frame between the last look and the flag).
    fn irq_rearm(&self, rx: &mut Queue) {
        if self.irq_mode.load(Ordering::Relaxed) != IRQ_POLLED && rx.vq.set_interrupts(true) {
            net::netdev::irq_wake();
        }
    }

    fn read_config(&self, f: u64) {
        let mut mac = [0u8; 6];
        if f & feat::NET_MAC == 0 || self.t.read_config(CFG_MAC, &mut mac).is_err() {
            // Locally administered, stable per slot: 02:00:00:bus:dev:func.
            let a = self.pci.addr;
            mac = [0x02, 0, 0, a.bus, a.dev, a.func];
            klog::event!(Warn, "net", "virtio.mac_fallback", ifname = self.name.as_str(), mac = mac_str(mac));
        }
        let mut m = 0u64;
        for (i, b) in mac.iter().enumerate() {
            m |= (*b as u64) << (8 * i);
        }
        self.mac.store(m, Ordering::Relaxed);
        let mtu = if f & feat::NET_MTU != 0 {
            self.t.config_u16(CFG_MTU).ok().map(|m| (m as usize).clamp(68, 1500)).unwrap_or(1500)
        } else {
            1500
        };
        self.mtu.store(mtu, Ordering::Relaxed);
        if f & feat::NET_SPEED_DUPLEX != 0 {
            let s = self.t.config_u32(CFG_SPEED).unwrap_or(u32::MAX);
            self.speed_mbps.store(if s == u32::MAX { 0 } else { s }, Ordering::Relaxed);
            let mut d = [1u8];
            let _ = self.t.read_config(CFG_DUPLEX, &mut d);
            self.full_duplex.store(d[0] != 0, Ordering::Relaxed);
        }
    }

    fn mac_bytes(&self) -> [u8; 6] {
        let m = self.mac.load(Ordering::Relaxed);
        core::array::from_fn(|i| (m >> (8 * i)) as u8)
    }

    /// Reads the link bit (or assumes up without VIRTIO_NET_F_STATUS) and logs changes.
    fn watch_link(&self, force_log: bool) {
        let up = if self.features() & feat::NET_STATUS != 0 {
            self.t.config_u16(CFG_STATUS).map(|s| s & LINK_UP != 0).unwrap_or(false)
        } else {
            true
        };
        let was = self.link_up.swap(up, Ordering::AcqRel);
        if was != up || force_log {
            if up {
                klog::event!(Info, "net", "link.up", ifname = self.name.as_str(), speed_mbps = self.speed_mbps.load(Ordering::Relaxed));
            } else {
                klog::event!(Warn, "net", "link.down", ifname = self.name.as_str());
            }
        }
    }

    /// Running -> Failed and schedules the restart. Never touches another state: a Quiesce that raced with
    /// the failing transmit/receive must stay in force.
    fn fail(&self, what: &str, error: String) {
        if self.state.compare_exchange(State::Running as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let delay = self.schedule_restart();
            klog::event!(Error, "net", "virtio.failed", ifname = self.name.as_str(), during = what, error = error,
                retry_in_ms = delay / 1_000_000);
        }
    }

    /// Sets the next restart time from the current back-off and doubles the back-off (at most 60 s).
    fn schedule_restart(&self) -> u64 {
        let delay = self.restart_delay.load(Ordering::Relaxed);
        self.next_restart.store(now().saturating_add(delay), Ordering::Relaxed);
        self.restart_delay.store(delay.saturating_mul(2).min(RESTART_MAX_NS), Ordering::Relaxed);
        delay
    }

    /// Logs the note `quiesce` left behind, if any.
    fn log_quiesce_note(&self) {
        let n = self.quiesce_note.swap(0, Ordering::AcqRel);
        if n & 0x80 != 0 {
            klog::event!(Info, "net", "virtio.quiesce", ifname = self.name.as_str(), was = State::from(n & 0x3f).as_str(),
                reset_done = n & 0x40 != 0);
        }
    }

    /// Stops and starts the card again, from any state (an explicit request: ktests, Reload's undo path, link
    /// restart). Waits a bounded time for a restart the poller is running.
    pub fn restart(&self, reason: &str) -> Result<(), String> {
        let deadline = now().saturating_add(RESTART_WAIT_NS);
        while self.restarting.swap(true, Ordering::AcqRel) {
            if now() >= deadline {
                klog::event!(Warn, "net", "virtio.restart_busy", ifname = self.name.as_str(), reason = reason);
                return Err("another restart of this card is still running".into());
            }
            core::hint::spin_loop();
        }
        self.state.store(State::Down as u8, Ordering::Release);
        let r = self.restart_claimed(reason);
        self.restarting.store(false, Ordering::Release);
        r
    }

    /// The poller's restart: only if the card is still in `from` (a Quiesce in between cancels it) and no other
    /// restart is running.
    fn auto_restart(&self, from: State, reason: &str) {
        if self.restarting.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.state.compare_exchange(from as u8, State::Down as u8, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _ = self.restart_claimed(reason);
        }
        self.restarting.store(false, Ordering::Release);
    }

    /// Caller holds `restarting` and has set the state to Down.
    fn restart_claimed(&self, reason: &str) -> Result<(), String> {
        self.log_quiesce_note();
        RESTARTS.inc();
        klog::event!(Warn, "net", "virtio.restart", ifname = self.name.as_str(), reason = reason);
        let Err(e) = self.bring_up() else { return Ok(()) };
        if self.state.compare_exchange(State::Down as u8, State::Failed as u8, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // Quiesced while coming up: stays stopped, no retry.
            klog::event!(Warn, "net", "virtio.restart_aborted", ifname = self.name.as_str(), error = e.as_str());
            return Err(e);
        }
        let _ = self.t.set_status(status::FAILED);
        let delay = self.schedule_restart();
        klog::event!(Error, "net", "virtio.restart_failed", ifname = self.name.as_str(), error = e.as_str(),
            retry_in_ms = delay / 1_000_000);
        Err(e)
    }

    /// Reclaims TX buffers the device has sent. Caller holds the TX lock.
    fn reclaim(&self, tx: &mut Queue) -> Result<(), QueueError> {
        while let Some((id, _)) = tx.vq.pop_used()? {
            tx.vq.recycle(id);
        }
        Ok(())
    }

    /// Puts one frame into the TX ring. No klog here: `poll_tx` (the panic mirror path) shares it.
    fn send_locked(&self, tx: &mut Queue, frame: &[u8]) -> Result<(), DevError> {
        let id = match tx.vq.take_free() {
            Some(id) => id,
            None => {
                self.reclaim(tx).map_err(|_| DevError::Io)?;
                tx.vq.take_free().ok_or(DevError::Timeout)?
            }
        };
        if !(tx.vq.write_buf(id, 0, &[0u8; NET_HDR]) && tx.vq.write_buf(id, NET_HDR, frame)) {
            tx.vq.recycle(id);
            return Err(DevError::Io);
        }
        tx.vq.submit(id, (NET_HDR + frame.len()) as u32, false).map_err(|_| DevError::Io)?;
        if tx.vq.needs_notify() {
            self.t.notify(tx.notify_off, TX_Q);
        }
        self.stats.tx_frames.fetch_add(1, Ordering::Relaxed);
        self.stats.tx_bytes.fetch_add(frame.len() as u64, Ordering::Relaxed);
        TX_FRAMES.inc();
        TX_BYTES.add(frame.len() as u64);
        Ok(())
    }

    /// Tells the device about refilled RX buffers. Caller holds the RX lock.
    fn kick_rx(&self, rx: &mut Queue) {
        if rx.vq.needs_notify() {
            self.t.notify(rx.notify_off, RX_Q);
        }
        rx.kick_pending = 0;
    }

    fn frame_ok(&self, frame: &[u8]) -> bool {
        frame.len() >= 14 && frame.len() <= BUF_SIZE - NET_HDR
    }
}

/// Reset is done by the caller; ACKNOWLEDGE, DRIVER, features, FEATURES_OK (§3.1.1).
fn negotiate(t: &Transport) -> Result<u64, String> {
    t.add_status(status::ACKNOWLEDGE | status::DRIVER).map_err(|e| format!("status: {e:?}"))?;
    let offered = t.device_features().map_err(|e| format!("features: {e:?}"))?;
    let f = caps::negotiate(offered).map_err(String::from)?;
    t.set_driver_features(f).map_err(|e| format!("features: {e:?}"))?;
    t.add_status(status::FEATURES_OK).map_err(|e| format!("status: {e:?}"))?;
    if t.status() & status::FEATURES_OK == 0 {
        let _ = t.set_status(status::FAILED);
        return Err(format!("device refused features {f:#x} (offered {offered:#x})"));
    }
    Ok(f)
}

impl NetDevice for VirtioNet {
    fn name(&self) -> &str {
        &self.name
    }
    fn mac(&self) -> MacAddr {
        MacAddr(self.mac_bytes())
    }
    fn mtu(&self) -> usize {
        self.mtu.load(Ordering::Relaxed)
    }
    fn link(&self) -> LinkState {
        LinkState {
            up: self.state() == State::Running && self.link_up.load(Ordering::Acquire),
            speed_mbps: self.speed_mbps.load(Ordering::Relaxed),
            full_duplex: self.full_duplex.load(Ordering::Relaxed),
        }
    }

    fn transmit(&self, frame: &[u8]) -> Result<(), DevError> {
        if !self.frame_ok(frame) {
            return Err(DevError::InvalidArgument);
        }
        // No frame to a card that is stopped or has no link (Linux carrier-off); the stack retries later.
        if self.state() != State::Running || !self.link_up.load(Ordering::Acquire) {
            self.stats.tx_dropped.fetch_add(1, Ordering::Relaxed);
            return Err(DevError::Io);
        }
        let mut tx = self.tx.lock();
        let r = self.send_locked(&mut tx, frame);
        drop(tx);
        match r {
            Err(DevError::Timeout) => {
                TX_RING_FULL.inc();
            }
            Err(DevError::Io) => self.fail("transmit", "TX ring refused the frame (ring memory or device index)".into()),
            _ => {}
        }
        r
    }

    fn receive(&self, buf: &mut [u8]) -> Option<usize> {
        if self.state() != State::Running {
            return None;
        }
        let mut rx = self.rx.lock();
        self.irq_service(&mut rx);
        // Bounded (B4): unusable frames are dropped and their buffers refilled, so a device or peer sending only
        // such frames could keep this loop going. One ring's worth per call; the rest comes with the next call.
        for _ in 0..rx.vq.size() {
            let (id, len) = match rx.vq.pop_used() {
                Ok(Some(x)) => x,
                Ok(None) => {
                    self.irq_rearm(&mut rx);
                    return None;
                }
                Err(e) => {
                    drop(rx);
                    self.fail("receive", format!("{e:?}"));
                    return None;
                }
            };
            let len = len as usize;
            let frame_len = len.wrapping_sub(NET_HDR);
            let good = len > NET_HDR && len <= BUF_SIZE;
            let fits = good && frame_len <= buf.len();
            let copied = fits && rx.vq.read_buf(id, NET_HDR, &mut buf[..frame_len]);
            if let Err(e) = rx.vq.submit(id, BUF_SIZE as u32, true) {
                drop(rx);
                self.fail("RX refill", format!("{e:?}"));
                return None;
            }
            // Kicks are batched (a notify is a VM exit under KVM): the device reads avail.idx itself while it has
            // buffers and needs a kick only after it ran dry, which takes a quarter ring of refills to fix; the
            // poller flushes the rest.
            rx.kick_pending += 1;
            if rx.kick_pending >= (rx.vq.size() as u32 / 4).max(1) {
                self.kick_rx(&mut rx);
            }
            if copied {
                self.stats.rx_frames.fetch_add(1, Ordering::Relaxed);
                self.stats.rx_bytes.fetch_add(frame_len as u64, Ordering::Relaxed);
                RX_FRAMES.inc();
                RX_BYTES.add(frame_len as u64);
                return Some(frame_len);
            }
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            RX_DROPPED.inc();
            if !good && !self.rx_bad_logged.swap(true, Ordering::Relaxed) {
                klog::event!(Warn, "net", "virtio.rx_bad_length", ifname = self.name.as_str(), len = len; "logged once per card");
            }
        }
        self.irq_rearm(&mut rx);
        None
    }

    /// Emergency path (panic mirror): no blocking lock, no klog, no allocation. Drops the frame when the TX
    /// lock is held (the panic may have interrupted a transmit) or the ring is full.
    fn poll_tx(&self, frame: &[u8]) -> bool {
        if !self.frame_ok(frame) || self.state() != State::Running {
            POLL_TX_DROPPED.inc();
            return false;
        }
        let Some(mut tx) = self.tx.try_lock() else {
            POLL_TX_DROPPED.inc();
            return false;
        };
        let ok = self.send_locked(&mut tx, frame).is_ok();
        if !ok {
            POLL_TX_DROPPED.inc();
        }
        ok
    }

    /// Housekeeping from the poller: device health, link watch (every 100 ms), TX reclaim, restarts.
    fn poll(&self) {
        self.log_quiesce_note();
        let t = now();
        match self.state() {
            State::Running => {
                if let Some(mut rx) = self.rx.try_lock()
                    && rx.kick_pending > 0
                {
                    self.kick_rx(&mut rx);
                }
                if t < self.next_watch.load(Ordering::Relaxed) {
                    return;
                }
                self.next_watch.store(t.saturating_add(WATCH_EVERY_NS), Ordering::Relaxed);
                let st = self.t.status();
                if st & (status::NEEDS_RESET | status::FAILED) != 0 || st & status::DRIVER_OK == 0 {
                    // Through the back-off like any failure: a device that keeps asking is not reset every tick.
                    self.fail("watch", format!("device status {st:#x}: asks for a reset"));
                    return;
                }
                if self.restart_delay.load(Ordering::Relaxed) != RESTART_FIRST_NS
                    && t.saturating_sub(self.up_since.load(Ordering::Relaxed)) >= STABLE_NS
                {
                    self.restart_delay.store(RESTART_FIRST_NS, Ordering::Relaxed);
                }
                self.watch_link(false);
                self.report_irq();
                if let Some(mut tx) = self.tx.try_lock()
                    && let Err(e) = self.reclaim(&mut tx)
                {
                    drop(tx);
                    self.fail("TX reclaim", format!("{e:?}"));
                }
            }
            State::Failed => {
                self.report_irq();
                if t >= self.next_restart.load(Ordering::Relaxed) {
                    self.auto_restart(State::Failed, "after failure");
                }
            }
            State::Down | State::Quiesced => {}
        }
    }

    fn stats(&self) -> NetStats {
        let s = &self.stats;
        NetStats {
            rx_frames: s.rx_frames.load(Ordering::Relaxed),
            tx_frames: s.tx_frames.load(Ordering::Relaxed),
            rx_bytes: s.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: s.tx_bytes.load(Ordering::Relaxed),
            rx_dropped: s.rx_dropped.load(Ordering::Relaxed),
            tx_dropped: s.tx_dropped.load(Ordering::Relaxed),
        }
    }
}

impl Quiesce for VirtioNet {
    fn name(&self) -> &str {
        &self.name
    }
    /// Device reset (stops all DMA, §2.1: the device forgets the queues) then PC-2 `disable_device` (MSI-X and
    /// bus master off). Lock-free, spinning only and without klog (whose locks the panicking CPU may hold), so
    /// it works from Reload, reboot and panic. The `virtio.quiesce` event is logged later by `poll` or `restart` from the note left here.
    /// A restart in progress sees the state change and gives up instead of bringing the card back.
    fn quiesce(&self) {
        let was = self.state.swap(State::Quiesced as u8, Ordering::AcqRel);
        self.link_up.store(false, Ordering::Release);
        let reset = self.t.reset(RESET_LIMIT_NS);
        let _ = self.pci.disable_device();
        self.quiesce_note.store(0x80 | if reset { 0x40 } else { 0 } | (was & 0x3f), Ordering::Release);
    }
}
