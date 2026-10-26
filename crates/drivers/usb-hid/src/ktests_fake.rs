//! In-kernel tests with a fake USB host: paths QEMU's HID devices never take. The fake answers the control
//! requests of enumeration and of the driver's probe the way a test needs (a report descriptor that fails, does
//! not parse or is vendor-only; SET_REPORT that fails), and hands the test the pipe's sink, so reports, hostile
//! reports and a given-up pipe can be fed without hardware. Devices go through the real `usb_core::enumerate`
//! and registry, so usb-hid binds them as it binds real ones; each test detaches its devices again. QemuOnly:
//! the fake devices' input devices feed the machine's real input sink.

use crate::iface::{self, HidIface, Protocol};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use console::evdev;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use hidparse::codes::*;
use ksync::SpinLock;
use ktest::{TestResult, kassert, ktest};
use usb_core::enumerate::{Scheme, enumerate};
use usb_core::{
    Buffer, Data, Device, Endpoint, HcSlot, Host, HubParams, PipeEvent, PipeOpts, Place, Request, Setup, Sink, Speed, TimedSink,
    Urb, UsbError, XferResult, registry,
};

const MS: u64 = 1_000_000;
const VENDOR: u16 = 0xB175;
/// A vendor-only report descriptor (HID++-like): one application on page 0xFF00, 8 bytes in.
const VENDOR_ONLY: &[u8] = &[0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x08,
    0x09, 0x01, 0x81, 0x02, 0xC0];

struct Fake {
    name: String,
    device: [u8; 18],
    config: Vec<u8>,
    report: Vec<u8>,
    /// GET_DESCRIPTOR(report) fails this many times first (u32::MAX: always).
    desc_fail: u32,
    desc_reads: AtomicU32,
    /// The next this-many SET_REPORTs fail.
    set_report_fail: AtomicU32,
    set_reports: AtomicU32,
    protocols: SpinLock<Vec<u16>>,
    pipes: SpinLock<Vec<Sink>>,
    /// IN-6b: a host that hands on interrupt times (as xhci since US-ev): pipes go to `timed`, fed by `feed_at`.
    with_times: AtomicBool,
    timed: SpinLock<Vec<TimedSink>>,
    pipe_starts: AtomicU32,
    /// The last pipe's interval wish: 0x100 | bInterval, or 0 for none (IN-2b).
    pipe_interval: AtomicU32,
    /// How the Sixaxis wake is answered: `WAKE_ANSWER`, `WAKE_SHORT_TIMEOUT` or `WAKE_STALL`.
    wake: AtomicU32,
    /// Every control request (type, request, value, index, length) in order; `PIPE_MARK` where a pipe started.
    controls: SpinLock<Vec<(u8, u8, u16, u16, u16)>>,
}

/// Stands in `Fake::controls` for "the interrupt pipe started here".
const PIPE_MARK: (u8, u8, u16, u16, u16) = (0xFF, 0xFF, 0, 0, 0);

/// 0xF2 answers its 17 bytes, 0xF5 stalls.
const WAKE_ANSWER: u32 = 0;
/// 0xF2 answers 5 of its 17 bytes, 0xF5 times out.
const WAKE_SHORT_TIMEOUT: u32 = 1;
/// Both stall.
const WAKE_STALL: u32 = 2;
const F2_ANSWER: [u8; 17] = [0xF2, 0xFF, 0, 0x34, 0x12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// How a fake device looks beyond its report descriptor.
#[derive(Clone, Copy)]
struct Shape {
    vendor: u16,
    product: u16,
    subclass: u8,
    protocol: u8,
    mps0: u8,
    /// wMaxPacketSize and bInterval of the interrupt IN endpoint 0x81.
    mps: u8,
    interval: u8,
    /// An interrupt OUT endpoint 0x01 beside it (Georg's DragonRise has one).
    out: bool,
}

impl Fake {
    fn new(tag: &str, product: u16, subclass: u8, protocol: u8, report: &[u8], desc_fail: u32) -> Arc<Fake> {
        let shape = Shape { vendor: VENDOR, product, subclass, protocol, mps0: 64, mps: 8, interval: 10, out: false };
        Fake::shaped(tag, shape, report, desc_fail)
    }

    fn shaped(tag: &str, sh: Shape, report: &[u8], desc_fail: u32) -> Arc<Fake> {
        let [vl, vh] = sh.vendor.to_le_bytes();
        let [pl, ph] = sh.product.to_le_bytes();
        // iManufacturer 1, iProduct 2 as Georg's DragonRise names them; nothing may ask for them (string trap).
        let device = [18, 1, 0x00, 0x02, 0, 0, 0, sh.mps0, vl, vh, pl, ph, 0x00, 0x01, 1, 2, 0, 1];
        let [rl, rh] = (report.len() as u16).to_le_bytes();
        let mut config = alloc::vec![9, 2, 0, 0, 1, 1, 0, 0x80, 50];
        config.extend_from_slice(&[9, 4, 0, 0, 1 + sh.out as u8, 3, sh.subclass, sh.protocol, 0]);
        config.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, rl, rh]);
        config.extend_from_slice(&[7, 5, 0x81, 3, sh.mps, 0, sh.interval]);
        if sh.out {
            config.extend_from_slice(&[7, 5, 0x01, 3, sh.mps, 0, sh.interval]);
        }
        let [tl, th] = (config.len() as u16).to_le_bytes();
        config[2] = tl;
        config[3] = th;
        Arc::new(Fake {
            name: alloc::format!("hidfake-{tag}"),
            device,
            config,
            report: report.to_vec(),
            desc_fail,
            desc_reads: AtomicU32::new(0),
            set_report_fail: AtomicU32::new(0),
            set_reports: AtomicU32::new(0),
            protocols: SpinLock::named("hidfake.protocols", Vec::new()),
            pipes: SpinLock::named("hidfake.pipes", Vec::new()),
            with_times: AtomicBool::new(false),
            timed: SpinLock::named("hidfake.timed", Vec::new()),
            pipe_starts: AtomicU32::new(0),
            pipe_interval: AtomicU32::new(0),
            wake: AtomicU32::new(WAKE_ANSWER),
            controls: SpinLock::named("hidfake.controls", Vec::new()),
        })
    }

    fn controls(&self) -> Vec<(u8, u8, u16, u16, u16)> {
        self.controls.lock().clone()
    }

    fn feed(&self, report: &[u8]) {
        let sinks: Vec<Sink> = self.pipes.lock().clone();
        for s in sinks {
            s(PipeEvent::Data(report));
        }
    }

    /// A report whose controller interrupt came at `at` (a host with times).
    fn feed_at(&self, report: &[u8], at: u64) {
        let sinks: Vec<TimedSink> = self.timed.lock().clone();
        for s in sinks {
            s(PipeEvent::Data(report), at);
        }
    }
    /// The host gives the pipe up (as xHCI after 50 errors in a row): its sink hears `Stopped` once.
    fn give_up(&self) {
        let sinks: Vec<Sink> = core::mem::take(&mut *self.pipes.lock());
        for s in sinks {
            s(PipeEvent::Stopped(UsbError::Transaction));
        }
    }
}

fn copy(src: &[u8], data: Data<'_>) -> Result<usize, UsbError> {
    match data {
        Data::In(buf) => {
            let n = src.len().min(buf.len());
            buf[..n].copy_from_slice(&src[..n]);
            Ok(n)
        }
        _ => Err(UsbError::Stall),
    }
}

impl Host for Fake {
    fn name(&self) -> &str {
        &self.name
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns);
    }
    fn address(&self, _: &Place, _: u16, _: bool) -> Result<(HcSlot, u8), UsbError> {
        Ok((HcSlot(1), 7))
    }
    fn set_address(&self, _: HcSlot) -> Result<u8, UsbError> {
        Ok(7)
    }
    fn set_mps0(&self, _: HcSlot, _: u16) -> Result<(), UsbError> {
        Ok(())
    }
    fn control(&self, _: HcSlot, s: Setup, data: Data<'_>) -> Result<usize, UsbError> {
        self.controls.lock().push((s.request_type, s.request, s.value, s.index, s.length));
        match (s.request_type, s.request, s.value >> 8) {
            (0x80, 6, 1) => copy(&self.device, data),
            (0x80, 6, 2) => copy(&self.config, data),
            // The DragonRise trap: a string index (named or not) answers with 255 bytes of memory.
            (0x80, 6, 3) => copy(&[0xA5; 255], data),
            // Sixaxis wake: an answer, a short answer, a timeout and a stall must all be fine.
            (0xA1, 1, 3) => match (self.wake.load(Ordering::Acquire), s.value & 0xFF) {
                (WAKE_ANSWER, 0xF2) => copy(&F2_ANSWER, data),
                (WAKE_SHORT_TIMEOUT, 0xF2) => copy(&F2_ANSWER[..5], data),
                (WAKE_SHORT_TIMEOUT, 0xF5) => Err(UsbError::Timeout),
                _ => Err(UsbError::Stall),
            },
            (0x81, 6, 0x22) => {
                if self.desc_reads.fetch_add(1, Ordering::AcqRel) < self.desc_fail {
                    return Err(UsbError::Stall);
                }
                copy(&self.report, data)
            }
            // Many devices stall SET_IDLE; the driver goes on.
            (0x21, 0x0A, _) => Err(UsbError::Stall),
            (0x21, 0x0B, _) => {
                self.protocols.lock().push(s.value);
                Ok(0)
            }
            (0x21, 0x09, _) => {
                self.set_reports.fetch_add(1, Ordering::AcqRel);
                // Only the driver's worker sends LED reports: no other thread races this check.
                let fail = self.set_report_fail.load(Ordering::Acquire) > 0;
                if fail {
                    self.set_report_fail.fetch_sub(1, Ordering::AcqRel);
                }
                if fail { Err(UsbError::Timeout) } else { Ok(s.length as usize) }
            }
            (0x00, 9, _) => Ok(0),
            _ => Err(UsbError::Stall),
        }
    }
    fn configure(&self, _: HcSlot, _: &[Endpoint]) -> Result<(), UsbError> {
        Ok(())
    }
    fn set_hub(&self, _: HcSlot, _: HubParams) -> Result<(), UsbError> {
        Err(UsbError::Invalid("not a hub"))
    }
    fn interrupt_in(&self, _: HcSlot, _: u8, opts: PipeOpts, sink: Sink) -> Result<(), UsbError> {
        self.pipe_interval.store(opts.interval.map_or(0, |b| 0x100 | b as u32), Ordering::Release);
        let mut p = self.pipes.lock();
        if !p.is_empty() {
            return Err(UsbError::Busy);
        }
        p.push(sink);
        self.controls.lock().push(PIPE_MARK);
        self.pipe_starts.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
    fn interrupt_in_at(&self, _: HcSlot, _: u8, _: PipeOpts, sink: &TimedSink) -> Option<Result<(), UsbError>> {
        if !self.with_times.load(Ordering::Acquire) {
            return None;
        }
        let mut p = self.timed.lock();
        if !p.is_empty() {
            return Some(Err(UsbError::Busy));
        }
        p.push(sink.clone());
        self.controls.lock().push(PIPE_MARK);
        self.pipe_starts.fetch_add(1, Ordering::AcqRel);
        Some(Ok(()))
    }
    fn stop_pipe(&self, _: HcSlot, _: u8) -> Result<(), UsbError> {
        let sinks: Vec<Sink> = core::mem::take(&mut *self.pipes.lock());
        for s in sinks {
            s(PipeEvent::Stopped(UsbError::Cancelled));
        }
        let timed: Vec<TimedSink> = core::mem::take(&mut *self.timed.lock());
        for s in timed {
            s(PipeEvent::Stopped(UsbError::Cancelled), time::now_ns());
        }
        Ok(())
    }
    fn alloc_buffer(&self, _: usize) -> Result<Buffer, UsbError> {
        Err(UsbError::NoMemory)
    }
    fn submit(&self, _: HcSlot, _: u8, _: Request, _: Arc<Urb>) -> Result<(), UsbError> {
        Err(UsbError::Invalid("fake host has no transfers"))
    }
    fn wait(&self, _: HcSlot, _: &Urb, _: u64) -> bool {
        true
    }
    fn cancel(&self, _: HcSlot, _: u8, urb: &Arc<Urb>, why: UsbError) {
        urb.complete(XferResult::failed(why, 0));
    }
    fn reset_endpoint(&self, _: HcSlot, _: u8) -> Result<(), UsbError> {
        Ok(())
    }
    fn abort_endpoint(&self, _: HcSlot, _: u8) -> Result<usize, UsbError> {
        Ok(0)
    }
    fn release(&self, _: HcSlot) {
        self.pipes.lock().clear();
    }
    fn kick(&self) {}
}

fn until(ns: u64, f: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns().saturating_add(ns);
    while time::now_ns() < deadline {
        if f() {
            return true;
        }
        sched::sleep_ns(10 * MS);
    }
    f()
}

/// Enumerates the fake device, runs `t` with it and its bound interface, and always detaches it again.
fn with_device(fake: &Arc<Fake>, port: u8, t: impl FnOnce(&Arc<Device>, Option<Arc<HidIface>>) -> TestResult) -> TestResult {
    with_device_at(fake, port, Speed::Full, t)
}

fn with_device_at(fake: &Arc<Fake>, port: u8, speed: Speed, t: impl FnOnce(&Arc<Device>, Option<Arc<HidIface>>) -> TestResult) -> TestResult {
    if iface::left_raw().is_some() {
        return ktest::skip("CONFIG leaves HID devices unbound");
    }
    let host: Arc<dyn Host> = fake.clone();
    let dev = enumerate(&host, Place::root(port, speed), Scheme::Direct).map_err(|e| alloc::format!("enumerate: {}", e.as_str()))?;
    let h = iface::all().into_iter().find(|h| h.dev.id == dev.id);
    let r = t(&dev, h);
    registry::detach(&dev);
    r
}

/// Events of `numbers` from sequence `from` on, as (type, code, value) without SYN and MSC.
fn events_of(numbers: &[u32], from: u64) -> Vec<(u16, u16, i32)> {
    evdev::sink()
        .recent_since(from)
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| !e.injected && numbers.contains(&e.device) && e.kind != EV_SYN && e.kind != EV_MSC)
        .map(|e| (e.kind, e.code, e.value))
        .collect()
}

fn none_held(numbers: &[u32]) -> bool {
    evdev::devices().iter().filter(|d| numbers.contains(&d.number)).all(|d| d.held_keys().is_empty())
}

/// Boot fallback: a boot keyboard whose report descriptor never comes (four stalls) and a boot mouse whose
/// descriptor does not parse both bind in the boot protocol (SET_PROTOCOL(boot) sent) and turn boot reports into
/// events. Hostile reports on the keyboard (ErrorRollOver, too short, too long) neither panic nor leave a key
/// held nor flood; the unplug releases what is held.
fn boot_fallback() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    // Junk reports may press Caps or Num Lock: the machine's lock state is put back afterwards.
    let locks = evdev::sink().locks();
    let r = boot_fallback_inner();
    evdev::sink().set_locks(locks);
    r
}

fn boot_fallback_inner() -> TestResult {
    let kbd = Fake::new("bootkbd", 1, 1, 1, hidparse::boot::BOOT_KEYBOARD, u32::MAX);
    with_device(&kbd, 201, |_, h| {
        let h = h.ok_or("boot keyboard not bound")?;
        kassert!(h.protocol == Protocol::Boot, "protocol {}", h.protocol.as_str());
        kassert!(kbd.desc_reads.load(Ordering::Acquire) == 4, "descriptor tries {}", kbd.desc_reads.load(Ordering::Acquire));
        kassert!(*kbd.protocols.lock() == [0], "SET_PROTOCOL requests {:?}", *kbd.protocols.lock());
        let n = h.numbers();
        kassert!(n.len() == 1, "input devices {n:?}");
        let from = evdev::sink().next_seq();
        kbd.feed(&[0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        let ev = events_of(&n, from);
        kassert!(ev == [(EV_KEY, KEY_LEFTSHIFT, 1), (EV_KEY, KEY_A, 1)], "boot report: {ev:?}");
        // ErrorRollOver in every slot: the keyboard lost track, the old state stays (Linux hid-core.c).
        let from = evdev::sink().next_seq();
        kbd.feed(&[0x02, 0, 1, 1, 1, 1, 1, 1]);
        kassert!(events_of(&n, from).is_empty(), "rollover changed keys: {:?}", events_of(&n, from));
        // Too short (zero-padded as Linux: everything released), then too long (the rest ignored), then junk.
        let dev = evdev::devices().into_iter().find(|d| d.number == n[0]).ok_or("input device not registered")?;
        let passed = dev.events();
        let reports: u64 = 4 + 64;
        kbd.feed(&[0x02]);
        kbd.feed(&[0u8; 64]);
        kbd.feed(&[]);
        for b in 0..64u8 {
            let b = b.wrapping_mul(37);
            kbd.feed(&[b, b.wrapping_mul(7), b, b.wrapping_add(1), 0xFF, b, 3, b]);
        }
        kbd.feed(&[0u8; 8]);
        // At most 8 modifier changes, 6 releases and 6 presses (each with its scan code) and a SYN per report.
        let flood = dev.events() - passed;
        kassert!(flood <= reports * 41, "{flood} events from {reports} reports");
        kassert!(none_held(&n), "keys held after the all-zero report");
        let from = evdev::sink().next_seq();
        kbd.feed(&[0, 0, 0x2C, 0, 0, 0, 0, 0]);
        kassert!(events_of(&n, from) == [(EV_KEY, KEY_SPACE, 1)], "space after junk: {:?}", events_of(&n, from));
        Ok(())
    })?;
    kassert!(!evdev::sink().key_held_physical(KEY_SPACE), "space held after the unplug");

    let mouse = Fake::new("bootmouse", 2, 1, 2, &[0x05], 0);
    with_device(&mouse, 202, |_, h| {
        let h = h.ok_or("boot mouse not bound")?;
        kassert!(h.protocol == Protocol::Boot, "protocol {}", h.protocol.as_str());
        kassert!(*mouse.protocols.lock() == [0], "SET_PROTOCOL requests {:?}", *mouse.protocols.lock());
        let n = h.numbers();
        let from = evdev::sink().next_seq();
        mouse.feed(&[0x01, 5, 0xFB]);
        let ev = events_of(&n, from);
        kassert!(ev == [(EV_KEY, BTN_LEFT, 1), (EV_REL, REL_X, 5), (EV_REL, REL_Y, -5)], "boot mouse report: {ev:?}");
        Ok(())
    })?;
    kassert!(!evdev::sink().key_held_physical(BTN_LEFT), "button held after the unplug");
    Ok(())
}
ktest!("in.hid.fake_boot_fallback", QemuOnly, boot_fallback);

/// A vendor-only interface binds without input device and without a pipe; a report descriptor that comes only
/// on the fourth try still gives the report protocol, and no SET_PROTOCOL is sent (Linux never sends one).
fn vendor_and_retry() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let vendor = Fake::new("vendor", 3, 0, 0, VENDOR_ONLY, 0);
    with_device(&vendor, 203, |_, h| {
        let h = h.ok_or("vendor interface not bound")?;
        kassert!(h.numbers().is_empty(), "vendor interface got input devices {:?}", h.numbers());
        kassert!(vendor.pipe_starts.load(Ordering::Acquire) == 0, "vendor interface polled");
        kassert!(h.pipe_state.load(Ordering::Acquire) == iface::PIPE_NONE, "pipe state");
        Ok(())
    })?;
    let late = Fake::new("late", 4, 1, 1, hidparse::boot::BOOT_KEYBOARD, 3);
    with_device(&late, 204, |_, h| {
        let h = h.ok_or("keyboard with a late descriptor not bound")?;
        kassert!(h.protocol == Protocol::Report, "protocol {}", h.protocol.as_str());
        kassert!(late.desc_reads.load(Ordering::Acquire) == 4, "descriptor tries {}", late.desc_reads.load(Ordering::Acquire));
        kassert!(late.protocols.lock().is_empty(), "SET_PROTOCOL sent: {:?}", *late.protocols.lock());
        kassert!(late.pipe_starts.load(Ordering::Acquire) == 1, "pipe not started");
        Ok(())
    })
}
ktest!("in.hid.fake_vendor_and_retry", QemuOnly, vendor_and_retry);

/// IN-6b: on a host that hands on the controller interrupt's time (xhci since US-ev), a report's events carry
/// that time, not the time usb-hid saw the report: evdev and the latency tool's arrival start at the interrupt.
fn interrupt_stamp() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let mouse = Fake::new("stamp", 5, 1, 2, hidparse::boot::BOOT_MOUSE, u32::MAX);
    mouse.with_times.store(true, Ordering::Release);
    with_device(&mouse, 205, |_, h| {
        let h = h.ok_or("boot mouse not bound")?;
        let n = h.numbers();
        kassert!(n.len() == 1 && mouse.timed.lock().len() == 1, "input devices {n:?}, the pipe not timed");
        for dx in [5u8, 0xFB] {
            let irq = time::now_ns();
            sched::sleep_ns(2 * MS); // usb-hid sees the report 2 ms after its interrupt
            let from = evdev::sink().next_seq();
            let fed = time::now_ns();
            mouse.feed_at(&[0, dx, 0], irq);
            let got: Vec<u64> = evdev::sink().recent_since(from).into_iter().map(|(_, e)| e).filter(|e| e.device == n[0]).map(|e| e.time_ns).collect();
            kassert!(!got.is_empty(), "the report passed nothing");
            // Stamped on arrival it would be at or after `fed`.
            kassert!(got.iter().all(|&t| t >= irq && t < fed), "times {got:?}, interrupt at {irq}, fed at {fed}");
        }
        Ok(())
    })
}
ktest!("in.hid.fake_interrupt_stamp", QemuOnly, interrupt_stamp);

/// A pipe the host gives up releases the held keys at once and is started again by the worker (1 s later); the
/// device stays registered and its reports pass again. A failing SET_REPORT is sent again until it works.
fn pipe_restart_and_led_retry() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let kbd = Fake::new("restart", 5, 1, 1, hidparse::boot::BOOT_KEYBOARD, 0);
    let sink = evdev::sink();
    let locks = sink.locks();
    let r = with_device(&kbd, 205, |_, h| {
        let h = h.ok_or("keyboard not bound")?;
        let n = h.numbers();
        kbd.feed(&[0x02, 0, 0x04, 0, 0, 0, 0, 0]);
        kassert!(!none_held(&n), "keys not held");
        let from = sink.next_seq();
        kbd.give_up();
        let ev = events_of(&n, from);
        kassert!(ev.len() == 2 && ev.iter().all(|e| e.2 == 0), "releases at give-up: {ev:?}");
        kassert!(none_held(&n), "keys held after the pipe was given up");
        kassert!(h.pipe_state.load(Ordering::Acquire) == iface::PIPE_RESTARTING, "not restarting");
        let back = until(5000 * MS, || {
            kbd.pipe_starts.load(Ordering::Acquire) == 2 && h.pipe_state.load(Ordering::Acquire) == iface::PIPE_RUNNING
        });
        kassert!(back, "pipe not restarted (starts {})", kbd.pipe_starts.load(Ordering::Acquire));
        let from = sink.next_seq();
        kbd.feed(&[0, 0, 0x04, 0, 0, 0, 0, 0]);
        kassert!(events_of(&n, from) == [(EV_KEY, KEY_A, 1)], "after the restart: {:?}", events_of(&n, from));
        kbd.feed(&[0u8; 8]);
        // LEDs: the first two SET_REPORTs of the new state fail, the third goes through.
        kassert!(until(3000 * MS, || h.leds.sent.load(Ordering::Acquire) == sink.locks()), "initial LEDs not sent");
        let want = locks ^ (1 << evdev::LED_CAPSL);
        let before = kbd.set_reports.load(Ordering::Acquire);
        kbd.set_report_fail.store(2, Ordering::Release);
        sink.set_locks(want);
        let ok = until(6000 * MS, || h.leds.sent.load(Ordering::Acquire) == want);
        let tries = kbd.set_reports.load(Ordering::Acquire) - before;
        kassert!(ok, "LEDs {want:#x} not sent after retries ({tries} SET_REPORTs)");
        kassert!(tries >= 3 && h.leds.failures.load(Ordering::Acquire) == 0, "tries {tries}");
        Ok(())
    });
    sink.set_locks(locks);
    r
}
ktest!("in.hid.fake_pipe_restart", QemuOnly, pipe_restart_and_led_retry);

/// Georg's DragonRise 0079:0006 (usbredir-pad.mjs DRAGONRISE, hidparse tests/common/descs.rs).
const DRAGONRISE: &[u8] = &[0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0xa1, 0x02, 0x75, 0x08, 0x95, 0x05, 0x15, 0x00, 0x26, 0xff,
    0x00, 0x35, 0x00, 0x46, 0xff, 0x00, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x32, 0x09, 0x35, 0x81, 0x02, 0x75, 0x04,
    0x95, 0x01, 0x25, 0x07, 0x46, 0x3b, 0x01, 0x65, 0x14, 0x09, 0x39, 0x81, 0x42, 0x65, 0x00, 0x75, 0x01, 0x95, 0x0c, 0x25,
    0x01, 0x45, 0x01, 0x05, 0x09, 0x19, 0x01, 0x29, 0x0c, 0x81, 0x02, 0x06, 0x00, 0xff, 0x75, 0x01, 0x95, 0x08, 0x25, 0x01,
    0x45, 0x01, 0x09, 0x01, 0x81, 0x02, 0xc0, 0xa1, 0x02, 0x75, 0x08, 0x95, 0x07, 0x46, 0xff, 0x00, 0x26, 0xff, 0x00, 0x09,
    0x02, 0x91, 0x02, 0xc0, 0xc0];
/// A DualShock 3 054C:0268 (usbredir-pad.mjs DUALSHOCK3, hidparse tests/common/descs.rs), 148 bytes.
const DUALSHOCK3: &[u8] = &[0x05, 0x01, 0x09, 0x04, 0xa1, 0x01, 0xa1, 0x02, 0x85, 0x01, 0x75, 0x08, 0x95, 0x01, 0x15, 0x00,
    0x26, 0xff, 0x00, 0x81, 0x03, 0x75, 0x01, 0x95, 0x13, 0x15, 0x00, 0x25, 0x01, 0x35, 0x00, 0x45, 0x01, 0x05, 0x09, 0x19,
    0x01, 0x29, 0x13, 0x81, 0x02, 0x75, 0x01, 0x95, 0x0d, 0x06, 0x00, 0xff, 0x81, 0x03, 0x15, 0x00, 0x26, 0xff, 0x00, 0x05,
    0x01, 0x09, 0x01, 0xa1, 0x00, 0x75, 0x08, 0x95, 0x04, 0x35, 0x00, 0x46, 0xff, 0x00, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32,
    0x09, 0x35, 0x81, 0x02, 0xc0, 0x05, 0x01, 0x95, 0x13, 0x09, 0x01, 0x81, 0x02, 0x95, 0x0c, 0x81, 0x01, 0x75, 0x10, 0x95,
    0x04, 0x26, 0xff, 0x03, 0x46, 0xff, 0x03, 0x09, 0x01, 0x81, 0x02, 0xc0, 0xa1, 0x02, 0x85, 0x02, 0x75, 0x08, 0x95, 0x30,
    0x09, 0x01, 0xb1, 0x02, 0xc0, 0xa1, 0x02, 0x85, 0xee, 0x75, 0x08, 0x95, 0x30, 0x09, 0x01, 0xb1, 0x02, 0xc0, 0xa1, 0x02,
    0x85, 0xef, 0x75, 0x08, 0x95, 0x30, 0x09, 0x01, 0xb1, 0x02, 0xc0, 0xc0];
/// A high speed gamepad: two 8-bit axes and 8 buttons.
const HS_PAD: &[u8] = &[0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08,
    0x95, 0x02, 0x81, 0x02, 0x05, 0x09, 0x19, 0x01, 0x29, 0x08, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0];

fn no_strings(fake: &Fake) -> TestResult {
    let asked: Vec<_> = fake.controls().into_iter().filter(|c| c.0 == 0x80 && c.1 == 6 && c.2 >> 8 == 3).collect();
    kassert!(asked.is_empty(), "string descriptors asked for: {asked:x?}");
    Ok(())
}

fn want_poll(h: &HidIface, b: u8) -> TestResult {
    let want = if pci::registry::opted_in("usbhid_pad_binterval") { None } else { Some(b) };
    kassert!(h.pad && h.poll_want == want, "pad {} poll_want {:?}, want {want:?}", h.pad, h.poll_want);
    Ok(())
}

/// One DragonRise life: bound as a pad polled at 1 ms, no string descriptor asked for, reports become ABS and
/// BTN events, the unplug releases the held buttons and removes its input device.
fn dragonrise_life(pad: &Arc<Fake>, port: u8) -> TestResult {
    let mut numbers = Vec::new();
    with_device_at(pad, port, Speed::Low, |_, h| {
        let h = h.ok_or("DragonRise not bound")?;
        kassert!(h.protocol == Protocol::Report && h.out_ep == Some(0x01) && h.b_interval == 10, "protocol/out/interval");
        want_poll(&h, 1)?;
        no_strings(pad)?;
        kassert!(!pad.controls().iter().any(|c| c.0 == 0xA1), "GET_REPORT sent to a pad that needs none");
        numbers = h.numbers();
        kassert!(numbers.len() == 1, "input devices {numbers:?}");
        let from = evdev::sink().next_seq();
        pad.feed(&[0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x0F, 0x00, 0xC0]);
        let ev = events_of(&numbers, from);
        kassert!(ev.contains(&(EV_ABS, ABS_X, 127)) && ev.contains(&(EV_ABS, ABS_RZ, 127)), "idle report: {ev:?}");
        // Stick left and up, button 3 and Start (button 10) held.
        let from = evdev::sink().next_seq();
        pad.feed(&[0, 0, 0x7F, 0x7F, 0x7F, 0x4F, 0x20, 0xC0]);
        let ev = events_of(&numbers, from);
        kassert!(ev == [(EV_ABS, ABS_X, 0), (EV_ABS, ABS_Y, 0), (EV_KEY, BTN_THUMB2, 1), (EV_KEY, BTN_BASE4, 1)], "buttons: {ev:?}");
        kassert!(!none_held(&numbers), "buttons not held");
        Ok(())
    })?;
    kassert!(evdev::devices().iter().all(|d| !numbers.contains(&d.number)), "input device {numbers:?} left after the unplug");
    kassert!(!evdev::sink().key_held_physical(BTN_BASE4), "Start held after the unplug");
    Ok(())
}

/// Gamepads (IN-2b): Georg's DragonRise twice (hot plug: the second plug-in binds anew and works), a DualShock 3
/// woken with feature reports 0xF2 and 0xF5 before its pipe starts (the stall of 0xF5 changes nothing), a high
/// speed gamepad whose bInterval 7 (8 ms) becomes 4 (1 ms) and one whose 1 already is fast enough.
fn pads() -> TestResult {
    let _console = console::ktest_lease::ConsoleLease::take()?;
    let shape = Shape { vendor: 0x0079, product: 0x0006, subclass: 0, protocol: 0, mps0: 8, mps: 8, interval: 10, out: true };
    let pad = Fake::shaped("dragonrise", shape, DRAGONRISE, 0);
    dragonrise_life(&pad, 206)?;
    pad.controls.lock().clear();
    dragonrise_life(&pad, 206)?;
    let wish = pad.pipe_interval.load(Ordering::Acquire);
    kassert!(wish == 0x101, "the low-speed pad's pipe asks bInterval 1 (1 ms): {wish:#x}");

    let shape = Shape { vendor: 0x054C, product: 0x0268, subclass: 0, protocol: 0, mps0: 64, mps: 64, interval: 1, out: true };
    let ds3 = Fake::shaped("ds3", shape, DUALSHOCK3, 0);
    with_device(&ds3, 207, |_, h| {
        let h = h.ok_or("DualShock 3 not bound")?;
        no_strings(&ds3)?;
        let c = ds3.controls();
        let at = |w: (u8, u8, u16, u16, u16)| c.iter().position(|&x| x == w);
        let (f2, f5, pipe) = (at((0xA1, 1, 0x03F2, 0, 17)), at((0xA1, 1, 0x03F5, 0, 8)), at(PIPE_MARK));
        kassert!(matches!((f2, f5, pipe), (Some(a), Some(b), Some(p)) if a < b && b < p), "wake order {f2:?} {f5:?} pipe {pipe:?}: {c:x?}");
        kassert!(h.pad && h.poll_want.is_none() && h.report_len == 64, "pad {} poll_want {:?} report_len {}", h.pad, h.poll_want, h.report_len);
        let n = h.numbers();
        let mut r = [0u8; 49];
        r[0] = 1;
        r[6..10].fill(0x80);
        pad_report(&ds3, &n, &r, &[(EV_ABS, ABS_X, 128), (EV_ABS, ABS_Y, 128), (EV_ABS, ABS_Z, 128), (EV_ABS, ABS_RZ, 128)])?;
        r[2] = 0x01;
        r[6] = 0;
        r[10] = 0x40;
        pad_report(&ds3, &n, &r, &[(EV_KEY, BTN_TRIGGER, 1), (EV_ABS, ABS_X, 0), (EV_ABS, ABS_MISC, 0x40)])?;
        hostile_reports(&ds3, &h, &n, &r)
    })?;
    kassert!(!evdev::sink().key_held_physical(BTN_TRIGGER), "Select held after the unplug");
    // The other Sixaxis ids, woken with a short 0xF2 answer and a 0xF5 timeout, or with two stalls: bound and
    // polled all the same.
    for (port, vendor, product, wake) in [(210, 0x054C, 0x042F, WAKE_SHORT_TIMEOUT), (211, 0x1345, 0x3008, WAKE_STALL)] {
        let shape = Shape { vendor, product, ..shape };
        let sx = Fake::shaped("sixaxis", shape, DUALSHOCK3, 0);
        sx.wake.store(wake, Ordering::Release);
        with_device(&sx, port, |_, h| sixaxis_bound(&sx, h))?;
    }

    let shape = Shape { vendor: VENDOR, product: 8, subclass: 0, protocol: 0, mps0: 64, mps: 8, interval: 7, out: false };
    let hs = Fake::shaped("hspad", shape, HS_PAD, 0);
    with_device_at(&hs, 208, Speed::High, |_, h| want_poll(&*h.ok_or("high speed pad not bound")?, 4))?;
    let wish = hs.pipe_interval.load(Ordering::Acquire);
    kassert!(wish == 0x104, "the high-speed pad's pipe asks bInterval 4 (1 ms): {wish:#x}");
    let shape = Shape { interval: 4, ..shape };
    let fast = Fake::shaped("hsfast", shape, HS_PAD, 0);
    with_device_at(&fast, 209, Speed::High, |_, h| {
        let h = h.ok_or("fast high speed pad not bound")?;
        kassert!(h.pad && h.poll_want.is_none(), "bInterval 4 is 1 ms already: poll_want {:?}", h.poll_want);
        Ok(())
    })
}
ktest!("in.hid.fake_pads", QemuOnly, pads);

/// A pad answering the wake badly still has both requests sent before its pipe starts and reports as any.
fn sixaxis_bound(fake: &Fake, h: Option<Arc<HidIface>>) -> TestResult {
    let h = h.ok_or("Sixaxis not bound after a bad wake answer")?;
    let c = fake.controls();
    let at = |w: (u8, u8, u16, u16, u16)| c.iter().position(|&x| x == w);
    let (f2, f5, pipe) = (at((0xA1, 1, 0x03F2, 0, 17)), at((0xA1, 1, 0x03F5, 0, 8)), at(PIPE_MARK));
    kassert!(matches!((f2, f5, pipe), (Some(a), Some(b), Some(p)) if a < b && b < p), "wake order {f2:?} {f5:?} pipe {pipe:?}: {c:x?}");
    kassert!(h.pad && fake.pipe_starts.load(Ordering::Acquire) == 1, "pad {} pipe starts {}", h.pad, fake.pipe_starts.load(Ordering::Acquire));
    let mut r = [0u8; 49];
    r[0] = 1;
    r[2] = 0x01;
    pad_report(fake, &h.numbers(), &r, &[(EV_KEY, BTN_TRIGGER, 1)])
}

/// Hostile reports on a numbered 64-byte pad (report_len = max(descriptor, MPS)), Linux `hid_report_raw_event`
/// (linux-6.6/drivers/hid/hid-core.c:1999-2030): an empty report and an unknown ID are dropped and counted; an
/// over-long report is read up to its fields (the rest is ignored); a short one is padded with zeros, so what it
/// cannot reach reads as released or 0. `held` is the last report (Select held, X 0, pressure 0x40).
fn hostile_reports(fake: &Fake, h: &HidIface, n: &[u32], held: &[u8; 49]) -> TestResult {
    let bad = h.bad_reports.load(Ordering::Relaxed);
    pad_report(fake, n, &[], &[])?;
    pad_report(fake, n, &[0x07, 0xFF, 0xFF, 0xFF], &[])?;
    kassert!(h.bad_reports.load(Ordering::Relaxed) == bad + 2, "bad_reports {} -> {}", bad, h.bad_reports.load(Ordering::Relaxed));
    let mut long = [0xFFu8; 300];
    long[..49].copy_from_slice(held);
    pad_report(fake, n, &long, &[])?;
    let reports = h.reports.load(Ordering::Relaxed);
    let from = evdev::sink().next_seq();
    fake.feed(&held[..3]);
    let ev = events_of(n, from);
    kassert!(h.reports.load(Ordering::Relaxed) == reports + 1, "short report not taken");
    kassert!(ev.contains(&(EV_ABS, ABS_Y, 0)) && ev.contains(&(EV_ABS, ABS_MISC, 0)) && !ev.iter().any(|e| e.0 == EV_KEY),
        "short report: {ev:?}");
    Ok(())
}

fn pad_report(fake: &Fake, n: &[u32], r: &[u8], want: &[(u16, u16, i32)]) -> TestResult {
    let from = evdev::sink().next_seq();
    fake.feed(r);
    let ev = events_of(n, from);
    kassert!(ev == want, "report {r:x?}: {ev:?}, want {want:?}");
    Ok(())
}
