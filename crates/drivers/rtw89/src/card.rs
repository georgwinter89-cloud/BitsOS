//! One RTL8852BE in the kernel: the driver thread (spec/wlan.md §2.4 trap 2: served from its own
//! thread, whatever programs do), the queues the netdev side shares with it, the status copy the
//! MCP tool reads, the commands it sends (scan, join, leave) and Quiesce.
//!
//! Locks: `core` (a sleeping mutex) is the thread's while it works on the chip; everything the
//! network stack or MCP touch (`txq`, `rxq`, `status`, `cmds`) is a short spin lock, so neither
//! ever waits for a chip access (B4). Quiesce only tries `core` (interrupts off, no sleeping):
//! when the thread holds it, bus master and MSI off alone keep the chip from DMA, and the shared
//! `quiesced` flag makes the bus refuse the thread's further work (`kbus` module text).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use devmodel::Quiesce;
use ksync::{Completion, Mutex, SpinLock, WaitQueue};
use rtw89_core::Bus;
use rtw89_core::deadread::CmacRead;
use rtw89_core::drv::status::Status;
use rtw89_core::drv::{DevConfig, DevEvent, Level, Runner, TurnOut};
use rtw89_core::fw::Firmware;
use rtw89_core::sta::StaEnv;

use crate::kbus::KBus;

/// The stack's frames waiting for the thread; past this `transmit` says "ring full" (the stack
/// sends again later). The TX ring's own length: the chip cannot take more at once anyway.
pub fn txq_max() -> usize {
    DevConfig::kernel().ach0 as usize
}
/// Frames for the stack it has not taken yet; past this the oldest go (counted). Four RX rings.
pub fn rxq_max() -> usize {
    DevConfig::kernel().rxq as usize * 4
}
/// The status copy is renewed after a turn that changed something, else at least this often
pub const STATUS_EVERY_MS: u64 = 1_000;
/// The longest the thread sleeps without a look (B4: bounded; the health look is every 2 s)
pub const MAX_SLEEP_MS: u64 = 2_000;

/// What MCP asks the thread to do
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Scan { channels: Vec<u32>, dwell_ms: u32 },
    Join,
    Leave,
}

/// An answer the MCP tool waits for (bounded)
pub struct Reply {
    pub done: Completion,
    pub out: SpinLock<Option<Result<Vec<String>, String>>>,
}

impl Reply {
    pub fn new() -> Reply {
        Reply { done: Completion::new(), out: SpinLock::named("rtw89.reply", None) }
    }
}

impl Default for Reply {
    fn default() -> Reply {
        Reply::new()
    }
}

/// The thread's side: the bus (with Linux's CMAC read rule), the runner once the firmware is in
pub struct Core {
    pub bus: CmacRead<KBus>,
    pub runner: Option<Runner>,
}

/// The kernel's StaEnv: the monotonic clock, sleeping, the kernel's random generator
pub struct KEnv;

impl<B: Bus + ?Sized> StaEnv<B> for KEnv {
    fn now_ms(&mut self, _: &mut B) -> u64 {
        now_ms()
    }
    fn sleep_ms(&mut self, _: &mut B, ms: u32) {
        sched::sleep_ns(u64::from(ms) * 1_000_000);
    }
    fn random(&mut self, out: &mut [u8]) -> bool {
        vfs::random::fill(out);
        true
    }
}

pub fn now_ms() -> u64 {
    time::now_ns() / 1_000_000
}

pub struct Card {
    pub name: String,
    /// "04:00.0" (none for the glue-only card of the ktests)
    pub function: String,
    pci: Option<pci::PciFunction>,
    pub core: Mutex<Option<Core>>,
    pub txq: SpinLock<VecDeque<Vec<u8>>>,
    pub rxq: SpinLock<VecDeque<Vec<u8>>>,
    pub status: SpinLock<Status>,
    /// what the thread does when it has no runner yet (starting, firmware missing, ...)
    pub phase: SpinLock<String>,
    pub cmds: SpinLock<VecDeque<(Cmd, Arc<Reply>)>>,
    pub link_up: AtomicBool,
    /// the interface's MAC (48 bits), 0 until the first bring-up
    pub mac: AtomicU64,
    pub registered: AtomicBool,
    /// set by Quiesce; the same flag is the bus's `gone`
    pub quiesced: Arc<AtomicBool>,
    pending: AtomicBool,
    wake: WaitQueue,
    pub tx_queued: AtomicU64,
    pub tx_refused: AtomicU64,
    pub tx_no_link: AtomicU64,
    pub rx_taken: AtomicU64,
    pub rx_overflow: AtomicU64,
    /// PC-3's interrupt set (MSI) when the chip got one; deliveries counted by the hook
    pub irqs: SpinLock<Option<pci::IrqSet>>,
    pub irq_count: AtomicU64,
}

impl Card {
    pub fn new(name: String, function: String, pci: Option<pci::PciFunction>, mut core: Option<Core>) -> Card {
        let quiesced = Arc::new(AtomicBool::new(false));
        if let Some(c) = core.as_mut() {
            c.bus.inner.gone = quiesced.clone();
        }
        Card {
            name,
            function,
            pci,
            core: Mutex::named("rtw89.core", core),
            txq: SpinLock::named("rtw89.txq", VecDeque::new()),
            rxq: SpinLock::named("rtw89.rxq", VecDeque::new()),
            status: SpinLock::named("rtw89.status", Status::default()),
            phase: SpinLock::named("rtw89.phase", String::from("starting")),
            cmds: SpinLock::named("rtw89.cmds", VecDeque::new()),
            link_up: AtomicBool::new(false),
            mac: AtomicU64::new(0),
            registered: AtomicBool::new(false),
            quiesced,
            pending: AtomicBool::new(false),
            wake: WaitQueue::new(),
            tx_queued: AtomicU64::new(0),
            tx_refused: AtomicU64::new(0),
            tx_no_link: AtomicU64::new(0),
            rx_taken: AtomicU64::new(0),
            rx_overflow: AtomicU64::new(0),
            irqs: SpinLock::named("rtw89.irqs", None),
            irq_count: AtomicU64::new(0),
        }
    }

    pub fn mac_bytes(&self) -> [u8; 6] {
        let v = self.mac.load(Ordering::Acquire).to_le_bytes();
        [v[0], v[1], v[2], v[3], v[4], v[5]]
    }

    /// Wakes the thread (a frame to send, a command); lock-free
    pub fn kick(&self) {
        self.pending.store(true, Ordering::Release);
        self.wake.wake_one();
    }

    pub fn set_phase(&self, p: &str) {
        let mut g = self.phase.lock();
        if g.as_str() != p {
            g.clear();
            g.push_str(p);
        }
    }

    /// A command for the thread, its answer awaited at most `timeout_ns` (B4)
    pub fn command(&self, cmd: Cmd, timeout_ns: u64) -> Result<Vec<String>, String> {
        let reply = Arc::new(Reply::default());
        self.cmds.lock().push_back((cmd, reply.clone()));
        self.kick();
        if !reply.done.wait_timeout(timeout_ns) {
            // still queued: taken back, so a retry never piles up stale scans, joins or leaves
            let taken_back = {
                let mut q = self.cmds.lock();
                let n = q.len();
                q.retain(|(_, r)| !Arc::ptr_eq(r, &reply));
                q.len() != n
            };
            return Err(String::from(if taken_back {
                "the driver thread is busy with the chip (a join or a bring-up): the command was not run, try again"
            } else {
                "the driver thread is still running the command: see status"
            }));
        }
        reply.out.lock().take().unwrap_or_else(|| Err(String::from("no answer")))
    }

    /// Frames from the stack for the thread
    pub fn queue_tx(&self, frame: &[u8]) -> Result<(), devmodel::DevError> {
        if !self.link_up.load(Ordering::Acquire) {
            // no link: the frame goes (the stack's ARP/DHCP send again once the link is up)
            self.tx_no_link.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        {
            let mut q = self.txq.lock();
            if q.len() >= txq_max() {
                self.tx_refused.fetch_add(1, Ordering::Relaxed);
                return Err(devmodel::DevError::Timeout);
            }
            q.push_back(frame.to_vec());
        }
        self.tx_queued.fetch_add(1, Ordering::Relaxed);
        self.kick();
        Ok(())
    }

    /// Frames from the thread for the stack (the oldest go past `rxq_max`)
    pub fn deliver(&self, frames: Vec<Vec<u8>>) {
        if frames.is_empty() {
            return;
        }
        {
            let mut q = self.rxq.lock();
            q.extend(frames);
            while q.len() > rxq_max() {
                q.pop_front();
                self.rx_overflow.fetch_add(1, Ordering::Relaxed);
            }
        }
        net::netdev::irq_wake();
    }

    pub fn take_rx(&self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let f = self.rxq.lock().pop_front()?;
            if f.len() <= buf.len() {
                buf[..f.len()].copy_from_slice(&f);
                self.rx_taken.fetch_add(1, Ordering::Relaxed);
                return Some(f.len());
            }
            self.rx_overflow.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn set_link(&self, out: &TurnOut) {
        if let Some(up) = out.link {
            self.link_up.store(up, Ordering::Release);
            if !up {
                // frames for the old link go; the stack sends again on the new one
                let n = {
                    let mut q = self.txq.lock();
                    let n = q.len();
                    q.clear();
                    n
                };
                self.tx_no_link.fetch_add(n as u64, Ordering::Relaxed);
            }
            klog::event!(Info, "wlan", if up { "netdev.link_up" } else { "netdev.link_down" }, ifname = self.name.as_str());
            net::netdev::irq_wake();
        }
    }
}

/// The driver's events into klog (B6), subsystem "wlan"
pub fn log(ifname: &str, evs: impl Iterator<Item = DevEvent>) {
    for e in evs {
        let t = e.text.as_str();
        match e.level {
            Level::Error => klog::event!(Error, "wlan", e.name, ifname = ifname; "{}", t),
            Level::Warn => klog::event!(Warn, "wlan", e.name, ifname = ifname; "{}", t),
            Level::Info => klog::event!(Info, "wlan", e.name, ifname = ifname; "{}", t),
            Level::Debug => klog::event!(Debug, "wlan", e.name, ifname = ifname; "{}", t),
        }
    }
}

/// The firmware file: the first path that reads and parses
fn load_firmware(paths: &[String]) -> Result<(String, Firmware), String> {
    let ns = vfs::root_ns();
    let mut why = Vec::new();
    for p in paths {
        match ns.read_file(p, crate::config::FW_MAX) {
            Ok(bytes) => {
                let n = bytes.len();
                return Firmware::parse(bytes, rtw89_core::bringup::EXPECT_CV, false)
                    .map(|f| (format!("{p} ({n} bytes)"), f))
                    .map_err(|e| format!("{p}: not a usable rtw8852b firmware: {e:?}"));
            }
            Err(e) => why.push(format!("{p}: {e:?}")),
        }
    }
    Err(why.join("; "))
}

fn enable_failed(card: &Card, e: pci::EnableError) {
    let e = format!("{e}");
    klog::event!(Error, "wlan", "chip.enable_failed", function = card.function.as_str(), error = e.as_str());
    card.set_phase("PCI enable failed");
}

/// The driver thread: memory decoding on and a MAC that is still on (a Reload or a killed run left
/// it, its DMA engines holding the old kernel's addresses) powered off before bus master is ever
/// on; then bus master on the bridges and MSI (both need it), bus master on the function off
/// again until rtw89-core's safety step; the firmware from the stick, the network from CONFIG,
/// then one runner turn per wake-up
pub fn run(card: Arc<Card>) {
    if start(&card) {
        service(&card);
    }
}

// Everything before the first turn, in a frame of its own: its temporaries (the firmware's load,
// CONFIG, the klog events) would otherwise stay under every turn's deepest call on the thread's
// 64 KiB stack. False: no runner (quiesced, PCI enable failed).
#[inline(never)]
fn start(card: &Arc<Card>) -> bool {
    let Some(pci) = card.pci.clone() else { return false };
    if card.quiesced.load(Ordering::Acquire) {
        return false;
    }
    if let Err(e) = pci.enable_device(false) {
        enable_failed(card, e);
        return false;
    }
    mac_off_first(card);
    if card.quiesced.load(Ordering::Acquire) {
        return false;
    }
    if let Err(e) = pci.enable_device(true) {
        enable_failed(card, e);
        return false;
    }
    let irq = setup_msi(card, &pci);
    pci.disable_bus_master();
    if card.quiesced.load(Ordering::Acquire) {
        let _ = pci.disable_device();
        return false;
    }
    let paths = crate::config::fw_paths(&|k| net::config(k));
    let mut tries: u64 = 0;
    let fw = loop {
        if card.quiesced.load(Ordering::Acquire) {
            return false;
        }
        match load_firmware(&paths) {
            Ok((from, fw)) => {
                klog::event!(Info, "wlan", "fw.file", ifname = card.name.as_str(), path = from.as_str());
                break fw;
            }
            Err(e) => {
                // the stick may be mounted after the PCI probe: every second for 30 s, then every 10 s
                if tries == 0 || tries.is_multiple_of(30) {
                    klog::event!(Warn, "wlan", "fw.missing", ifname = card.name.as_str(), tries = tries; "{}", e.as_str());
                }
                card.set_phase("waiting for the firmware file (see the log: fw.missing)");
                tries += 1;
                sched::sleep_ns(if tries < 30 { 1_000_000_000 } else { 10_000_000_000 });
            }
        }
    };
    let network = match crate::config::network_with(&|k| net::config(k), &mut |f| net::take_secret("wlan_psk", f)) {
        Ok(Some(n)) => {
            let ssid = String::from_utf8_lossy(&n.ssid).into_owned();
            klog::event!(Info, "wlan", "config.network", ifname = card.name.as_str(), ssid = ssid.as_str(), channels = n.channels.len());
            Some(n)
        }
        Ok(None) => {
            klog::event!(Info, "wlan", "config.no_network", ifname = card.name.as_str(); "no wlan_ssid= in CONFIG.TXT: the chip comes up, nothing is joined");
            None
        }
        Err(e) => {
            klog::event!(Error, "wlan", "config.bad", ifname = card.name.as_str(); "{}", e.as_str());
            None
        }
    };
    {
        let mut g = card.core.lock();
        let Some(core) = g.as_mut() else { return false };
        if card.quiesced.load(Ordering::Acquire) {
            return false;
        }
        let mut r = Runner::new(fw, DevConfig { rings: DevConfig::kernel(), keep_mac: None });
        r.irq = irq.then(rtw89_core::pci::intr::config_intr_mask);
        r.set_network(network, now_ms());
        core.runner = Some(r);
    }
    card.set_phase("running");
    true
}

/// rtw89-core's MAC-off step with memory decoding on and bus master off (the probe switched it
/// off): a MAC found on or in LPS is powered off and logged as `chip.found_on`
fn mac_off_first(card: &Card) {
    let mut g = card.core.lock();
    let Some(core) = g.as_mut() else { return };
    match rtw89_core::bringup::safety::mac_off_if_on(&mut core.bus) {
        Ok(m) if m.powered_off => {
            klog::event!(Warn, "wlan", "chip.found_on", ifname = card.name.as_str(), mac_state = m.mac_state;
                "the MAC was left on: powered off before bus master")
        }
        Ok(m) if m.dead_look => {
            klog::event!(Warn, "wlan", "chip.dead_look", ifname = card.name.as_str(); "IC_PWR_STATE reads all ones (the bring-up decides)")
        }
        Ok(_) => klog::event!(Debug, "wlan", "chip.found_off", ifname = card.name.as_str()),
        Err(e) => {
            let e = format!("{e:?}");
            klog::event!(Warn, "wlan", "chip.off_failed", ifname = card.name.as_str(), error = e.as_str(); "the bring-up's safety step tries again")
        }
    }
}

/// One MSI vector (the chip has no MSI-X; capability 0x50) whose hook wakes the thread; polled
/// without one, or with CONFIG `net_irq=off` (NW's existing way back for every card)
fn setup_msi(card: &Arc<Card>, pci: &pci::PciFunction) -> bool {
    if net::config("net_irq").is_some_and(|v| v.trim() == "off") {
        klog::event!(Info, "wlan", "irq.polled", ifname = card.name.as_str(), reason = "net_irq=off");
        return false;
    }
    let c = card.clone();
    let hook: pci::IrqHook = Arc::new(move |_| {
        c.irq_count.fetch_add(1, Ordering::Relaxed);
        c.kick();
    });
    let req = pci::IrqRequest::new("rtw89", 1, 1).modes(false, true, false).hook(hook);
    match pci.alloc_irqs(&req) {
        Ok(set) if set.mode() == pci::IrqMode::Msi => {
            klog::event!(Info, "wlan", "irq.msi", ifname = card.name.as_str(); "MSI wakes the driver thread; a 20 ms poll stays as the safety net");
            *card.irqs.lock() = Some(set);
            true
        }
        Ok(set) => {
            let mode = set.mode().name();
            klog::event!(Warn, "wlan", "irq.polled", ifname = card.name.as_str(), reason = mode);
            false
        }
        Err(e) => {
            let e = format!("{e}");
            klog::event!(Warn, "wlan", "irq.polled", ifname = card.name.as_str(), reason = e.as_str());
            false
        }
    }
}

/// A stormed or released MSI line: back to the 2 ms poll, the chip's masks off
fn irq_degraded(card: &Card) -> bool {
    let g = card.irqs.lock();
    match g.as_ref() {
        None => false,
        Some(s) => s.is_released() || s.stats(0).is_some_and(|x| x.stormed),
    }
}

fn service(card: &Card) {
    let mut status_at = 0u64;
    let mut irq_seen = card.irq_count.load(Ordering::Acquire);
    let mut env = KEnv;
    loop {
        if card.quiesced.load(Ordering::Acquire) {
            card.set_phase("quiesced");
            // the chip is off; this thread waits (bounded steps) for the Reload/reboot to take it
            loop {
                sched::sleep_ns(1_000_000_000);
            }
        }
        let now;
        let next;
        let mut changed = false;
        let mut mac = None;
        {
            let mut g = card.core.lock();
            let Some(Core { bus, runner: Some(r) }) = g.as_mut() else { return };
            let cmds: Vec<(Cmd, Arc<Reply>)> = card.cmds.lock().drain(..).collect();
            for (cmd, reply) in cmds {
                let res = command(card, r, bus, &mut env, cmd);
                *reply.out.lock() = Some(res);
                reply.done.complete();
                changed = true;
            }
            let irqs = card.irq_count.load(Ordering::Acquire);
            let by_irq = irqs != irq_seen && r.irq.is_some();
            irq_seen = irqs;
            if by_irq {
                r.irq_ack(bus, now_ms());
            }
            let mut tx = core::mem::take(&mut *card.txq.lock());
            let out = r.turn(bus, &mut env, &mut tx);
            if by_irq {
                r.irq_rearm(bus);
            }
            if !tx.is_empty() {
                let mut q = card.txq.lock();
                while let Some(f) = tx.pop_back() {
                    q.push_front(f);
                }
            }
            changed |= out.link.is_some() || !r.log.is_empty();
            card.set_link(&out);
            card.deliver(out.frames);
            log(&card.name, r.log.drain(..));
            now = now_ms();
            if changed || now >= status_at {
                if r.irq.is_some() && irq_degraded(card) {
                    r.irq = None;
                    let _ = rtw89_core::pci::intr::disable(bus);
                    klog::event!(Warn, "wlan", "irq.polled", ifname = card.name.as_str(), reason = "stormed or released"; "back to the 2 ms poll");
                }
                *card.status.lock() = Status::of(r);
                status_at = now + STATUS_EVERY_MS;
            }
            if !card.registered.load(Ordering::Acquire) {
                mac = r.cfg.keep_mac;
            }
            next = r.next_wake(now);
        }
        if let Some(m) = mac {
            let mut v = [0u8; 8];
            v[..6].copy_from_slice(&m);
            card.mac.store(u64::from_le_bytes(v), Ordering::Release);
            crate::register_netdev(card);
        }
        let ms = next.saturating_sub(now).min(MAX_SLEEP_MS);
        if ms > 0 {
            card.wake.wait_until_timeout(|| card.pending.load(Ordering::Acquire), ms * 1_000_000);
        }
        card.pending.store(false, Ordering::Release);
    }
}

fn command(
    card: &Card,
    r: &mut Runner,
    bus: &mut CmacRead<KBus>,
    env: &mut KEnv,
    cmd: Cmd,
) -> Result<Vec<String>, String> {
    match cmd {
        Cmd::Scan { channels, dwell_ms } => {
            let list = r.scan(bus, env, &channels, dwell_ms)?;
            let mut out = alloc::vec![format!("scan: {} BSS on {} channels", list.len(), channels.len())];
            for b in list {
                let usable = match wifi::rsn::usable(b.rsn().as_ref()) {
                    Ok(_) => String::from("wpa2-psk"),
                    Err(u) => format!("not joinable: {u}"),
                };
                out.push(format!(
                    "bss {} ssid={:?} channel={} signal_dbm={} {}",
                    rtw89_core::drv::device::mac_str(&b.bssid),
                    String::from_utf8_lossy(&b.ssid),
                    b.ch,
                    b.rssi.map_or(String::from("-"), |d| format!("{d}")),
                    usable
                ));
            }
            Ok(out)
        }
        Cmd::Join => {
            r.join(now_ms())?;
            card.kick();
            Ok(alloc::vec![String::from("join: the supervisor is on, the join follows (see status)")])
        }
        Cmd::Leave => {
            let out = r.leave(bus, env);
            card.set_link(&out);
            Ok(alloc::vec![String::from("leave: left, no rejoin until join")])
        }
    }
}

impl Quiesce for Card {
    fn name(&self) -> &str {
        &self.name
    }
    // Before Reload, reboot or a panic: bus master and MSI off first (no DMA from here on), then,
    // when the thread does not hold the chip, the CH12 key copies wiped and the MAC powered off
    // with spinning waits (no sleep, no klog). Every DMA chunk is zeroed before it goes.
    fn quiesce(&self) {
        self.quiesced.store(true, Ordering::SeqCst);
        if let Some(pci) = self.pci.as_ref() {
            let _ = pci.disable_device();
        }
        if let Some(mut g) = self.core.try_lock()
            && let Some(core) = g.as_mut()
        {
            core.bus.inner.quiet = true;
            if let Some(r) = core.runner.as_mut() {
                r.quiesce(&mut core.bus);
            }
            core.bus.inner.dma.wipe_all();
        }
    }
}
