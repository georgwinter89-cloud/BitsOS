//! PC-2 on the machine: `PciFunction::enable_device` / `disable_device` and the BAR save/restore helpers,
//! around the logic in `enable.rs`. Drivers call these instead of writing the command register themselves
//! (CONTRACTS.md §6a). Every change becomes a `pci` event naming the function (B6); waits sleep with no lock
//! held (R5); `disable_device` is safe for `Quiesce` (no klog, no heap, no sleep, bounded lock attempt).
//!
//! Locks, outermost first: `pci.enable` (sleeping mutex, held by `enable_device`, `save_config` and
//! `restore_config` for the whole sequence including the D3hot wait, like Linux's `pci_bridge_mutex`; taken
//! with a bounded wait, B4) > `pci.cfg` (per dword, and across each read-modify-write) > `pci.ecam` / hal's
//! 0xCF8 pair > klog. `pci.state` (the per-function record) and `pci.devices` are leaves, never held while
//! taking another. The quiesce path takes none of them unbounded (`Quiet`).

use crate::enable::{self, Change, Disabled, DmaStatus, EnableError, Saved, CMD_IO, CMD_MASTER, CMD_MEM};
use crate::{access, ConfigAccess, Loc, PciFunction, CFG_LOCK, NODES};
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Mutex, MutexGuard, SpinLock};

/// What the kernel remembers per function it enabled or bound (R1: a map, as many as there are). PC-4:
/// the binding lives here too, and the whole record is freed when the driver is removed.
#[derive(Default)]
pub(crate) struct FnState {
    /// The driver whose probe is running or bound (set by `bind`).
    pub(crate) driver: Option<&'static str>,
    /// Enabled by `enable_device` and not switched off by `disable_device` since.
    pub(crate) enabled: bool,
    /// PC-4: the binding (probing, bound or being removed), with the driver's remove hook.
    pub(crate) bind: Option<crate::bind::BindRec>,
    /// Enabled with bus master by `enable_device(true)` and not disabled since.
    pub(crate) dma: bool,
    /// Stale MSI/MSI-X enables were cleared by an enable since boot or the last `disable_device`: enables
    /// found now are the driver's own vectors and stay.
    msi_checked: bool,
    /// The header after the last enable or `save_config` (written back by `restore_config`).
    pub(crate) saved: Option<Saved>,
}

pub(crate) static STATE: SpinLock<BTreeMap<Loc, FnState>> = SpinLock::named("pci.state", BTreeMap::new());

/// Serialises `enable_device`, `save_config` and `restore_config` (two drivers enabling siblings below one
/// bridge, a driver's reset path against another's probe).
static ENABLE: Mutex<()> = Mutex::named("pci.enable", ());

/// The longest `enable_device` waits for another one (a deep bridge chain all in D3hot needs 20 ms per
/// level; anything far beyond that is a hung holder).
const ENABLE_WAIT_NS: u64 = 2_000_000_000;

pub(crate) fn lock_enable(function: &str) -> Result<MutexGuard<'static, ()>, EnableError> {
    let start = time::now_ns();
    let mut tries = 0u32;
    loop {
        if let Some(g) = ENABLE.try_lock() {
            if tries > 0 {
                klog::event!(Debug, "pci", "enable.waited", function = function, waited_ms = time::now_ns().saturating_sub(start) / 1_000_000);
            }
            return Ok(g);
        }
        tries += 1;
        let over = if time::tsc_hz() == 0 { tries > 2_000 } else { time::now_ns().saturating_sub(start) > ENABLE_WAIT_NS };
        if over {
            klog::event!(Error, "pci", "enable.timeout", function = function, waited_ms = ENABLE_WAIT_NS / 1_000_000;
                "another enable or restore held pci.enable too long");
            return Err(EnableError::Busy);
        }
        if time::tsc_hz() == 0 {
            core::hint::spin_loop();
        } else {
            sched::sleep_ns(1_000_000);
        }
    }
}

/// Configuration access taking `pci.cfg` for each dword and across each read-modify-write.
pub(crate) struct Locked;

impl ConfigAccess for Locked {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        let _g = CFG_LOCK.lock();
        access().read32(at, off)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        let _g = CFG_LOCK.lock();
        access().write32(at, off, v)
    }
    fn write_narrow(&self, at: Loc, off: u16, width: u8, v: u32) -> bool {
        let _g = CFG_LOCK.lock();
        access().write_narrow(at, off, width, v)
    }
    fn narrow_writes(&self) -> bool {
        access().narrow_writes()
    }
    fn modify32(&self, at: Loc, off: u16, f: &mut dyn FnMut(u32) -> Option<u32>) -> u32 {
        let _g = CFG_LOCK.lock();
        let c = access();
        let old = c.read32(at, off);
        if let Some(new) = f(old) {
            c.write32(at, off, new);
        }
        old
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        access().space_size(segment, bus)
    }
    fn name(&self) -> &'static str {
        access().name()
    }
}

/// 1 ms from now, or about as many spins when the clock is not running.
fn deadline_1ms() -> impl Fn() -> bool {
    let start = time::now_ns();
    let spins = Cell::new(0u32);
    move || {
        spins.set(spins.get().saturating_add(1));
        if time::tsc_hz() == 0 { spins.get() > 1 << 20 } else { time::now_ns().saturating_sub(start) > 1_000_000 }
    }
}

/// Spins on `f` until it gives `Some` or 1 ms passed.
pub(crate) fn bounded<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let over = deadline_1ms();
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if over() {
            return None;
        }
        core::hint::spin_loop();
    }
}

/// For the quiesce path (panic, Reload): no lock is waited for unbounded, since the stopped CPU may hold
/// one. `pci.cfg` is tried for 1 ms (once it was found stuck, one try per access); without it the back
/// end's own lock (`pci.ecam`, hal's 0xCF8 pair) gets its own 1 ms through the `*_bounded` accesses. An
/// access whose back-end lock stays busy is skipped and noted (`gave_up`), never forced.
pub(crate) struct Quiet {
    cfg_stuck: AtomicBool,
    gave_up: AtomicBool,
}

impl Quiet {
    pub(crate) const fn new() -> Quiet {
        Quiet { cfg_stuck: AtomicBool::new(false), gave_up: AtomicBool::new(false) }
    }

    /// An access was skipped because a lock stayed busy.
    pub(crate) fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::Relaxed)
    }

    fn with<R>(&self, fail: R, f: impl FnOnce(&dyn ConfigAccess, &dyn Fn() -> bool) -> Option<R>) -> R {
        let guard = if self.cfg_stuck.load(Ordering::Relaxed) { CFG_LOCK.try_lock() } else { bounded(|| CFG_LOCK.try_lock()) };
        if guard.is_none() {
            self.cfg_stuck.store(true, Ordering::Relaxed);
        }
        let over = deadline_1ms();
        let r = f(access(), &over);
        drop(guard);
        r.unwrap_or_else(|| {
            self.gave_up.store(true, Ordering::Relaxed);
            fail
        })
    }
}

impl ConfigAccess for Quiet {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        self.with(u32::MAX, |c, over| c.read32_bounded(at, off, over))
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        self.with(false, |c, over| c.write32_bounded(at, off, v, over))
    }
    fn modify32(&self, at: Loc, off: u16, f: &mut dyn FnMut(u32) -> Option<u32>) -> u32 {
        self.with(u32::MAX, |c, over| {
            let old = c.read32_bounded(at, off, over)?;
            if let Some(new) = f(old) {
                c.write32_bounded(at, off, new, over)?;
            }
            Some(old)
        })
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        access().space_size(segment, bus)
    }
    fn name(&self) -> &'static str {
        access().name()
    }
}

/// What `enable_device` did, for the driver's own log or test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enabled {
    /// Command register after the enable.
    pub command: u16,
    /// The function came out of a low power state (1-3).
    pub woke_from: Option<u8>,
    /// Bridges whose bus master this call switched on (root first).
    pub bridges_bus_master: Vec<Loc>,
    pub msi_cleared: bool,
}

fn sleep(ns: u64) {
    if time::tsc_hz() == 0 {
        klog::event!(Warn, "pci", "pm.no_clock", wait_ns = ns; "no calibrated clock: the power-state recovery time cannot be waited");
        return;
    }
    sched::sleep_ns(ns);
}

fn driver_of(at: Loc) -> Option<&'static str> {
    STATE.lock().get(&at).and_then(|s| s.driver)
}

/// `driver_of` for the quiesce path: `None` when `pci.state` stayed busy for 1 ms.
fn driver_of_bounded(at: Loc) -> Option<Option<&'static str>> {
    bounded(|| STATE.try_lock().map(|st| st.get(&at).and_then(|s| s.driver)))
}

pub(crate) fn driver_of_loc(at: Loc) -> Option<&'static str> {
    driver_of(at)
}

/// A command write without bus master (a driver's own `set_command`, the G0 `disable_bus_master`): the
/// function no longer counts as enabled for DMA. Never waits for the lock (quiesce paths).
pub(crate) fn note_master_off(at: Loc) {
    if let Some(mut st) = STATE.try_lock()
        && let Some(s) = st.get_mut(&at)
    {
        s.dma = false;
    }
}

/// `disable_device` switched MSI/MSI-X and bus master off: the next enable checks for stale enables again.
/// Never waits for the lock (quiesce paths).
fn note_disabled(at: Loc) {
    if let Some(mut st) = STATE.try_lock()
        && let Some(s) = st.get_mut(&at)
    {
        s.dma = false;
        s.msi_checked = false;
        s.enabled = false;
    }
}

/// Enabled by `enable_device` and not disabled since (sysfs `enable`).
pub fn is_enabled(at: Loc) -> bool {
    STATE.lock().get(&at).is_some_and(|s| s.enabled)
}

/// A record exists for this function (enabled, saved or bound); `remove` frees it.
pub fn has_record(at: Loc) -> bool {
    STATE.lock().contains_key(&at)
}

/// Enabled by `enable_device(true)` (bus master on) and not disabled since: what MSI needs (PC-3).
pub(crate) fn dma_on(at: Loc) -> bool {
    STATE.lock().get(&at).is_some_and(|s| s.enabled && s.dma)
}

/// Functions enabled with bus master and not disabled since (for checks and `hw_info`).
pub fn dma_enabled() -> Vec<Loc> {
    STATE.lock().iter().filter(|(_, s)| s.dma).map(|(l, _)| *l).collect()
}

fn hex(v: u64) -> klog::Hex {
    klog::Hex(v)
}

impl PciFunction {
    /// Switches the function on for its driver (PC-2, Linux `pci_enable_device` + `pci_set_master` +
    /// `pci_enable_bridge`): D0 (10 ms after D3hot), header written back if the wake reset it, BARs sized if
    /// the scan could not, stale MSI/MSI-X enables cleared, memory/I/O decoding as the BARs need, INTx off,
    /// and with `dma` bus master on the function **and on every bridge up to the root port** (each bridge
    /// switched is logged); bridges on the path in D1-D3hot are brought to D0 first. Call it at the start of
    /// probe, before setting up MSI vectors: stale MSI/MSI-X enables are cleared on the first call since boot
    /// or since `disable_device` only, so a later call keeps the driver's vectors. **A driver that uses MSI
    /// or MSI-X must pass `dma = true`**: an interrupt message is a memory write by the device and needs bus
    /// master on the function and every bridge above, exactly like DMA. Refused without any access for a
    /// function left untouched by its opt-in key (R8d) and for mass storage unless the NVMe driver is probing
    /// it (R8a). Idempotent: a second call changes nothing and logs at Debug only. Serialised with other
    /// enables (`pci.enable`, at most 2 s wait, then `Busy`). May sleep (20 ms per power-state change on the
    /// path); never call it with a spin lock held.
    pub fn enable_device(&self, dma: bool) -> Result<Enabled, EnableError> {
        let at = self.loc();
        let fname = format!("{at}");
        let _serial = lock_enable(&fname)?;
        let removing = STATE.lock().get(&at).and_then(|s| s.bind.as_ref()).is_some_and(|b| b.state == crate::bind::BindState::Removing);
        if removing {
            klog::event!(Warn, "pci", "enable.refused", function = fname.as_str(), reason = "removing";
                "the driver is being removed: a late enable would leave bus master on behind a freed record");
            return Err(EnableError::Busy);
        }
        let driver = driver_of(at);
        let clear_msi = !STATE.lock().get(&at).is_some_and(|s| s.msi_checked);
        let nodes = NODES.lock().clone();
        let Some(idx) = nodes.iter().position(|n| n.loc == at) else {
            klog::event!(Error, "pci", "enable.failed", function = fname.as_str(), reason = "not_scanned");
            return Err(EnableError::NotScanned);
        };
        let may_storage = driver == Some(crate::optin::STORAGE_DRIVER);
        let o = enable::enable(&Locked, &nodes, idx, dma, may_storage, clear_msi, crate::phys_limit(), &mut sleep);
        let dname = driver.unwrap_or("-");
        log_changes(&fname, dname, dma, &o.changes);
        if let Some(e) = o.error {
            let reason = format!("{e}");
            match e {
                EnableError::Untouched(_) | EnableError::Storage => {
                    klog::event!(Warn, "pci", "enable.refused", function = fname.as_str(), driver = dname, reason = reason.as_str())
                }
                _ => klog::event!(Error, "pci", "enable.failed", function = fname.as_str(), driver = dname, reason = reason.as_str()),
            }
            return Err(e);
        }
        if let Some(b) = o.sized {
            if let Some(n) = NODES.lock().iter_mut().find(|n| n.loc == at) {
                n.bars = Some(b);
            }
            for (i, bar) in b.iter().enumerate() {
                if let Some(bar) = bar {
                    klog::event!(Info, "pci", "bar.sized_after_wake", function = fname.as_str(), index = i, start = hex(bar.start()), size = hex(bar.size()));
                }
            }
        }
        {
            let mut st = STATE.lock();
            let s = st.entry(at).or_default();
            s.saved = o.saved;
            s.dma |= dma;
            s.msi_checked = true;
            s.enabled = true;
        }
        // PC-5: the I226 family loses packets in L1.2 (Linux igc does the same at probe); events in aspm_kernel.
        if crate::aspm::needs_l12_off(self.vendor, self.device) {
            let _ = crate::aspm_kernel::l12_off_locked(at);
        }
        let command = o.saved.map(|s| s.header[1] as u16).unwrap_or_default();
        if !o.changes.iter().any(|c| matches!(c, Change::Command { .. } | Change::Bridge { .. } | Change::Woke { .. })) {
            klog::event!(Debug, "pci", "enable.unchanged", function = fname.as_str(), driver = dname, dma = dma, command = hex(command as u64));
        }
        Ok(Enabled {
            command,
            woke_from: o.changes.iter().find_map(|c| if let Change::Woke { from, .. } = c { Some(*from) } else { None }),
            bridges_bus_master: o
                .changes
                .iter()
                .filter_map(|c| if let Change::Bridge { at, old, new } = c { (old & CMD_MASTER == 0 && new & CMD_MASTER != 0).then_some(*at) } else { None })
                .collect(),
            msi_cleared: o.changes.iter().any(|c| matches!(c, Change::MsiCleared | Change::MsixCleared)),
        })
    }

    /// Switches the function off for `Quiesce`, remove and failed probes: MSI and MSI-X off first, then bus
    /// master off; memory decoding and the bridges stay. PC-3: the function's interrupt set (`alloc_irqs`) is
    /// released and its vectors freed; the driver's `IrqSet` then answers `Released`. Safe on the panic and Reload paths: no klog, no
    /// heap, no sleeping, every lock tried for at most 1 ms (an access whose lock stays busy is skipped:
    /// `incomplete`). Writes nothing to an untouched function (R8d) or to mass storage not bound to the NVMe
    /// driver (R8a): `refused`. The caller logs the returned facts later if it wants
    /// (`disable_device_logged` does both).
    pub fn disable_device(&self) -> Disabled {
        self.disable_with(self.msi_cap, self.msix_cap)
    }

    /// `disable_device` with the given MSI/MSI-X offsets (0, 0: bus master only, the G0 `disable_bus_master`).
    pub(crate) fn disable_with(&self, msi: u8, msix: u8) -> Disabled {
        if self.untouched.is_some() || self.storage_not_ours() {
            return Disabled { refused: true, ..Disabled::default() };
        }
        let q = Quiet::new();
        let mut d = enable::disable(&q, self.loc(), msi, msix);
        d.incomplete = q.gave_up();
        if msi == 0 && msix == 0 {
            note_master_off(self.loc());
        } else {
            note_disabled(self.loc());
            // PC-3: the enables are off now, so the vectors can go (no heap, no klog, bounded).
            d.incomplete |= !crate::irq::release_quiet(self.loc());
        }
        d
    }

    /// R8a for the writes outside `enable_device`: class 01 is written only for the NVMe driver. Lock-free
    /// apart from a bounded try of `pci.state` (busy: treated as not ours, nothing is written).
    fn storage_not_ours(&self) -> bool {
        self.class >> 16 == 0x01 && driver_of_bounded(self.loc()) != Some(Some(crate::optin::STORAGE_DRIVER))
    }

    /// `disable_device` plus its event (normal paths: remove, probe failure; not Quiesce).
    pub fn disable_device_logged(&self) -> Disabled {
        let d = self.disable_device();
        let f = format!("{}", self.loc());
        let driver = driver_of(self.loc()).unwrap_or("-");
        if d.refused {
            klog::event!(Warn, "pci", "disable.refused", function = f.as_str(), driver = driver; "untouched (R8d) or mass storage not bound to nvme (R8a)");
        } else if d.incomplete {
            klog::event!(Error, "pci", "disable.incomplete", function = f.as_str(), driver = driver,
                bus_master_was_on = d.bus_master_was_on, msi_was_on = d.msi_was_on, msix_was_on = d.msix_was_on;
                "a configuration lock stayed busy: an access was skipped");
        } else {
            klog::event!(Info, "pci", "disable", function = f.as_str(), driver = driver,
                bus_master_was_on = d.bus_master_was_on, msi_was_on = d.msi_was_on, msix_was_on = d.msix_was_on);
        }
        d
    }

    /// Reads the 64-byte header and keeps it as the copy `restore_config` writes back (`enable_device`
    /// already saves one; call this again after changing the header).
    pub fn save_config(&self) -> Option<Saved> {
        if self.untouched.is_some() {
            return None;
        }
        let _serial = lock_enable(&format!("{}", self.loc())).ok()?;
        let pcie = NODES.lock().iter().find(|n| n.loc == self.loc()).map_or(0, enable::pcie_cap);
        let s = enable::save(&Locked, self.loc(), pcie);
        STATE.lock().entry(self.loc()).or_default().saved = Some(s);
        Some(s)
    }

    /// After a link retrain or a function reset (V1 `nicpci.asm`: BAR0/BAR1 read 0 on the running card):
    /// writes back every header dword and PCIe control word that differs from the saved copy, decoding off
    /// while the BARs move, command last; each dword logged. PC-3: then the MSI/MSI-X capability and table of
    /// the live interrupt set if the device lost them (counted as one). The command register only gains the decoding
    /// of the saved copy, and bus master only while the function is enabled for DMA (a `disable_device`
    /// since the save stays in force). Returns how many dwords were written (0: nothing was lost).
    pub fn restore_config(&self) -> Result<usize, EnableError> {
        if let Some(k) = self.untouched {
            return Err(EnableError::Untouched(k));
        }
        let fname = format!("{}", self.loc());
        if self.class >> 16 == 0x01 && driver_of(self.loc()) != Some(crate::optin::STORAGE_DRIVER) {
            klog::event!(Warn, "pci", "restore.refused", function = fname.as_str(); "mass storage is written only for the NVMe driver (R8a)");
            return Err(EnableError::Storage);
        }
        let _serial = lock_enable(&fname)?;
        let (saved, master) = STATE.lock().get(&self.loc()).map_or((None, false), |s| (s.saved, s.dma));
        let Some(saved) = saved else {
            klog::event!(Warn, "pci", "restore.no_copy", function = fname.as_str(); "nothing saved: enable_device or save_config first");
            return Err(EnableError::NotScanned);
        };
        match enable::restore(&Locked, &saved, self.class >> 8 == 0x0600, master) {
            Ok(ch) => {
                log_changes(&fname, driver_of(self.loc()).unwrap_or("-"), false, &ch);
                if !ch.is_empty() {
                    klog::event!(Warn, "pci", "config.lost", function = fname.as_str(), restored = ch.len(); "header differed from the saved copy (link retrain or reset)");
                }
                // PC-3: the MSI/MSI-X state lives in the live interrupt set, not in the saved header.
                let msi = crate::irq::restore_locked(self.loc());
                Ok(ch.len() + msi as usize)
            }
            Err(e) => {
                klog::event!(Error, "pci", "restore.failed", function = fname.as_str(), reason = format!("{e}"));
                Err(e)
            }
        }
    }

    /// The bridges from this function up to its root bus (nearest first).
    pub fn upstream_bridges(&self) -> Vec<PciFunction> {
        let nodes = NODES.lock();
        let Some(i) = nodes.iter().position(|n| n.loc == self.loc()) else { return Vec::new() };
        crate::node::ancestors(&nodes, i).into_iter().map(|p| nodes[p].function()).collect()
    }

    /// The "link up but nothing arrives" check (spec/usb.md §8): command, status and PCIe device status of
    /// the function and every bridge above it, each logged (Warn where bus master is off or an abort / UR
    /// shows). Reads only.
    pub fn dma_status(&self) -> Vec<DmaStatus> {
        let nodes = NODES.lock().clone();
        let Some(i) = nodes.iter().position(|n| n.loc == self.loc()) else { return Vec::new() };
        let v = enable::dma_status(&Locked, &nodes, i);
        for s in &v {
            let f = format!("{}", s.at);
            let sec = s.secondary_status.map(|x| x as u64).unwrap_or(0);
            let dev = s.pcie_dev_status.map(|x| x as u64).unwrap_or(0);
            if s.trouble() || !s.bus_master() {
                klog::event!(Warn, "pci", "dma.status", function = f.as_str(), command = hex(s.command as u64), status = hex(s.status as u64),
                    secondary_status = hex(sec), pcie_dev_status = hex(dev), bus_master = s.bus_master(), trouble = s.trouble());
            } else {
                klog::event!(Info, "pci", "dma.status", function = f.as_str(), command = hex(s.command as u64), status = hex(s.status as u64),
                    secondary_status = hex(sec), pcie_dev_status = hex(dev), bus_master = true, trouble = false);
            }
        }
        v
    }
}

fn bits(cmd: u16) -> String {
    let mut s = String::new();
    for (b, n) in [(CMD_MEM, "mem"), (CMD_IO, "io"), (CMD_MASTER, "bus_master"), (enable::CMD_INTX_OFF, "intx_off")] {
        if cmd & b != 0 {
            if !s.is_empty() {
                s.push('+');
            }
            s.push_str(n);
        }
    }
    if s.is_empty() { String::from("none") } else { s }
}

fn log_changes(f: &str, driver: &str, dma: bool, changes: &[Change]) {
    for c in changes {
        match *c {
            // `function` is the one changed (the device or a bridge above it), `device` the one being enabled.
            Change::Woke { at, from, waited_ns, soft_reset } => klog::event!(Info, "pci", "pm.wake", function = format!("{at}"), device = f,
                from = format!("D{from}"), waited_ms = waited_ns / 1_000_000, soft_reset = soft_reset; "to D0"),
            Change::WakeRetry { at, state } => klog::event!(Warn, "pci", "pm.retry", function = format!("{at}"), device = f, state = format!("D{state}");
                "still not in D0 after the recovery time: waiting once more"),
            Change::Restored { at, offset, was, now } => klog::event!(Info, "pci", "config.restored", function = format!("{at}"), device = f,
                offset = hex(offset as u64), was = hex(was as u64), now = hex(now as u64)),
            Change::MsiCleared => klog::event!(Info, "pci", "msi.stale_cleared", function = f, kind = "msi"; "enable left on by firmware or a previous kernel"),
            Change::MsixCleared => klog::event!(Info, "pci", "msi.stale_cleared", function = f, kind = "msix"; "enable left on by firmware or a previous kernel"),
            Change::Command { old, new } => klog::event!(Info, "pci", "enable", function = f, driver = driver, dma = dma,
                command_old = hex(old as u64), command_new = hex(new as u64), now_on = bits(new & !old)),
            Change::Bridge { at, old, new } => {
                let b = format!("{at}");
                if old & CMD_MASTER == 0 && new & CMD_MASTER != 0 {
                    // The Mini-PC's 00:02.1 line (spec/usb.md §8), like Linux's pci_enable_bridge.
                    klog::event!(Info, "pci", "bridge.bus_master", bridge = b.as_str(), device = f, command_old = hex(old as u64),
                        command_new = hex(new as u64), now_on = bits(new & !old); "bus master on for a device below");
                } else {
                    klog::event!(Info, "pci", "bridge.decode", bridge = b.as_str(), device = f, command_old = hex(old as u64),
                        command_new = hex(new as u64), now_on = bits(new & !old));
                }
            }
            Change::BarOutsideWindow { index, bridge, start } => klog::event!(Warn, "pci", "bar.outside_window", function = f, index = index,
                start = hex(start), bridge = format!("{bridge}"); "no window of this bridge forwards the BAR"),
        }
    }
}
