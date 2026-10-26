//! HD Audio controllers. Owner: SN. Item SN-2: the Mini-PC's FCH HD Audio (1022:15E3, ALC269 analog codec),
//! the GPU's HDMI/DP audio controller (1002:1640, enumerated only until SN-5's [`hdmi::hdmi_on`] turns the display
//! side's audio on and makes it stream) and QEMU's intel-hda
//! (8086:2668, ich9 8086:293E) with hda-duplex, behind `devmodel::audio::AudioDevice`, `Quiesce` and a richer
//! API for SN-3's mixer ([`Hda`] / [`Controller`]: open on an output, format, start/stop, period callback,
//! position, delay, jack events).
//!
//! Layout: `regs` (registers), `hw` (what the logic needs from the machine), `link` (reset, CORB/RIRB,
//! immediate commands), `stream` (BDL, descriptor, LPIB position), `format` (format words), `ctrl::Controller`
//! (bring-up, codecs through `hdacodec`, streams, jack events, Quiesce; logs), `mmio_hw` (the kernel's `Hw`),
//! `fake` (tests only: a register-level controller with codecs from `hdacodec` dumps), `signal` (the WAV-check
//! signal), `ktests`.
//!
//! Each controller has a service thread `hda/<n>` (KernelRt, V2-PLAN §5): it sleeps on its MSI (period
//! completions, RIRB) with a deadline of a quarter period (100 ms idle), then reads positions, calls period
//! callbacks, handles unsolicited plug events and polls the jacks that cannot send them. Without MSI (CONFIG
//! `hda_irq=off`, or none could be had) the same thread polls on its deadline.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod ctrl;
/// The register-level fake, also for the mixer's host tests in `snd` (feature `fake`, dev-dependency only).
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod format;
pub mod hdmi;
pub mod hw;
mod ktests;
mod ktests_hdmi;
pub mod link;
mod mmio_hw;
pub mod regs;
pub mod signal;
pub mod stream;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_capture;
#[cfg(test)]
mod tests_hdmi;

pub use ctrl::{Config, Controller, DP_AUDIO_PIN, HdaError, InputInfo, JackFn, Kind, OutputId, OutputInfo, PeriodFn, State, StreamId};
pub use mmio_hw::MmioHw;

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use devmodel::audio::{AudioDevice, Output as Port, PcmFormat};
use devmodel::{DevError, Quiesce};
use hdacodec::path::OutputKind;
use ksync::SpinLock;

static MATCHES: [pci::Match; 5] = [
    pci::Match::Id { vendor: regs::VENDOR_INTEL, device: regs::DEV_ICH6 },
    pci::Match::Id { vendor: regs::VENDOR_INTEL, device: regs::DEV_ICH9 },
    pci::Match::Id { vendor: regs::VENDOR_AMD, device: regs::DEV_AMD_FCH },
    pci::Match::Id { vendor: regs::VENDOR_ATI, device: regs::DEV_ATI_HDMI },
    // Every other HD Audio function (class 04/03), as Linux's class entries; the vendor picks the quirks.
    pci::Match::Class { class: 0x04_03_00, mask: 0xFF_FF_00 },
];

pci::pci_driver!(pci::DriverEntry { name: "hda", matches: &MATCHES, opt_in_key: None, probe });

/// A controller in the kernel with its interrupt set.
pub struct Hda {
    pub ctrl: Controller<MmioHw>,
    /// PC-3's MSI set (None: polled). Dropped outside the lock (dropping may sleep).
    irqs: SpinLock<Option<pci::IrqSet>>,
    handle: SpinLock<Option<pci::IrqHandle>>,
    /// The service thread runs (the GPU controller gets it when it first streams, SN-5).
    thread: core::sync::atomic::AtomicBool,
}

static DEVICES: SpinLock<Vec<Arc<Hda>>> = SpinLock::named("hda.devices", Vec::new());

/// Every controller this driver kept, in probe order (`hda0`, `hda1`, ...).
pub fn controllers() -> Vec<Arc<Hda>> {
    DEVICES.lock().clone()
}

/// The controllers as `AudioDevice`s (SN-3, SN-4).
pub fn audio_devices() -> Vec<Arc<dyn AudioDevice>> {
    controllers().into_iter().map(|d| d as Arc<dyn AudioDevice>).collect()
}

pub fn is_hda(f: &pci::PciFunction) -> bool {
    MATCHES.iter().any(|m| m.hits(f.vendor, f.device, f.class))
}

fn config() -> Config {
    Config { force_no_snoop: pci::registry::config_value("hda_snoop").is_some_and(|v| v.trim() == "off") }
}

fn irqs_wanted() -> bool {
    !pci::registry::config_value("hda_irq").is_some_and(|v| v.trim() == "off")
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    // MSI and bus master off before anything else: a Reload may have left streams running.
    let _ = f.disable_device();
    let kind = Kind::of(f.vendor, f.device);
    let function = format!("{}", f.addr);
    klog::event!(Info, "snd", "hda.probe", function = function.as_str(), kind = kind.name(), vendor = klog::Hex(f.vendor as u64),
        device = klog::Hex(f.device as u64), enumerate_only = kind.enumerate_only());
    let hw = MmioHw::map(f).map_err(|e| {
        klog::event!(Error, "snd", "hda.probe_failed", function = function.as_str(), error = e.as_str());
        e
    })?;
    // Index and entry in one step: two probes at once get different names.
    let dev = {
        let mut devs = DEVICES.lock();
        let index = devs.len();
        let dev = Arc::new(Hda {
            ctrl: Controller::new(format!("hda{index}"), kind, hw, config()),
            irqs: SpinLock::named("hda.irqs", None),
            handle: SpinLock::named("hda.handle", None),
            thread: core::sync::atomic::AtomicBool::new(false),
        });
        devs.push(dev.clone());
        dev
    };
    // Quiesce from here on, also for a controller that failed: its DMA may have been started by a Reload.
    devmodel::register_quiesce(dev.clone());
    let up = dev.ctrl.bring_up();
    if dev.ctrl.read_only() {
        return up;
    }
    if up.is_ok() {
        dev.setup_irqs();
    }
    // The service thread also for a failed controller: a later restart() needs it (it idles while not up).
    dev.ensure_thread();
    up
}

fn service_loop(dev: Arc<Hda>) {
    loop {
        let t = dev.ctrl.service_interval_ns();
        let h = dev.handle.lock().clone();
        match h {
            Some(h) => {
                h.wait(t);
            }
            None => sched::sleep_ns(t),
        }
        dev.ctrl.service();
    }
}

impl Hda {
    /// Starts the service thread `hda/<n>` once.
    fn ensure_thread(self: &Arc<Self>) {
        if self.thread.swap(true, core::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let d = self.clone();
        let name = format!("hda/{}", self.ctrl.name().trim_start_matches("hda"));
        match sched::spawn(&name, sched::Class::KernelRt, move || service_loop(d)) {
            Ok(_) => klog::event!(Info, "snd", "hda.thread", ctrl = self.ctrl.name(), thread = name.as_str(), class = "kernel_rt"),
            Err(e) => {
                self.thread.store(false, core::sync::atomic::Ordering::Release);
                klog::event!(Error, "snd", "hda.thread_failed", ctrl = self.ctrl.name(), error = alloc::format!("{e:?}");
                    "no service thread: no period callbacks, no jack events")
            }
        }
    }

    /// MSI with one vector whose hard-IRQ hook acknowledges the controller; polled if none.
    fn setup_irqs(self: &Arc<Self>) {
        let old = self.irqs.lock().take();
        *self.handle.lock() = None;
        drop(old);
        if !irqs_wanted() {
            self.ctrl.set_irq_driven(false);
            return;
        }
        let w: Weak<Hda> = Arc::downgrade(self);
        let hook: pci::IrqHook = Arc::new(move |_| {
            if let Some(d) = w.upgrade() {
                d.ctrl.irq_ack();
            }
        });
        let req = pci::IrqRequest::new("hda", 1, 1).modes(false, true, false).hook(hook);
        match self.ctrl.hw.pci().alloc_irqs(&req) {
            Ok(set) => {
                *self.handle.lock() = set.handle(0);
                *self.irqs.lock() = Some(set);
                self.ctrl.set_irq_driven(true);
            }
            Err(e) => {
                klog::event!(Warn, "snd", "hda.irq_fallback", ctrl = self.ctrl.name(), error = alloc::format!("{e}"); "no MSI: polled");
                self.ctrl.set_irq_driven(false);
            }
        }
    }

    /// Brings a quiesced or failed controller up again (ktests; a driver restart). Open streams are gone.
    pub fn restart(self: &Arc<Self>) -> Result<(), String> {
        let r = self.ctrl.bring_up();
        if self.ctrl.read_only() {
            // The GPU controller back to enumerate-only: no MSI (bus master is off).
            let old = self.irqs.lock().take();
            *self.handle.lock() = None;
            drop(old);
            return r;
        }
        r?;
        self.setup_irqs();
        self.ensure_thread();
        Ok(())
    }

    /// The MSI set's mode and deliveries (ktests).
    pub fn irq_info(&self) -> Option<(pci::IrqMode, u64)> {
        let g = self.irqs.lock();
        g.as_ref().map(|s| (s.mode(), s.count(0)))
    }

    /// The output `AudioDevice::open_stream` picks for a port: a plugged headphone, else line out, else speaker.
    pub fn pick(&self, port: Port) -> Option<OutputId> {
        let outs: Vec<OutputInfo> = self.ctrl.outputs().into_iter().filter(|o| o.port == port && o.playable).collect();
        let rank = |o: &OutputInfo| match (o.kind, o.plugged) {
            (OutputKind::Headphone, Some(true)) => 0,
            (OutputKind::LineOut, _) => 1,
            (OutputKind::Speaker, _) => 2,
            _ => 3,
        };
        outs.iter().min_by_key(|o| rank(o)).map(|o| o.id)
    }
}

impl AudioDevice for Hda {
    fn name(&self) -> &str {
        self.ctrl.name()
    }
    fn outputs(&self) -> Vec<Port> {
        let mut v: Vec<Port> = Vec::new();
        for o in self.ctrl.outputs() {
            if !v.contains(&o.port) {
                v.push(o.port);
            }
        }
        v
    }
    fn formats(&self) -> Vec<PcmFormat> {
        self.pick(Port::Jack).and_then(|id| self.ctrl.outputs().into_iter().find(|o| o.id == id)).map_or_else(Vec::new, |o| o.formats)
    }
    fn open_stream(&self, out: Port, fmt: PcmFormat, ring_frames: u32) -> Result<u32, DevError> {
        let id = self.pick(out).ok_or(if out == Port::Hdmi { DevError::Refused } else { DevError::InvalidArgument })?;
        let sid = self.ctrl.open(id, fmt, ring_frames, ring_frames / 4).map_err(HdaError::dev)?;
        self.ctrl.start(sid).map_err(HdaError::dev)?;
        Ok(sid)
    }
    fn write(&self, stream: u32, data: &[u8]) -> Result<usize, DevError> {
        self.ctrl.write(stream, data).map_err(HdaError::dev)
    }
    fn position(&self, stream: u32) -> u64 {
        self.ctrl.position(stream).unwrap_or(0)
    }
    fn close_stream(&self, stream: u32) {
        let _ = self.ctrl.close(stream);
    }
    fn jack_present(&self, out: Port) -> Option<bool> {
        let states: Vec<Option<bool>> = self.ctrl.outputs().into_iter().filter(|o| o.port == out).map(|o| o.plugged).collect();
        if states.contains(&Some(true)) {
            Some(true)
        } else if states.contains(&Some(false)) {
            Some(false)
        } else {
            None
        }
    }
}

impl Quiesce for Hda {
    fn name(&self) -> &str {
        self.ctrl.name()
    }
    fn quiesce(&self) {
        self.ctrl.quiesce();
        // The display side after the GPU controller's DMA has stopped (Linux's off order: samples, then packets).
        if self.ctrl.kind() == Kind::AtiHdmi {
            hdmi::quiesce_display();
        }
    }
}
