//! Binding (one `Function` per AudioControl interface; Linux `card.c` `snd_usb_create_streams`) and what the
//! sound side uses: the playable outputs, opening a playback or capture stream, the hardware mute, a callback
//! when outputs come or go.
//!
//! Probe (hotplug worker, may sleep): plan the streaming settings, then for each playback interface set its
//! alternate setting, 48 kHz on its endpoint and its feature units (unmuted unless the master switch is off,
//! volume 0 dB); these settings stay while the device is there, so the mixer's thread opens a stream without a
//! control transfer (`iso_pipe` only). Capture settings are set when a capture opens (thread context) and go
//! back to alternate setting 0 at close, so the microphone runs only while someone records.

use crate::plan::{self, FunctionPlan, StreamPlan};
use crate::req::{self, Ctl, RateResult};
use crate::stream::{self, Capture, Layout, Playback, Wake};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Mutex, SpinLock};
use usb_core::{Device, DeviceId, Interface, IsoOpts, UsbError};

klog::counter!(BOUND, "usb_audio.functions_bound");

pub struct Function {
    pub dev: Arc<Device>,
    pub control: u8,
    /// Playback settings in use (alternate setting and rate already set).
    pub playback: Vec<StreamPlan>,
    /// Capture settings (set at open).
    pub capture: Vec<StreamPlan>,
    /// Capture interfaces open now: one capture per interface, because closing sets it back to setting 0.
    capturing: SpinLock<Vec<u8>>,
}

static FUNCS: SpinLock<Vec<Arc<Function>>> = SpinLock::named("usb_audio.funcs", Vec::new());
static WATCHERS: SpinLock<Vec<Arc<dyn Fn() -> bool + Send + Sync>>> = SpinLock::named("usb_audio.watchers", Vec::new());
static GEN: AtomicU64 = AtomicU64::new(0);
/// Mirror of the applied hardware mute for readers; written only under `MUTE`.
static HW_MUTE: AtomicBool = AtomicBool::new(false);
/// Serialises applying the hardware mute (sleeps across control transfers) with a probe's adding of a device,
/// so every device ends up with the state last asked for. Above `usb_audio.funcs` and the caller's state lock.
static MUTE: Mutex<()> = Mutex::named("usb_audio.mute", ());

/// CONFIG keys that leave audio devices unbound: `usb_audio=off`, and US's isochronous scenario (`usb_iso_test`)
/// that streams to QEMU's usb-audio itself.
pub fn kept_off() -> Option<&'static str> {
    if pci::registry::config_value("usb_audio").is_some_and(|v| v.trim() == "off") {
        return Some("usb_audio=off");
    }
    pci::registry::opted_in("usb_iso_test").then_some("usb_iso_test=on")
}

/// Bumped whenever an output comes or goes.
pub fn generation() -> u64 {
    GEN.load(Ordering::Acquire)
}

/// `f` runs (hotplug worker) whenever outputs change; it returns false to be dropped. It must not sleep.
pub fn on_change(f: Arc<dyn Fn() -> bool + Send + Sync>) {
    WATCHERS.lock().push(f);
}

fn changed() {
    GEN.fetch_add(1, Ordering::AcqRel);
    let list: Vec<_> = WATCHERS.lock().clone();
    let mut dead = Vec::new();
    for (i, w) in list.iter().enumerate() {
        if !w() {
            dead.push(i);
        }
    }
    if !dead.is_empty() {
        let mut l = WATCHERS.lock();
        l.retain(|w| !list.iter().enumerate().any(|(i, x)| dead.contains(&i) && Arc::ptr_eq(x, w)));
    }
}

pub fn functions() -> Vec<Arc<Function>> {
    FUNCS.lock().iter().filter(|f| !f.dev.is_gone()).cloned().collect()
}

/// A playback output as the sound side names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputInfo {
    /// "usb:xhci0-1": stays the same across a replug into the same port.
    pub key: String,
    pub id: DeviceId,
    pub control: u8,
    pub iface: u8,
    pub channels: u8,
    pub terminal_type: u16,
}

pub fn key(dev: &Device) -> String {
    format!("usb:{}", dev.path())
}

/// Every playback output of the devices present, stereo ones first, then by device and interface.
pub fn outputs() -> Vec<OutputInfo> {
    let mut v: Vec<OutputInfo> = functions()
        .iter()
        .flat_map(|f| f.playback.iter().map(move |s| OutputInfo { key: key(&f.dev), id: f.dev.id, control: f.control, iface: s.iface,
            channels: s.channels, terminal_type: s.terminal_type }))
        .collect();
    v.sort_by_key(|o| (o.channels != 2, o.id, o.iface));
    v
}

fn find(key_: &str, iface: u8, playback: bool) -> Option<(Arc<Function>, StreamPlan)> {
    functions().into_iter().find_map(|f| {
        if key(&f.dev) != key_ {
            return None;
        }
        let list = if playback { &f.playback } else { &f.capture };
        let s = list.iter().find(|s| s.iface == iface)?.clone();
        Some((f, s))
    })
}

fn layout(s: &StreamPlan) -> Layout {
    Layout { channels: s.channels, subframe: s.subframe }
}

/// Opens playback on output (`key`, `iface`): `ring` frames, the bus window from the mixer's `period`. No
/// control transfer (the setting was made at probe): callable from the mixer's thread.
pub fn open_playback(key_: &str, iface: u8, ring: usize, period: u32, wake: Wake) -> Result<Playback, UsbError> {
    let (f, s) = find(key_, iface, true).ok_or(UsbError::Gone)?;
    let interval = usb_core::iso::service_interval_ns(f.dev.speed(), &s.ep);
    let (window_ns, req) = stream::sizing(period, interval);
    let pipe = f.dev.iso_pipe(s.ep.address, IsoOpts { window_ns })?;
    let dev = f.dev.clone();
    let p = Playback::open(Box::new(pipe), &|n| dev.alloc_buffer(n), layout(&s), ring, req, wake)?;
    klog::event!(Info, "usb_audio", "uac.play_open", device = f.dev.path(), interface = iface, ring = p.ring_frames(), window_us = window_ns / 1000,
        packets_per_request = req, channels = s.channels, bytes = s.subframe);
    Ok(p)
}

/// The capture inputs (microphones) of the devices present.
pub fn inputs() -> Vec<OutputInfo> {
    functions()
        .iter()
        .flat_map(|f| f.capture.iter().map(move |s| OutputInfo { key: key(&f.dev), id: f.dev.id, control: f.control, iface: s.iface,
            channels: s.channels, terminal_type: s.terminal_type }))
        .collect()
}

/// The claim of one capture interface; released when dropped (also on an error path or a handle never closed).
struct Claim {
    f: Arc<Function>,
    iface: u8,
}

impl Claim {
    fn take(f: &Arc<Function>, iface: u8) -> Result<Claim, UsbError> {
        let mut l = f.capturing.lock();
        if l.contains(&iface) {
            return Err(UsbError::Busy);
        }
        l.push(iface);
        Ok(Claim { f: f.clone(), iface })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.f.capturing.lock().retain(|&i| i != self.iface);
    }
}

/// An open capture: closing sets the interface back to alternate setting 0 (thread context).
pub struct CaptureHandle {
    pub cap: Capture,
    claim: Claim,
}

impl CaptureHandle {
    pub fn close(self) {
        self.cap.close();
        let (dev, iface) = (&self.claim.f.dev, self.claim.iface);
        if !dev.is_gone() {
            let _ = Ctl::set_interface(dev, iface, 0);
        }
        klog::event!(Info, "usb_audio", "uac.capture_close", device = dev.path(), interface = iface, frames = self.cap.counts().0);
    }
}

/// Opens and starts capture on input (`key`, `iface`): sets its alternate setting, 48 kHz and its feature units.
/// Sleeps (control transfers): thread context, not the mixer's thread. A second open of the same input is `Busy`.
pub fn open_capture(key_: &str, iface: u8, ring: usize, wake: Option<Wake>) -> Result<CaptureHandle, UsbError> {
    let (f, s) = find(key_, iface, false).ok_or(UsbError::Gone)?;
    let claim = Claim::take(&f, iface).inspect_err(|_| {
        klog::event!(Info, "usb_audio", "uac.capture_busy", device = f.dev.path(), interface = iface);
    })?;
    prepare(&f.dev, &s, false)?;
    let interval = usb_core::iso::service_interval_ns(f.dev.speed(), &s.ep);
    let (window_ns, req) = stream::sizing(480, interval);
    let opened = f.dev.iso_pipe(s.ep.address, IsoOpts { window_ns }).and_then(|pipe| {
        let dev = f.dev.clone();
        Capture::open(Box::new(pipe), &|n| dev.alloc_buffer(n), layout(&s), ring, req, wake)
    });
    let cap = match opened.and_then(|c| c.start().map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            let _ = Ctl::set_interface(&f.dev, iface, 0);
            return Err(e);
        }
    };
    klog::event!(Info, "usb_audio", "uac.capture_open", device = f.dev.path(), interface = iface, channels = s.channels);
    Ok(CaptureHandle { cap, claim })
}

/// Alternate setting, rate, feature units of one stream.
fn prepare(dev: &Arc<Device>, s: &StreamPlan, mute: bool) -> Result<(), UsbError> {
    Ctl::set_interface(dev, s.iface, s.alt).inspect_err(|e| {
        klog::event!(Warn, "usb_audio", "uac.alt_failed", device = dev.path(), interface = s.iface, alt = s.alt, error = e.as_str());
    })?;
    match req::set_rate(dev, s) {
        Ok(r) => {
            let rate = match r {
                RateResult::Set(x) => x as u64,
                _ => 0,
            };
            if matches!(r, RateResult::Set(x) if x != plan::RATE) {
                klog::event!(Warn, "usb_audio", "uac.rate_mismatch", device = dev.path(), endpoint = klog::Hex(s.ep.address as u64), read = rate);
            }
            klog::event!(Debug, "usb_audio", "uac.rate", device = dev.path(), endpoint = klog::Hex(s.ep.address as u64), result = format!("{r:?}").as_str());
        }
        // A device with 48 kHz as its only rate plays it anyway (Linux would give up here; QEMU's and cheap
        // dongles stall the request).
        Err(e) if s.rate_only => {
            klog::event!(Warn, "usb_audio", "uac.rate_refused", device = dev.path(), endpoint = klog::Hex(s.ep.address as u64), error = e.as_str();
                "48 kHz is its only rate: going on");
        }
        Err(e) => {
            klog::event!(Warn, "usb_audio", "uac.rate_failed", device = dev.path(), endpoint = klog::Hex(s.ep.address as u64), error = e.as_str());
            let _ = Ctl::set_interface(dev, s.iface, 0);
            return Err(e);
        }
    }
    let r = req::apply_features(dev, s, mute);
    klog::event!(Debug, "usb_audio", "uac.features", device = dev.path(), interface = s.iface, mutes = r.mutes, volumes = r.volumes,
        failed = r.failed, level_db256 = r.level as i64);
    Ok(())
}

/// Hardware mute of every playback path (the master switch off on both channels). Thread context (sleeps).
/// `wanted` is read under `usb_audio.mute`, from the caller's own state (not a value read earlier), so of
/// concurrent callers the last one applies the newest state; it must not sleep.
pub fn set_hw_mute(wanted: &dyn Fn() -> bool) {
    let _g = MUTE.lock();
    let mute = wanted();
    if HW_MUTE.load(Ordering::SeqCst) == mute {
        return;
    }
    HW_MUTE.store(mute, Ordering::SeqCst);
    for f in functions() {
        apply_mute(&f, mute);
    }
    klog::event!(Info, "usb_audio", "uac.hw_mute", mute = mute);
}

fn apply_mute(f: &Function, mute: bool) {
    for s in &f.playback {
        if let Err(e) = req::set_mute(&f.dev, s, mute) {
            klog::event!(Warn, "usb_audio", "uac.mute_failed", device = f.dev.path(), interface = s.iface, error = e.as_str());
        }
    }
}

pub fn hw_mute() -> bool {
    HW_MUTE.load(Ordering::SeqCst)
}

fn log_plan(dev: &Device, p: &FunctionPlan) {
    for s in &p.skipped {
        klog::event!(Debug, "usb_audio", "uac.alt_skipped", device = dev.path(), interface = s.iface, alt = s.alt, why = s.why);
    }
    if p.bad_descriptors > 0 {
        klog::event!(Warn, "usb_audio", "uac.bad_descriptors", device = dev.path(), control = p.control, count = p.bad_descriptors);
    }
}

pub fn probe(dev: &Arc<Device>, f: &Interface) -> Result<(), String> {
    if let Some(k) = kept_off() {
        return Err(format!("left unbound while {k}"));
    }
    let cfg = dev.config.as_ref().ok_or("no configuration")?;
    let p = plan::plan(cfg, f.number);
    log_plan(dev, &p);
    let mut playback = Vec::new();
    let prepared_mute = HW_MUTE.load(Ordering::SeqCst);
    for s in FunctionPlan::best(&p.playback) {
        if s.sync.wants_feedback() {
            klog::event!(Warn, "usb_audio", "uac.no_feedback", device = dev.path(), interface = s.iface;
                "asynchronous endpoint: played at the nominal 48 kHz without reading its feedback");
        }
        if prepare(dev, &s, prepared_mute).is_ok() {
            playback.push(s);
        }
    }
    let capture = FunctionPlan::best(&p.capture);
    if playback.is_empty() && capture.is_empty() {
        return Err(format!("no usable 48 kHz PCM setting (skipped {})", p.skipped.len()));
    }
    for s in playback.iter().chain(capture.iter()) {
        klog::event!(Info, "usb_audio", "uac.stream", device = dev.path(), control = f.number, interface = s.iface, alt = s.alt,
            endpoint = klog::Hex(s.ep.address as u64), dir = if s.playback { "playback" } else { "capture" }, channels = s.channels,
            bits = s.bits, max_packet = s.ep.max_packet(), sync = format!("{:?}", s.sync).as_str(), rate_control = s.rate_control,
            terminal = klog::Hex(s.terminal_type as u64), features = s.features.len());
    }
    let func = Arc::new(Function { dev: dev.clone(), control: f.number, playback, capture, capturing: SpinLock::named("usb_audio.capturing", Vec::new()) });
    // A master switch changed (either way) between `prepare` and the push reached no device: under `MUTE` a
    // `set_hw_mute` either finished before (its state is read here) or starts after (it finds this device).
    let mute = {
        let _g = MUTE.lock();
        FUNCS.lock().push(func.clone());
        let mute = HW_MUTE.load(Ordering::SeqCst);
        if mute != prepared_mute {
            apply_mute(&func, mute);
        }
        mute
    };
    BOUND.inc();
    klog::event!(Info, "usb_audio", "uac.bound", device = dev.path(), vendor = klog::Hex(dev.vendor() as u64), product = klog::Hex(dev.product() as u64),
        control = f.number, playback = func.playback.len(), capture = func.capture.len(), mute = mute);
    changed();
    Ok(())
}

pub fn disconnect(dev: &Arc<Device>, iface: u8) {
    let gone: Vec<Arc<Function>> = {
        let mut l = FUNCS.lock();
        let v = l.iter().filter(|f| f.dev.id == dev.id && f.control == iface).cloned().collect();
        l.retain(|f| !(f.dev.id == dev.id && f.control == iface));
        v
    };
    if gone.is_empty() {
        return;
    }
    // Unbound with the device still there (driver unregistered): its interfaces back to setting 0.
    if !dev.is_gone() {
        for f in &gone {
            for s in &f.playback {
                let _ = Ctl::set_interface(dev, s.iface, 0);
            }
        }
    }
    klog::event!(Info, "usb_audio", "uac.unbound", device = dev.path(), control = iface, gone = dev.is_gone());
    changed();
}
