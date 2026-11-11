//! US-R: a device brought back in place. A wedged device gets a port reset and is set up again with the same slot,
//! the same `Device`, the same drivers (Linux `usb_reset_device` -> `usb_reset_and_verify_device`,
//! linux-6.6/drivers/usb/core/hub.c:6013): nothing above the driver notices, so a mass storage disk keeps its name
//! and its mounted file system. Its descriptors are compared afterwards; another device there, a change of speed
//! or an empty port end in an ordinary detach (Linux "re_enumerate").
//!
//! The work runs in the controller's hotplug worker, `PortSet::service`: only it touches ports, so a reset never
//! races a port change (Linux's `hub->busy_bits`) and needs no lock across the ports of a hub. The asking thread
//! waits with a bound.
//!
//! Steps of `ResetKind::Recover`: port reset (a SuperSpeed port: hot reset, then warm), `Host::reset_slot`,
//! `Host::set_address`, device descriptor, serial string and configuration descriptor compared (Linux
//! `descriptors_changed`, hub.c:5198), `Host::configure`, SET_CONFIGURATION.

use crate::device::Device;
use crate::enumerate::{read_config, SET_ADDRESS_RECOVERY_NS};
use crate::desc::{DeviceDescriptor, Endpoint};
use crate::host::{Data, Host, UsbError};
use crate::ports::Ports;
use crate::setup::{dt, Setup};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use ksync::{SpinLock, WaitQueue};

/// When a transfer's time ran out: this much running time for the controller's answer to arrive. It costs a device
/// that really hangs this much on top of its time, and only that.
pub const LOOK_NS: u64 = 250_000_000;
/// The longest an asking thread waits for the hotplug worker: both tries, each a port reset (below a second) and one
/// step that runs into its 5 s (a device answers or it does not; Address Device's command or endpoint 0's control).
/// A job still running then goes on, and a next one follows it.
pub const RESET_WAIT_NS: u64 = 13_000_000_000;
/// After a port's power came back: how long the device may take to show up again.
const POWER_UP_NS: u64 = 1_000_000_000;
const POWER_UP_POLL_NS: u64 = 10_000_000;

klog::counter!(RESETS, "usb.device_resets");
klog::counter!(RESET_FAILURES, "usb.device_reset_failures");
klog::counter!(LOST, "usb.device_reset_lost");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResetKind {
    /// The port reset and the device set up again (see the module text).
    Recover,
    /// As `Recover`, the port's power off and on first if the port can do that (a device that stays wedged through
    /// a warm reset comes back only from losing its power).
    RecoverPower,
    /// Linux's logical disconnect: detached, then the port is enumerated afresh (a new `Device`).
    Redetect,
    /// Before the machine is reset: power off and on if possible, a warm reset, nothing after it. The firmware
    /// finds a device fresh from a reset; the slot is left as it is (the controller halts next).
    ForFirmware,
}

impl ResetKind {
    /// In the order of this enum: a later one replaces an earlier one that is still queued (`Device::queue_reset`).
    const ALL: [ResetKind; 4] = [ResetKind::Recover, ResetKind::RecoverPower, ResetKind::Redetect, ResetKind::ForFirmware];
}

/// One asked-for reset.
pub struct ResetJob {
    /// Index into `ResetKind::ALL`; a stronger ask raises it while the job is queued.
    kind: AtomicU8,
    /// Nobody waits: a failure detaches the device (the worker found its port disabled under it). An asker who
    /// joins the job takes that decision back.
    unattended: AtomicBool,
    done: SpinLock<Option<Result<(), UsbError>>>,
    wq: WaitQueue,
}

impl ResetJob {
    fn new(kind: ResetKind, unattended: bool) -> Arc<ResetJob> {
        let job = ResetJob { kind: AtomicU8::new(0), unattended: AtomicBool::new(unattended), done: SpinLock::named("usb.reset_job", None), wq: WaitQueue::new() };
        job.raise(kind);
        Arc::new(job)
    }
    pub fn kind(&self) -> ResetKind {
        ResetKind::ALL[self.kind.load(Ordering::Acquire) as usize]
    }
    /// Only while the job is queued (the cell's lock), or before anyone else has it.
    fn raise(&self, kind: ResetKind) {
        self.kind.fetch_max(ResetKind::ALL.iter().position(|k| *k == kind).unwrap_or(0) as u8, Ordering::AcqRel);
    }
    /// The result, once the worker is done.
    pub fn result(&self) -> Option<Result<(), UsbError>> {
        *self.done.lock()
    }
    pub(crate) fn finish(&self, r: Result<(), UsbError>) {
        *self.done.lock() = Some(r);
        self.wq.wake_all();
    }
    /// Sleeps until the worker is done, at most `timeout_ns` (`Timeout` then; the job may still run).
    pub fn wait(&self, timeout_ns: u64) -> Result<(), UsbError> {
        self.wq.wait_until_timeout(|| self.done.lock().is_some(), timeout_ns);
        self.result().unwrap_or(Err(UsbError::Timeout))
    }
}

pub(crate) struct ResetCell {
    queued: SpinLock<Option<Arc<ResetJob>>>,
    running: AtomicBool,
}

impl Default for ResetCell {
    fn default() -> Self {
        ResetCell { queued: SpinLock::named("usb.reset_cell", None), running: AtomicBool::new(false) }
    }
}

impl ResetCell {
    /// The worker is resetting: every other user of the device gets `Busy`.
    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
    pub(crate) fn fail_queued(&self, e: UsbError) {
        if let Some(j) = self.queued.lock().take() {
            j.finish(Err(e));
        }
    }
}

/// A device's serial string as it was first read, for the comparison after a reset (same model, same moment, other
/// stick: that one must not get the mounted file system of this one).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Serial {
    lang: u16,
    text: Vec<u8>,
}

fn read_string(host: &dyn Host, slot: crate::host::HcSlot, index: u8, lang: u16) -> Result<Vec<u8>, UsbError> {
    let mut b = [0u8; 255];
    let n = host.control(slot, Setup::new(0x80, 6, (dt::STRING as u16) << 8 | index as u16, lang, 255), Data::In(&mut b))?;
    Ok(b[..n].to_vec())
}

impl Device {
    /// Reads and keeps the serial string (a driver that survives a reset calls this at its probe; a reset cannot read
    /// it any more once the device is wedged). The first language it offers, as Linux's `usb_string`. A device that has
    /// none or fails to tell has only its descriptors compared after a reset.
    pub fn remember_serial(self: &Arc<Self>) {
        if self.desc.i_serial == 0 {
            return;
        }
        let read = || -> Result<Serial, UsbError> {
            let host = self.host.as_ref();
            let langs = read_string(host, self.slot, 0, 0)?;
            let lang = match langs[..] {
                [_, _, lo, hi, ..] => u16::from_le_bytes([lo, hi]),
                _ => return Err(UsbError::Descriptor),
            };
            Ok(Serial { lang, text: read_string(host, self.slot, self.desc.i_serial, lang)? })
        };
        match read() {
            Ok(s) => *self.serial.lock() = Some(s),
            Err(e) => klog::event!(Info, "usb", "device.no_serial", device = self.path(), error = e.as_str();
                "compared by its descriptors only after a reset"),
        }
    }

    /// Asks the hotplug worker for a reset. A job still queued is shared and the stronger of the two kinds wins: a
    /// `Redetect` or `ForFirmware` asked while a `Recover` waits must not get lost in it. A job already running ends
    /// as it is and a new one follows it.
    /// `Invalid`: `Recover` was asked of a device that cannot be reset in place (`registry::survives_reset`, an
    /// alternate setting in use); the caller's way on is `Redetect`.
    pub fn start_reset(self: &Arc<Self>, kind: ResetKind) -> Result<Arc<ResetJob>, UsbError> {
        self.queue_reset(kind, false)
    }

    pub(crate) fn queue_reset(self: &Arc<Self>, kind: ResetKind, unattended: bool) -> Result<Arc<ResetJob>, UsbError> {
        if self.is_gone() {
            return Err(UsbError::Gone);
        }
        if matches!(kind, ResetKind::Recover | ResetKind::RecoverPower) && !self.can_reset_in_place() {
            return Err(UsbError::Invalid("another driver or an alternate setting is in use on this device"));
        }
        let job = {
            let mut q = self.reset.queued.lock();
            if let Some(j) = q.as_ref() {
                j.raise(kind);
                if !unattended {
                    j.unattended.store(false, Ordering::Release);
                }
                j.clone()
            } else {
                let j = ResetJob::new(kind, unattended);
                *q = Some(j.clone());
                j
            }
        };
        self.host.kick();
        Ok(job)
    }

    /// Asks for a reset and waits for it, at most `timeout_ns` (`RESET_WAIT_NS` covers a reset without the power's off
    /// and on).
    pub fn reset_device(self: &Arc<Self>, kind: ResetKind, timeout_ns: u64) -> Result<(), UsbError> {
        self.start_reset(kind)?.wait(timeout_ns)
    }

    pub(crate) fn can_reset_in_place(&self) -> bool {
        self.alts.all_default() && crate::registry::survives_reset(self.id)
    }

    pub(crate) fn reset_pending(&self) -> bool {
        self.reset.queued.lock().is_some()
    }

    pub(crate) fn take_reset(&self) -> Option<Arc<ResetJob>> {
        self.reset.queued.lock().take()
    }

    /// The time for a transfer on `ep` ran out: did it really stand still? Asks the controller, whose events are
    /// handed on with that (the waiter's timer and the event thread race when the CPUs come back from a stop),
    /// gives `wait` `LOOK_NS` of running time and asks again. True: `wait` saw the end, or the endpoint moved
    /// since `last` (the previous look, kept up to date here), so the time starts over. Linux has no such step;
    /// its URB timeouts run on a timer that also fires late, then cancel. 3.11.: that cost the Mini-PC its boot disk.
    pub fn still_moving(&self, ep: u8, last: &mut Option<crate::host::EpLook>, wait: &dyn Fn(u64) -> bool) -> bool {
        let a = self.host.look_endpoint(self.slot, ep);
        if wait(LOOK_NS) {
            return true;
        }
        let b = self.host.look_endpoint(self.slot, ep);
        let moved = |x: &Option<crate::host::EpLook>, y: &Option<crate::host::EpLook>| match (x, y) {
            (Some(x), Some(y)) => y.moved_since(x),
            _ => false,
        };
        let r = moved(last, &a) || moved(&a, &b);
        *last = b;
        r
    }
}

/// How one reset ended for the worker.
pub(crate) enum Ended {
    /// The device stays (`Ok`, or it failed and the asker decides what next).
    Kept(Result<(), UsbError>),
    /// The device has to go: nothing there, another device there, or a logical disconnect.
    Lost,
}

enum TryErr {
    Again(UsbError),
    Lost(UsbError),
}

impl From<UsbError> for TryErr {
    fn from(e: UsbError) -> TryErr {
        TryErr::Again(e)
    }
}

/// Runs a job for the device on `port` (the hotplug worker, `PortSet::service`).
pub(crate) fn run(host: &Arc<dyn Host>, ports: &dyn Ports, port: u8, dev: &Arc<Device>, job: &ResetJob) -> Ended {
    // The cell gave the job up: its kind and who waits for it stay as they are now.
    let kind = job.kind();
    if kind == ResetKind::Redetect {
        LOST.inc();
        klog::event!(Warn, "usb", "device.redetect", device = dev.path(), id = dev.id; "detaching the device and enumerating its port afresh");
        return Ended::Lost;
    }
    dev.reset.running.store(true, Ordering::Release);
    let ended = match kind {
        ResetKind::ForFirmware => Ended::Kept(for_firmware(ports, port, dev)),
        _ => recover(host, ports, port, dev, kind == ResetKind::RecoverPower),
    };
    dev.reset.running.store(false, Ordering::Release);
    match ended {
        Ended::Kept(Err(_)) if job.unattended.load(Ordering::Acquire) => Ended::Lost,
        e => e,
    }
}

fn recover(host: &Arc<dyn Host>, ports: &dyn Ports, port: u8, dev: &Arc<Device>, power: bool) -> Ended {
    let t0 = host.now_ns();
    let mut last = UsbError::Timeout;
    // Hot reset first, then the warm one (linux-6.6/drivers/usb/core/hub.c:2948 hub_port_reset; on a USB 2 port the
    // two are the same reset, tried twice as Linux's `hub_port_init` does).
    for (i, warm) in [false, true].into_iter().enumerate() {
        let on = power && i == 0;
        match one_try(host, ports, port, dev, warm, on) {
            Ok(()) => {
                RESETS.inc();
                klog::event!(Info, "usb", "device.reset", device = dev.path(), id = dev.id, port = port, warm = warm, power = on,
                    address = dev.address(), ms = host.now_ns().saturating_sub(t0) / 1_000_000; "the same device, set up again");
                return Ended::Kept(Ok(()));
            }
            Err(TryErr::Lost(e)) => {
                LOST.inc();
                klog::event!(Warn, "usb", "device.reset_lost", device = dev.path(), id = dev.id, port = port, error = e.as_str();
                    "not the same device any more; detaching it");
                return Ended::Lost;
            }
            Err(TryErr::Again(e)) => {
                klog::event!(Warn, "usb", "device.reset_try_failed", device = dev.path(), id = dev.id, port = port, warm = warm,
                    error = e.as_str());
                last = e;
            }
        }
    }
    RESET_FAILURES.inc();
    klog::event!(Error, "usb", "device.reset_failed", device = dev.path(), id = dev.id, port = port, error = last.as_str(),
        ms = host.now_ns().saturating_sub(t0) / 1_000_000);
    // `Invalid` tells the asker this device can never be reset in place (`start_reset`); this one just failed.
    Ended::Kept(Err(if matches!(last, UsbError::Invalid(_)) { UsbError::Timeout } else { last }))
}

fn one_try(host: &Arc<dyn Host>, ports: &dyn Ports, port: u8, dev: &Arc<Device>, warm: bool, power: bool) -> Result<(), TryErr> {
    bring_port_back(host, ports, port, dev, warm, power)?;
    host.reset_slot(dev.slot)?;
    let address = host.set_address(dev.slot)?;
    host.sleep_ns(SET_ADDRESS_RECOVERY_NS);
    verify_and_configure(host.as_ref(), dev)?;
    dev.address.store(address, Ordering::Release);
    Ok(())
}

/// The port's reset: afterwards it is enabled with the device's old speed and shows no change bits, so the worker's
/// next look does not take the reset for a new device.
fn bring_port_back(host: &Arc<dyn Host>, ports: &dyn Ports, port: u8, dev: &Device, warm: bool, power: bool) -> Result<(), TryErr> {
    if power && ports.power_cycle(port) {
        let end = host.now_ns().saturating_add(POWER_UP_NS);
        while !ports.status(port)?.connected && host.now_ns() < end {
            host.sleep_ns(POWER_UP_POLL_NS);
        }
    }
    if !ports.status(port)?.connected {
        return Err(TryErr::Lost(UsbError::Gone));
    }
    let speed = ports.reset_device(port, warm).map_err(|e| match e {
        UsbError::Gone => TryErr::Lost(e),
        e => TryErr::Again(e),
    })?;
    let st = ports.status(port)?;
    ports.clear_changes(port, &st);
    if !st.connected {
        return Err(TryErr::Lost(UsbError::Gone));
    }
    if speed != dev.place.speed {
        klog::event!(Warn, "usb", "device.reset_speed", device = dev.path(), was = dev.place.speed.as_str(), now = speed.as_str());
        return Err(TryErr::Lost(UsbError::Descriptor));
    }
    Ok(())
}

fn verify_and_configure(host: &dyn Host, dev: &Device) -> Result<(), TryErr> {
    let mut d = [0u8; 18];
    let n = host.control(dev.slot, Setup::get_descriptor(dt::DEVICE, 0, 18), Data::In(&mut d))?;
    let desc = DeviceDescriptor::parse(&d[..n]).map_err(|_| UsbError::Descriptor)?;
    if desc != dev.desc {
        return Err(TryErr::Lost(UsbError::Descriptor));
    }
    // A string that cannot be read is another try (the device did answer a moment ago); Linux takes it for a change.
    let serial = dev.serial.lock().clone();
    if let Some(old) = serial
        && read_string(host, dev.slot, dev.desc.i_serial, old.lang)? != old.text
    {
        klog::event!(Warn, "usb", "device.reset_serial", device = dev.path(); "another serial number on this port");
        return Err(TryErr::Lost(UsbError::Descriptor));
    }
    let Some(old) = dev.config.as_ref() else {
        return Ok(());
    };
    let config = read_config(host, dev.slot)?;
    if config != *old {
        return Err(TryErr::Lost(UsbError::Descriptor));
    }
    let eps: Vec<Endpoint> = config.defaults().flat_map(|f| f.endpoints.iter().copied()).collect();
    host.configure(dev.slot, &eps)?;
    host.control(dev.slot, Setup::set_configuration(config.value), Data::None)?;
    Ok(())
}

fn for_firmware(ports: &dyn Ports, port: u8, dev: &Device) -> Result<(), UsbError> {
    let power = ports.power_cycle(port);
    let speed = ports.reset_device(port, true);
    if let Ok(st) = ports.status(port) {
        ports.clear_changes(port, &st);
    }
    klog::event!(Info, "usb", "device.reset_for_firmware", device = dev.path(), port = port, power = power,
        ok = speed.is_ok());
    speed.map(|_| ())
}
