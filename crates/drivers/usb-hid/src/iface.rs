//! One bound HID interface: probe (descriptor, protocol, input devices, the interrupt pipe), the report path
//! (pipe sink -> hidparse -> `console::evdev` input devices) and the unplug path (every held key released).
//!
//! Probe follows Linux usbhid (linux-6.6/drivers/hid/usbhid/hid-core.c:979-1058 `usbhid_parse`, 1060-1192
//! `usbhid_start`): SET_IDLE 0, the report descriptor read with up to four tries, hid-generic's input devices.
//! Unlike Linux, an interface that supports the boot protocol falls back to it (V1's path, spec usb.md §4.2)
//! when the report descriptor cannot be read, does not parse or maps to no input at all. Like Linux, the report
//! protocol is never requested: every device comes out of its bus reset in the report protocol (HID 1.11 §7.2.6),
//! and some receivers misbehave on an unexpected SET_PROTOCOL.
//!
//! Gamepads (IN-2b): a Joystick or Gamepad application gets its endpoint polled every 1 ms whatever bInterval
//! says (`class::fast_interval`, V1 `pad_fast`; CONFIG `usbhid_pad_binterval=on` keeps the descriptor's), and
//! Sixaxis-family pads (DualShock 3) are woken with GET_REPORT of feature reports 0xF2 and 0xF5 before their
//! pipe starts (Linux hid-sony). String descriptors are never read: Georg's DragonRise answers an index it does
//! not name with 255 bytes of its memory (spec usb.md §2).
//!
//! A pipe the controller gives up (50 errors in a row) releases the held keys at once and is started again by
//! the worker after 1, 2, 4 ... 32 s; after `PIPE_RESTARTS` failed restarts the interface is marked dead (the MCP
//! tool shows it) until it is plugged in again.

use crate::class::{self, HidDescriptor};
use crate::leds::Leds;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use console::evdev::{self, AbsInfo, Caps, DeviceInfo, InputDevice};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use hidparse::{Event, InputError, InputMap, InputState, ReportDescriptor};
use ksync::SpinLock;
use usb_core::{Device, DeviceId, EpKind, Interface, PipeEvent, PipeHandle, PipeOpts, UsbError};

klog::counter!(REPORTS, "input.hid.reports");
klog::counter!(BAD_REPORTS, "input.hid.bad_reports");
klog::counter!(PIPE_ERRORS, "input.hid.pipe_errors");

/// Transfers kept queued per interrupt IN pipe (US-3's recommendation; 1 ms gamepads want the depth).
const PIPE_DEPTH: usize = 4;
/// Tries for the report descriptor (Linux `hid_get_class_descriptor`).
const DESC_TRIES: usize = 4;
/// Restarts of a given-up pipe in a row before the interface counts as dead (a retry bound, not a count of
/// things); the delay doubles from 1 s.
pub const PIPE_RESTARTS: u32 = 6;
/// A restarted pipe that ran this long without being given up again starts with a fresh restart budget.
const PIPE_HEALTHY_NS: u64 = 60_000_000_000;

/// State of an interface's interrupt IN pipe (`HidIface::pipe_state`).
pub const PIPE_NONE: u8 = 0;
pub const PIPE_RUNNING: u8 = 1;
pub const PIPE_RESTARTING: u8 = 2;
pub const PIPE_DEAD: u8 = 3;

pub fn pipe_state_name(s: u8) -> &'static str {
    match s {
        PIPE_RUNNING => "running",
        PIPE_RESTARTING => "restarting",
        PIPE_DEAD => "dead",
        _ => "none",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Report,
    Boot,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Report => "report",
            Protocol::Boot => "boot",
        }
    }
}

struct St {
    input: InputState,
    gone: bool,
    events: Vec<Event>,
    pending: Vec<(u16, u16, i32)>,
}

pub struct HidIface {
    pub dev: Arc<Device>,
    pub iface: u8,
    pub protocol: Protocol,
    pub desc: ReportDescriptor,
    pub map: InputMap,
    /// Input device of each group of `map` (None for an empty group, which Linux does not register).
    pub inputs: Vec<Option<Arc<InputDevice>>>,
    pub ep_in: u8,
    pub out_ep: Option<u8>,
    /// The IN endpoint's bInterval as its descriptor says it.
    pub b_interval: u8,
    /// A Joystick or Gamepad application is among the interface's input devices.
    pub pad: bool,
    /// The bInterval the pipe asks for instead of the descriptor's (gamepads: 1 ms), None to keep it.
    pub poll_want: Option<u8>,
    /// Bytes per interrupt transfer (the longest report, at least the packet size).
    pub report_len: usize,
    pub leds: Leds,
    st: SpinLock<St>,
    pipe: SpinLock<Option<PipeHandle>>,
    stopping: AtomicBool,
    /// `PIPE_*`.
    pub pipe_state: AtomicU8,
    /// Restarts in a row of a given-up pipe, when the next one is due, when the last one ran.
    pub restarts: AtomicU32,
    restart_at: AtomicU64,
    restarted_ns: AtomicU64,
    pub reports: AtomicU64,
    pub bad_reports: AtomicU64,
    pub errors: AtomicU64,
}

static IFACES: SpinLock<Vec<Arc<HidIface>>> = SpinLock::named("hid.ifaces", Vec::new());

/// Interfaces usb-hid refused, with the reason (the MCP tool and `in.hid.devices` show them; the registry logs
/// `driver.probe_failed`). Entries of devices that went are dropped when read.
struct Refusal {
    dev: Weak<Device>,
    id: DeviceId,
    iface: u8,
    reason: String,
}
static REFUSED: SpinLock<Vec<Refusal>> = SpinLock::named("hid.refused", Vec::new());

fn note_refusal(dev: &Arc<Device>, iface: u8, reason: Option<&str>) {
    let mut l = REFUSED.lock();
    l.retain(|r| !(r.id == dev.id && r.iface == iface) && r.dev.upgrade().is_some_and(|d| !d.is_gone()));
    if let Some(reason) = reason {
        l.push(Refusal { dev: Arc::downgrade(dev), id: dev.id, iface, reason: String::from(reason) });
    }
}

/// Interfaces of present devices usb-hid refused, with the reason.
pub fn refusals() -> Vec<(Arc<Device>, u8, String)> {
    REFUSED.lock().iter().filter_map(|r| Some((r.dev.upgrade().filter(|d| !d.is_gone())?, r.iface, r.reason.clone()))).collect()
}

/// Every bound HID interface.
pub fn all() -> Vec<Arc<HidIface>> {
    IFACES.lock().clone()
}

/// Takes the bound interface out of the list (unplug, driver removed).
pub fn take(dev: &Device, iface: u8) -> Option<Arc<HidIface>> {
    let mut l = IFACES.lock();
    let at = l.iter().position(|h| h.dev.id == dev.id && h.iface == iface)?;
    Some(l.remove(at))
}

/// CONFIG keys that leave HID devices unbound: `usbhid_raw`, and US's scenarios that drive HID devices with
/// their own test drivers or raw pipes (`usb_xfer_test` for 40-xfer.sh, `usb_unplug_test` for 30-unplug.sh).
pub fn left_raw() -> Option<&'static str> {
    ["usbhid_raw", "usb_xfer_test", "usb_unplug_test"].into_iter().find(|k| pci::registry::opted_in(k))
}

pub fn probe(dev: &Arc<Device>, f: &Interface) -> Result<Arc<HidIface>, String> {
    if let Some(key) = left_raw() {
        return Err(format!("{key}=on: HID devices stay unbound"));
    }
    let r = probe_inner(dev, f);
    note_refusal(dev, f.number, r.as_ref().err().map(String::as_str));
    r
}

fn probe_inner(dev: &Arc<Device>, f: &Interface) -> Result<Arc<HidIface>, String> {
    let ep = f
        .endpoints
        .iter()
        .find(|e| e.is_in() && e.kind() == EpKind::Interrupt)
        .ok_or_else(|| String::from("no interrupt IN endpoint"))?;
    let out_ep = f.endpoints.iter().find(|e| !e.is_in() && e.kind() == EpKind::Interrupt).map(|e| e.address);
    let boot_kind = (f.subclass == class::SUBCLASS_BOOT)
        .then_some(f.protocol)
        .filter(|&p| p == class::PROTOCOL_KEYBOARD || p == class::PROTOCOL_MOUSE);
    let hid = class::hid_descriptor(&f.extra);
    // Linux sends SET_IDLE(0) before reading the report descriptor; many devices stall it, that is fine.
    if let Err(e) = dev.control_out(class::set_idle(f.number, 0), &[]) {
        klog::event!(Debug, "input", "hid.set_idle_refused", device = dev.path(), interface = f.number, error = e.as_str());
    }
    let parsed = match hid {
        Some(h) => read_and_map(dev, f.number, h),
        None => Err(String::from("no HID descriptor, or its report descriptor size is 0 or above 4096")),
    };
    // A descriptor without any input application (a receiver's vendor interface, HID++ on Logitech's
    // c52b): Linux hid-generic binds it with no input device (hidraw only, hid-input.c:2183-2193). So does
    // V2, without polling it: nothing would read the reports until hidraw exists (IN-5).
    let parsed = match parsed {
        Ok((desc, map)) if map.groups.iter().all(|g| g.is_empty()) => match boot_kind {
            Some(_) => Err(String::from("report descriptor maps to no input")),
            None => Ok((desc, map)),
        },
        other => other,
    };
    let (protocol, desc, map) = match (parsed, boot_kind) {
        (Ok((desc, map)), _) => (Protocol::Report, desc, map),
        (Err(why), Some(kind)) => {
            dev.control_out(class::set_protocol(f.number, class::PROTOCOL_BOOT), &[])
                .map_err(|e| format!("{why}; boot protocol refused too: {}", e.as_str()))?;
            let desc = if kind == class::PROTOCOL_KEYBOARD { hidparse::boot::boot_keyboard() } else { hidparse::boot::boot_mouse() };
            let map = InputMap::new(&desc);
            klog::event!(Warn, "input", "hid.boot_fallback", device = dev.path(), interface = f.number, reason = why.as_str();
                "report protocol not usable, boot protocol instead");
            (Protocol::Boot, desc, map)
        }
        (Err(why), None) => return Err(why),
    };
    if class::needs_sixaxis_wake(dev.vendor(), dev.product()) {
        sixaxis_wake(dev, f.number);
    }
    let report_len = class::max_input_report(&desc).max(ep.max_packet() as usize).min(class::MAX_REPORT_BYTES);
    let state = class::state_bytes(&desc, report_len);
    if state > class::MAX_STATE_BYTES {
        return Err(format!("report layout needs {state} bytes of state (more than {})", class::MAX_STATE_BYTES));
    }
    let input = InputState::new(&desc, &map, report_len);
    let inputs = register_inputs(dev, f.number, &desc, &map);
    let leds = Leds::new(&map);
    let pad = map.groups.iter().zip(&inputs).any(|(g, d)| d.is_some() && class::is_pad(g.application));
    let poll_want = class::pad_poll(pad, pci::registry::opted_in("usbhid_pad_binterval"), dev.speed(), ep.interval);
    let hid = Arc::new(HidIface {
        dev: dev.clone(),
        iface: f.number,
        protocol,
        desc,
        map,
        inputs,
        ep_in: ep.address,
        out_ep,
        b_interval: ep.interval,
        pad,
        poll_want,
        report_len,
        leds,
        st: SpinLock::named("hid.iface", St { input, gone: false, events: Vec::new(), pending: Vec::new() }),
        pipe: SpinLock::named("hid.pipe", None),
        stopping: AtomicBool::new(false),
        pipe_state: AtomicU8::new(PIPE_NONE),
        restarts: AtomicU32::new(0),
        restart_at: AtomicU64::new(0),
        restarted_ns: AtomicU64::new(0),
        reports: AtomicU64::new(0),
        bad_reports: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    });
    if hid.inputs.iter().any(Option::is_some) {
        // Running before the start: a pipe given up at once must find it running to be restarted.
        hid.pipe_state.store(PIPE_RUNNING, Ordering::Release);
        match hid.start_pipe() {
            Ok(p) => *hid.pipe.lock() = Some(p),
            Err(e) => {
                hid.shutdown();
                return Err(format!("interrupt pipe on {:#04x}: {}", ep.address, e.as_str()));
            }
        }
    } else {
        klog::event!(Info, "input", "hid.no_input", device = dev.path(), interface = f.number,
            vendor = klog::Hex(dev.vendor() as u64), product = klog::Hex(dev.product() as u64);
            "no input application (vendor interface): bound, not polled");
    }
    IFACES.lock().push(hid.clone());
    crate::worker::ensure_started();
    crate::leds::attach(&hid);
    let names: Vec<&str> = hid.inputs.iter().flatten().map(|d| d.info.name.as_str()).collect();
    let numbers: Vec<u32> = hid.inputs.iter().flatten().map(|d| d.number).collect();
    klog::event!(Info, "input", "hid.bound", device = dev.path(), interface = f.number, protocol = protocol.as_str(),
        vendor = klog::Hex(dev.vendor() as u64), product = klog::Hex(dev.product() as u64), report_len = report_len,
        inputs = format!("{numbers:?}"), names = format!("{names:?}"), pad = pad, b_interval = ep.interval,
        poll_us = class::poll_us(dev.speed(), ep.interval), pad_poll_want_us = class::poll_want_us(dev.speed(), poll_want).as_str());
    Ok(hid)
}

/// Linux hid-sony `sixaxis_set_operational_usb` steps 1 and 2: GET_REPORT of feature reports 0xF2 and 0xF5. An
/// answer, a short answer or a stall all change nothing here (V1 `pad_wake`): the pad either reports now or never.
/// Step 3 (a 1-byte output report on the interrupt OUT endpoint) is left out: hid-sony skips it for SHANWAN clones,
/// which it tells apart by their product string, and V2 reads no strings; the real DualShock 3 needs only 0xF2.
fn sixaxis_wake(dev: &Device, iface: u8) {
    let mut buf = [0u8; 17];
    for (id, len) in class::SIXAXIS_WAKE {
        let r = dev.control_in(class::get_report_feature(iface, id, len), &mut buf[..len as usize]);
        let got = r.as_ref().map(|&n| n as i64).unwrap_or(-1);
        let error = r.err().map(|e| e.as_str()).unwrap_or("none");
        klog::event!(Info, "input", "hid.sixaxis_wake", device = dev.path(), interface = iface, report = klog::Hex(id as u64),
            got = got, error = error;
            "feature report read so the pad starts reporting (Linux hid-sony)");
    }
}

/// Reads the report descriptor (`len` bytes, at most 4096 by `hid_descriptor`; zero-padded when the device sends less after four tries, as
/// Linux) and maps it; an error when it cannot be read or does not parse.
fn read_and_map(dev: &Device, iface: u8, h: HidDescriptor) -> Result<(ReportDescriptor, InputMap), String> {
    let mut buf = vec![0u8; h.report_len as usize];
    let mut got = Err(UsbError::Timeout);
    for _ in 0..DESC_TRIES {
        got = dev.control_in(class::get_report_descriptor(iface, h.report_len), &mut buf);
        if got.is_ok_and(|n| n >= buf.len()) {
            break;
        }
    }
    let n = got.map_err(|e| format!("report descriptor not read: {}", e.as_str()))?;
    if n < buf.len() {
        klog::event!(Warn, "input", "hid.desc_short", device = dev.path(), interface = iface, want = buf.len(), got = n;
            "zero-padded as Linux");
    }
    let desc = hidparse::parse(&buf).map_err(|e| format!("report descriptor does not parse: {e:?}"))?;
    let map = InputMap::new(&desc);
    klog::event!(Debug, "input", "hid.descriptor", device = dev.path(), interface = iface, bytes = buf.len(),
        reports = desc.reports.len(), groups = map.groups.len());
    Ok((desc, map))
}

fn register_inputs(dev: &Device, iface: u8, desc: &ReportDescriptor, map: &InputMap) -> Vec<Option<Arc<InputDevice>>> {
    let several = desc.collections.iter().filter(|c| c.kind == hidparse::descriptor::collection::APPLICATION).count() > 1;
    map.groups
        .iter()
        .map(|g| {
            if g.is_empty() {
                return None;
            }
            let mut caps = Caps {
                keys: g.keys.clone(),
                rel: g.rel.clone(),
                abs: g.abs.iter().map(|(&c, a)| (c, AbsInfo { min: a.min, max: a.max, fuzz: a.fuzz, flat: a.flat, resolution: a.resolution })).collect(),
                leds: g.leds.clone(),
                repeat: g.repeat,
                ..Caps::default()
            };
            if !caps.keys.is_empty() {
                caps.msc.insert(hidparse::codes::MSC_SCAN); // hid-input.c:1450-1453
            }
            let info = DeviceInfo {
                name: class::device_name(dev.vendor(), dev.product(), g.application, several),
                phys: format!("{}/input{iface}", dev.path()),
                bus: evdev::BUS_USB,
                vendor: dev.vendor(),
                product: dev.product(),
                version: dev.desc.device,
                caps,
                // hid-input: `input_dev->uniq = hid->uniq` (the serial string; V2 reads none: empty, not absent).
                uniq: Some(String::new()),
            };
            Some(evdev::register(evdev::sink(), info))
        })
        .collect()
}

fn on_pipe(weak: &Weak<HidIface>, e: PipeEvent<'_>, at: u64) {
    let Some(h) = weak.upgrade() else { return };
    match e {
        PipeEvent::Data(r) => h.on_report_at(r, at),
        PipeEvent::Error(err) => {
            h.errors.fetch_add(1, Ordering::Relaxed);
            PIPE_ERRORS.inc();
            klog::event!(Debug, "input", "hid.report_error", device = h.dev.path(), interface = h.iface, error = err.as_str());
        }
        PipeEvent::Stopped(why) => h.on_stopped(why),
    }
}

impl HidIface {
    fn start_pipe(self: &Arc<Self>) -> Result<PipeHandle, UsbError> {
        let weak = Arc::downgrade(self);
        let sink = Arc::new(move |e: PipeEvent<'_>, at: u64| on_pipe(&weak, e, at));
        // IN-2b: the 1 ms wish for gamepads goes to the controller (xhci re-adds the endpoint with it).
        let opts = PipeOpts::new(self.report_len, PIPE_DEPTH);
        let opts = match self.poll_want {
            Some(b) => opts.with_interval(b),
            None => opts,
        };
        self.dev.interrupt_pipe_at(self.ep_in, opts, sink)
    }

    /// The pipe ended without the driver asking (the host gave the endpoint up after 50 errors in a row, or a
    /// cancel from outside): nothing would release held keys, so do it now, and have the worker start the pipe
    /// again (event context: no waiting here).
    fn on_stopped(&self, why: UsbError) {
        if self.stopping.load(Ordering::Acquire) || self.dev.is_gone() {
            return;
        }
        if self.pipe_state.compare_exchange(PIPE_RUNNING, PIPE_RESTARTING, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        let now = time::now_ns();
        let released: usize = self.inputs.iter().flatten().map(|d| d.release_keys(now)).sum();
        // A pipe that ran a while gets the whole budget again; one given up again right away keeps counting.
        if now.saturating_sub(self.restarted_ns.load(Ordering::Acquire)) >= PIPE_HEALTHY_NS {
            self.restarts.store(0, Ordering::Release);
        }
        self.schedule_restart(now, why.as_str(), released);
    }

    /// Plans the next restart after 1, 2, 4 ... s, or marks the interface dead when the budget is used up.
    fn schedule_restart(&self, now: u64, why: &str, released: usize) {
        let n = self.restarts.fetch_add(1, Ordering::AcqRel) + 1;
        if n > PIPE_RESTARTS {
            self.pipe_state.store(PIPE_DEAD, Ordering::Release);
            klog::event!(Error, "input", "hid.pipe_dead", device = self.dev.path(), interface = self.iface, why = why,
                restarts = n - 1, released = released; "no reports from this interface until it is plugged in again");
            return;
        }
        let delay = crate::worker::TICK_NS << (n - 1).min(5);
        self.restart_at.store(now.saturating_add(delay), Ordering::Release);
        self.pipe_state.store(PIPE_RESTARTING, Ordering::Release);
        klog::event!(Warn, "input", "hid.pipe_stopped", device = self.dev.path(), interface = self.iface, why = why,
            released = released, restart = n, delay_ms = delay / 1_000_000; "held keys released, pipe started again later");
        crate::worker::kick();
    }

    /// The worker's part: starts a given-up pipe again when its time has come (thread context).
    pub fn restart_if_due(self: &Arc<Self>, now: u64) {
        if self.pipe_state.load(Ordering::Acquire) != PIPE_RESTARTING
            || now < self.restart_at.load(Ordering::Acquire)
            || self.stopping.load(Ordering::Acquire)
            || self.dev.is_gone()
        {
            return;
        }
        // The old pipe's leftovers (if the host has not cleaned them yet) go first; its later events are ignored.
        let old = self.pipe.lock().take();
        if let Some(p) = old {
            let _ = p.stop();
        }
        // Reports start from scratch: whatever the device held before counts as new presses.
        {
            let mut st = self.st.lock();
            st.input = InputState::new(&self.desc, &self.map, self.report_len);
        }
        match self.start_pipe() {
            Ok(p) => {
                let mut g = self.pipe.lock();
                if self.stopping.load(Ordering::Acquire) {
                    // Unbound meanwhile: `shutdown` took the pipe slot already, so this one is stopped here.
                    drop(g);
                    let _ = p.stop();
                    return;
                }
                *g = Some(p);
                drop(g);
                self.restarted_ns.store(now, Ordering::Release);
                self.pipe_state.store(PIPE_RUNNING, Ordering::Release);
                klog::event!(Info, "input", "hid.pipe_restarted", device = self.dev.path(), interface = self.iface,
                    restart = self.restarts.load(Ordering::Acquire));
            }
            Err(e) => self.schedule_restart(now, e.as_str(), 0),
        }
    }

    /// A report that arrived now (tests and callers without an interrupt time).
    pub fn on_report(&self, r: &[u8]) {
        self.on_report_at(r, time::now_ns())
    }

    /// One report from the pipe (the controller's event thread: no sleeping). `irq_ns` is the time the
    /// controller's interrupt for it was noticed (US-ev), so the evdev time and the latency tool's arrival stage
    /// include the way from the interrupt to here. Events go to the input device of the report's group under the
    /// interface lock, so an unplug's releases always come after them.
    pub fn on_report_at(&self, r: &[u8], irq_ns: u64) {
        let now = irq_ns;
        let mut st = self.st.lock();
        if st.gone {
            return;
        }
        let St { input, events, pending, .. } = &mut *st;
        events.clear();
        match input.process(&self.desc, &self.map, r, events) {
            Ok(()) => {
                self.reports.fetch_add(1, Ordering::Relaxed);
                REPORTS.inc();
            }
            Err(e) => {
                let n = self.bad_reports.fetch_add(1, Ordering::Relaxed);
                BAD_REPORTS.inc();
                if n == 0 {
                    let what = match e {
                        InputError::Empty => String::from("empty report"),
                        InputError::UnknownReport(id) => format!("unknown report ID {id}"),
                    };
                    klog::event!(Info, "input", "hid.report_dropped", device = self.dev.path(), interface = self.iface,
                        what = what.as_str(); "counted from here on (input.hid.bad_reports)");
                }
                return;
            }
        }
        // One report belongs to one group (hidparse maps reports, not fields, to input devices).
        let Some(group) = events.first().map(|e| e.group) else { return };
        let Some(Some(dev)) = self.inputs.get(group) else { return };
        pending.clear();
        pending.extend(events.iter().map(|e| (e.ev, e.code, e.value)));
        dev.emit(now, pending);
    }

    /// Unplug or driver removal: no report passes from now on, every held key is released at once, the input
    /// devices go away, the LED listener and the pipe stop.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        self.pipe_state.store(PIPE_NONE, Ordering::Release);
        self.st.lock().gone = true;
        let now = time::now_ns();
        for d in self.inputs.iter().flatten() {
            d.unregister(now);
        }
        crate::leds::detach(self);
        // Taken out first: stopping waits for the controller, never under the spin lock.
        let pipe = self.pipe.lock().take();
        if let Some(p) = pipe
            && !self.dev.is_gone()
            && let Err(e) = p.stop()
        {
            klog::event!(Warn, "input", "hid.pipe_stop_failed", device = self.dev.path(), interface = self.iface, error = e.as_str());
        }
    }

    /// Input devices of this interface (numbers), for tools and tests.
    pub fn numbers(&self) -> Vec<u32> {
        self.inputs.iter().flatten().map(|d| d.number).collect()
    }
}
