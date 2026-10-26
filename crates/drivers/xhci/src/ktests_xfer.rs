//! In-kernel tests of US-3 under QEMU: Bulk-Only reads of the boot stick through the transfer API (one large TD
//! cut at 64 KiB, 120 KiB reads, a K-2 style chain where each stage is queued from the previous one's completion
//! callback), the stall path (QEMU's usb-storage stalls an IN token while it waits for a CBW: Reset Endpoint, the
//! device's halt cleared, the next command works; the toggle reset of a running endpoint), a timeout (a boot
//! keyboard with idle 0 NAKs: Stop Endpoint + Set TR Dequeue, the next transfer works) and interrupt IN pipes on
//! usb-kbd/usb-tablet (idle reports keep coming through `depth` queued TDs, the ring wraps, stop ends them).
//! `host/tests/us/40-xfer.sh` boots with the HID devices and drives keys and the tablet through QMP for
//! `us.xfer.hid_input`. Devices a class driver has bound are left alone (the test skips).

use crate::driver::controllers;
use crate::hc::MS;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use ktest::{kassert, ktest, TestResult};
use usb_core::{registry, Buffer, Device, EpKind, PipeEvent, PipeOpts, Request, Setup, UsbError, XferResult};

const T: u64 = 5000 * MS;
const SETTLE_NS: u64 = 15_000 * MS;

fn settled() -> bool {
    let deadline = time::now_ns().saturating_add(SETTLE_NS);
    loop {
        if controllers().iter().all(|c| c.state() != crate::hc::State::Running || c.settled()) {
            return true;
        }
        if time::now_ns() >= deadline {
            return false;
        }
        sched::sleep_ns(20 * MS);
    }
}

type Found = (Arc<Device>, u8, Vec<usb_core::Endpoint>);

/// Every unbound device with an interface of this class/subclass/protocol, with that interface's endpoints.
fn find_all(class: u8, subclass: u8, protocol: u8) -> Vec<Found> {
    registry::devices().into_iter().filter_map(|d| {
        let f = d.config.as_ref()?.defaults().find(|f| (f.class, f.subclass, f.protocol) == (class, subclass, protocol))?.clone();
        registry::bindings_of(d.id).is_empty().then_some((d, f.number, f.endpoints))
    }).collect()
}

fn find(class: u8, subclass: u8, protocol: u8) -> Option<Found> {
    find_all(class, subclass, protocol).into_iter().next()
}

// ---- Bulk-Only Transport, just enough to read

struct Bot {
    d: Arc<Device>,
    ep_in: u8,
    ep_out: u8,
    tag: u32,
}

fn cbw(tag: u32, len: u32, dir_in: bool, cb: &[u8]) -> Vec<u8> {
    let mut v = b"USBC".to_vec();
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(&len.to_le_bytes());
    v.push(if dir_in { 0x80 } else { 0 });
    v.push(0);
    v.push(cb.len() as u8);
    let mut c = [0u8; 16];
    c[..cb.len()].copy_from_slice(cb);
    v.extend_from_slice(&c);
    v
}

fn read10(lba: u32, n: u16) -> [u8; 10] {
    let (l, k) = (lba.to_be_bytes(), n.to_be_bytes());
    [0x28, 0, l[0], l[1], l[2], l[3], 0, k[0], k[1], 0]
}

impl Bot {
    /// Every unbound Bulk-Only mass storage device (the boot stick; the scenario adds a full speed one behind a
    /// hub and a SuperSpeed one).
    fn sticks() -> Vec<Bot> {
        find_all(8, 6, 0x50).into_iter().filter_map(|(d, _, eps)| {
            let ep_in = eps.iter().find(|e| e.kind() == EpKind::Bulk && e.is_in())?.address;
            let ep_out = eps.iter().find(|e| e.kind() == EpKind::Bulk && !e.is_in())?.address;
            Some(Bot { d, ep_in, ep_out, tag: 1 })
        }).collect()
    }

    /// One IN command: CBW, data into `buf` (at most `len`), CSW. Returns (bytes read, CSW status, residue).
    fn command_in(&mut self, cb: &[u8], buf: &Buffer, len: usize) -> Result<(usize, u8, u32), String> {
        self.command_in_at(cb, buf, 0, len)
    }

    fn command_in_at(&mut self, cb: &[u8], buf: &Buffer, off: usize, len: usize) -> Result<(usize, u8, u32), String> {
        self.tag += 1;
        let tag = self.tag;
        self.d.out_bytes(self.ep_out, &cbw(tag, len as u32, true, cb), T).map_err(|e| format!("CBW: {e:?}"))?;
        let n = if len > 0 {
            self.d.transfer(self.ep_in, Request::new(buf.clone(), off, len), T).status().map_err(|e| format!("data: {e:?}"))?
        } else {
            0
        };
        let csw = self.d.in_bytes(self.ep_in, 13, T).map_err(|e| format!("CSW: {e:?}"))?;
        if csw.len() != 13 || &csw[0..4] != b"USBS" || u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]) != tag {
            return Err(format!("bad CSW {csw:x?}"));
        }
        Ok((n, csw[12], u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]])))
    }
}

impl Bot {
    /// TEST UNIT READY until it passes (REQUEST SENSE after each failure): a fresh stick answers its first media
    /// command with UNIT ATTENTION 29h "power on or reset" (SCSI; the US-4 driver retries the same way).
    fn ready(&mut self) -> Result<(), String> {
        let sense = self.d.alloc_buffer(18).map_err(|e| format!("{e:?}"))?;
        for _ in 0..5 {
            let (_, st, _) = self.command_in(&[0, 0, 0, 0, 0, 0], &sense, 0)?;
            if st == 0 {
                return Ok(());
            }
            let (k, _, _) = self.command_in(&[0x03, 0, 0, 0, 18, 0], &sense, 18)?;
            let s = read_all(&sense, k);
            klog::event!(Info, "usb", "ktest.unit_attention", device = self.d.path(), sense = format!("{s:x?}"));
        }
        Err("TEST UNIT READY never passed".into())
    }
}

fn read_all(b: &Buffer, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    let _ = b.read(0, &mut v);
    v
}

/// INQUIRY, READ CAPACITY, LBA 0 (boot signature), 1 MiB as one TD and the same again as 120 KiB reads (real
/// sticks: 32 KiB commands only).
fn stick_read() -> TestResult {
    kassert!(settled(), "hotplug did not settle");
    let sticks = Bot::sticks();
    if sticks.is_empty() {
        return ktest::skip("no unbound USB mass storage device");
    }
    for bot in sticks {
        let path = bot.d.path();
        stick_read_one(bot).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}
ktest!("us.xfer.stick_read", Safe, stick_read);

fn stick_read_one(mut bot: Bot) -> TestResult {
    bot.ready()?;
    let small = bot.d.alloc_buffer(512).map_err(|e| format!("{e:?}"))?;
    let (n, st, _) = bot.command_in(&[0x12, 0, 0, 0, 36, 0], &small, 36)?;
    kassert!(n == 36 && st == 0, "INQUIRY: {n} bytes, status {st}");
    let (n, st, _) = bot.command_in(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &small, 8)?;
    let cap = read_all(&small, 8);
    let blocks = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]) as u64 + 1;
    let bsize = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
    kassert!(n == 8 && st == 0 && bsize == 512, "READ CAPACITY: {n} bytes, status {st}, block {bsize}");
    let (n, st, _) = bot.command_in(&read10(0, 1), &small, 512)?;
    let lba0 = read_all(&small, 512);
    kassert!(n == 512 && st == 0 && lba0[510] == 0x55 && lba0[511] == 0xAA, "LBA 0: {n} bytes, status {st}, no boot signature");
    let span: usize = if blocks >= 2048 { 1 << 20 } else { (blocks as usize) * 512 };
    // Passes of different command sizes over the same span, each compared with the first. QEMU: the whole span as
    // one TD (cut at 64 KiB), 120 KiB (Linux max_sectors for USB 2) and 32 KiB. Real sticks: 32 KiB only, the
    // limit Linux keeps for Georg's 090C:1000 (US_FL_MAX_SECTORS_64); US-4 brings per-device limits.
    let passes: &[usize] = if hal::cpu::detect().hypervisor { &[usize::MAX, 240, 64] } else { &[64] };
    let big = bot.d.alloc_buffer(span).map_err(|e| format!("{e:?}"))?;
    let mut whole: Vec<u8> = Vec::new();
    let mut ns = 1;
    for &blocks_per in passes {
        let per = blocks_per.saturating_mul(512).min(span);
        let t0 = time::now_ns();
        let mut at = 0usize;
        while at < span {
            let k = (span - at).min(per);
            let (n, st, residue) = bot.command_in_at(&read10((at / 512) as u32, (k / 512) as u16), &big, at, k)?;
            kassert!(n == k && st == 0 && residue == 0, "{per}-byte reads, at {at:#x}: {n} of {k}, status {st}, residue {residue}");
            at += k;
        }
        let got = read_all(&big, span);
        if whole.is_empty() {
            ns = time::now_ns().saturating_sub(t0).max(1);
            kassert!(got[..512] == lba0[..], "the first pass's first block differs from LBA 0");
            whole = got;
        } else {
            kassert!(got == whole, "{per}-byte reads differ from the first pass");
        }
        kassert!(big.write(0, &vec![0u8; span]), "buffer clear");
    }
    klog::event!(Info, "usb", "ktest.stick_read", device = bot.d.path(), blocks = blocks, bytes = span,
        kib_per_s = (span as u64 * 1_000_000_000 / ns) / 1024, speed = bot.d.speed().as_str());
    Ok(())
}

static CHAIN_DONE: AtomicU32 = AtomicU32::new(0);
static CHAIN_FAILED: AtomicU32 = AtomicU32::new(0);
static CHAIN_WQ: WaitQueue = WaitQueue::new();

/// K-2 style (spec usb.md 3.6): each stage of a READ(10) is queued from the previous stage's completion callback
/// (event context), the thread only waits for the end of the chain; 16 commands back to back.
fn chain() -> TestResult {
    kassert!(settled(), "hotplug did not settle");
    let sticks = Bot::sticks();
    if sticks.is_empty() {
        return ktest::skip("no unbound USB mass storage device");
    }
    for bot in sticks {
        let path = bot.d.path();
        chain_one(bot).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}
ktest!("us.xfer.chain", Safe, chain);

fn chain_one(mut bot: Bot) -> TestResult {
    bot.ready()?;
    let d = bot.d.clone();
    let data = d.alloc_buffer(64 * 512).map_err(|e| format!("{e:?}"))?;
    let csw = d.alloc_buffer(13).map_err(|e| format!("{e:?}"))?;
    CHAIN_DONE.store(0, Ordering::Release);
    CHAIN_FAILED.store(0, Ordering::Release);
    let reference = {
        let mut b = Bot { d: d.clone(), ep_in: bot.ep_in, ep_out: bot.ep_out, tag: 100 };
        let r = d.alloc_buffer(64 * 512).map_err(|e| format!("{e:?}"))?;
        let (n, st, _) = b.command_in(&read10(8, 64), &r, 64 * 512)?;
        kassert!(n == 64 * 512 && st == 0, "reference read: {n} bytes, status {st}");
        read_all(&r, 64 * 512)
    };
    for i in 0..16u32 {
        let cmd = d.alloc_buffer(31).map_err(|e| format!("{e:?}"))?;
        kassert!(cmd.write(0, &cbw(0x1000 + i, 64 * 512, true, &read10(8, 64))));
        let (d1, d2, data1, csw1) = (d.clone(), d.clone(), data.clone(), csw.clone());
        let (ep_in, ep_in2) = (bot.ep_in, bot.ep_in);
        let fail = |what: &str, r: &XferResult| {
            CHAIN_FAILED.fetch_add(1, Ordering::AcqRel);
            klog::event!(Warn, "usb", "ktest.chain_failed", stage = what, error = format!("{:?}", r.error), moved = r.actual);
            CHAIN_WQ.wake_all();
        };
        let start = time::now_ns();
        d.submit_then(bot.ep_out, Request::new(cmd, 0, 31), alloc::boxed::Box::new(move |r: &XferResult| {
            if r.error.is_some() {
                return fail("cbw", r);
            }
            let next = d1.submit_then(ep_in, Request::new(data1, 0, 64 * 512), alloc::boxed::Box::new(move |r: &XferResult| {
                if r.error.is_some() || r.actual != 64 * 512 {
                    return fail("data", r);
                }
                let last = d2.submit_then(ep_in2, Request::new(csw1, 0, 13), alloc::boxed::Box::new(move |r: &XferResult| {
                    if r.error.is_some() || r.actual != 13 {
                        return fail("csw", r);
                    }
                    CHAIN_DONE.fetch_add(1, Ordering::AcqRel);
                    CHAIN_WQ.wake_all();
                }));
                if last.is_err() {
                    fail("csw submit", &XferResult::failed(UsbError::Busy, 0));
                }
            }));
            if next.is_err() {
                fail("data submit", &XferResult::failed(UsbError::Busy, 0));
            }
        })).map_err(|e| format!("CBW {i}: {e:?}"))?;
        let ok = CHAIN_WQ.wait_until_timeout(|| CHAIN_DONE.load(Ordering::Acquire) > i || CHAIN_FAILED.load(Ordering::Acquire) > 0, 3 * T);
        kassert!(ok && CHAIN_FAILED.load(Ordering::Acquire) == 0, "command {i}: chain did not finish ({} ms)", (time::now_ns() - start) / MS);
        let s = read_all(&csw, 13);
        kassert!(&s[0..4] == b"USBS" && u32::from_le_bytes([s[4], s[5], s[6], s[7]]) == 0x1000 + i && s[12] == 0, "command {i}: CSW {s:x?}");
        let got = read_all(&data, 64 * 512);
        if got != reference {
            let at = got.iter().zip(&reference).position(|(a, b)| a != b).unwrap_or(0);
            return Err(format!("command {i}: data differs at {at:#x}: {:x?} vs {:x?}", &got[at..(at + 16).min(got.len())],
                &reference[at..(at + 16).min(reference.len())]));
        }
    }
    Ok(())
}

/// QEMU's usb-storage stalls an IN token while it waits for a CBW: the transfer ends with Stall, the endpoint is
/// halted; `clear_halt` (CLEAR_FEATURE to the device, Reset Endpoint, dequeue past it) brings it back and the next
/// command works. Then the toggle reset of a running endpoint (Stop + Configure Endpoint drop/add).
fn stall() -> TestResult {
    kassert!(settled(), "hotplug did not settle");
    let sticks = Bot::sticks();
    if sticks.is_empty() {
        return ktest::skip("no unbound USB mass storage device");
    }
    if !hal::cpu::detect().hypervisor {
        return ktest::skip("the stall path relies on QEMU's usb-storage");
    }
    for bot in sticks {
        let path = bot.d.path();
        stall_one(bot).map_err(|e| format!("{path}: {e}"))?;
    }
    Ok(())
}
ktest!("us.xfer.stall", QemuOnly, stall);

fn stall_one(mut bot: Bot) -> TestResult {
    bot.ready()?;
    let c = controllers().into_iter().find(|c| c.name() == bot.d.host_name()).ok_or("controller")?;
    let buf = bot.d.alloc_buffer(512).map_err(|e| format!("{e:?}"))?;
    let behind = bot.d.alloc_buffer(13).map_err(|e| format!("{e:?}"))?;
    let failed = bot.d.submit(bot.ep_in, Request::new(buf.clone(), 0, 512)).map_err(|e| format!("{e:?}"))?;
    let r = failed.wait(T);
    kassert!(r.error == Some(UsbError::Stall), "IN before a CBW: {r:?} (QEMU stalls it)");
    // Queued behind the halt: it must not run until the endpoint is cleared.
    let queued = bot.d.submit(bot.ep_in, Request::new(behind.clone(), 0, 13)).map_err(|e| format!("{e:?}"))?;
    sched::sleep_ns(50 * MS);
    kassert!(!queued.is_done(), "a transfer ran on a halted endpoint");
    kassert!(queued.cancel().error == Some(UsbError::Cancelled), "cancel on the halted endpoint");
    bot.d.clear_halt(bot.ep_in).map_err(|e| format!("clear_halt: {e:?}"))?;
    let (n, st, _) = bot.command_in(&[0x12, 0, 0, 0, 36, 0], &buf, 36)?;
    kassert!(n == 36 && st == 0, "INQUIRY after the stall: {n} bytes, status {st}");
    // A running endpoint: Stop Endpoint + Configure Endpoint drop/add (toggle 0), the next command works.
    bot.d.host().reset_endpoint(bot.d.slot, bot.ep_out).map_err(|e| format!("reset OUT: {e:?}"))?;
    bot.d.host().reset_endpoint(bot.d.slot, bot.ep_in).map_err(|e| format!("reset IN: {e:?}"))?;
    let (n, st, _) = bot.command_in(&read10(0, 1), &buf, 512)?;
    if st != 0 {
        let (k, _, _) = bot.command_in(&[0x03, 0, 0, 0, 18, 0], &buf, 18)?;
        let sense = read_all(&buf, k);
        return Err(format!("READ after the resets: {n} bytes, status {st}, sense {sense:x?}"));
    }
    kassert!(n == 512, "READ after the resets: {n} bytes");
    kassert!(c.slot_info(bot.d.slot).is_some(), "slot gone");
    Ok(())
}

fn set_idle(d: &Device, iface: u8, four_ms: u8) -> Result<(), UsbError> {
    d.control(Setup::new(0x21, 0x0A, (four_ms as u16) << 8, iface as u16, 0), usb_core::Data::None).map(|_| ())
}

/// A boot keyboard with idle 0 has nothing to say (QEMU NAKs): the transfer times out, is stopped and skipped;
/// with idle 4 ms the next transfer gets a report.
fn timeout() -> TestResult {
    kassert!(settled(), "hotplug did not settle");
    let Some((d, iface, eps)) = find(3, 1, 1) else { return ktest::skip("no unbound USB boot keyboard (scenario us/40-xfer.sh)") };
    let ep = eps.iter().find(|e| e.is_in() && e.kind() == EpKind::Interrupt).ok_or("no interrupt IN")?.address;
    set_idle(&d, iface, 0).map_err(|e| format!("SET_IDLE 0: {e:?}"))?;
    // Drain what the keyboard still has (its first report after configuration).
    let buf = d.alloc_buffer(8).map_err(|e| format!("{e:?}"))?;
    for _ in 0..4 {
        if d.transfer(ep, Request::new(buf.clone(), 0, 8), 100 * MS).error == Some(UsbError::Timeout) {
            break;
        }
    }
    let t0 = time::now_ns();
    let r = d.transfer(ep, Request::new(buf.clone(), 0, 8), 200 * MS);
    let waited = (time::now_ns() - t0) / MS;
    kassert!(r == XferResult::failed(UsbError::Timeout, 0), "idle keyboard: {r:?}");
    kassert!(waited >= 190, "returned after {waited} ms");
    set_idle(&d, iface, 1).map_err(|e| format!("SET_IDLE 4 ms: {e:?}"))?;
    let r = d.transfer(ep, Request::new(buf.clone(), 0, 8), 2000 * MS);
    kassert!(r.error.is_none() && r.actual == 8, "report after the timeout: {r:?}");
    set_idle(&d, iface, 0).map_err(|e| format!("SET_IDLE 0: {e:?}"))?;
    Ok(())
}
ktest!("us.xfer.timeout", QemuOnly, timeout);

struct PipeLog {
    reports: AtomicU64,
    stopped: AtomicU32,
    errors: AtomicU32,
    bad_len: AtomicU32,
    last: SpinLock<Vec<u8>>,
    key_a: AtomicBool,
    tablet_moved: AtomicBool,
}

impl PipeLog {
    fn new() -> Arc<PipeLog> {
        Arc::new(PipeLog {
            reports: AtomicU64::new(0),
            stopped: AtomicU32::new(0),
            errors: AtomicU32::new(0),
            bad_len: AtomicU32::new(0),
            last: SpinLock::named("ktest.pipe", Vec::new()),
            key_a: AtomicBool::new(false),
            tablet_moved: AtomicBool::new(false),
        })
    }
}

type HidPipe = (Arc<Device>, u8, usb_core::PipeHandle, Arc<PipeLog>);

/// Starts a pipe with idle 4 ms on the first unbound HID device of `protocol` (1 keyboard, 2 mouse/tablet).
fn hid_pipe(protocol: u8, min_len: usize, depth: usize) -> Result<Option<HidPipe>, String> {
    // QEMU's usb-tablet is subclass 0 protocol 0 (no boot interface); usb-kbd/usb-mouse are 1/1 and 1/2.
    let found = if protocol == 0 { find(3, 0, 0) } else { find(3, 1, protocol) };
    let Some((d, iface, eps)) = found else { return Ok(None) };
    let ep = eps.iter().find(|e| e.is_in() && e.kind() == EpKind::Interrupt).ok_or("no interrupt IN")?;
    let log = PipeLog::new();
    let l = log.clone();
    let sink = Arc::new(move |e: PipeEvent<'_>| match e {
        PipeEvent::Data(r) => {
            l.reports.fetch_add(1, Ordering::AcqRel);
            if r.len() < min_len {
                l.bad_len.fetch_add(1, Ordering::AcqRel);
            }
            if protocol == 1 && r.get(2..8).is_some_and(|k| k.contains(&0x04)) {
                l.key_a.store(true, Ordering::Release);
            }
            if protocol == 0 && r.len() >= 5 && (u16::from_le_bytes([r[1], r[2]]) > 0x1000 || u16::from_le_bytes([r[3], r[4]]) > 0x1000) {
                l.tablet_moved.store(true, Ordering::Release);
            }
            *l.last.lock() = r.to_vec();
        }
        PipeEvent::Error(_) => {
            l.errors.fetch_add(1, Ordering::AcqRel);
        }
        PipeEvent::Stopped(_) => {
            l.stopped.fetch_add(1, Ordering::AcqRel);
        }
    });
    let h = d.interrupt_pipe(ep.address, PipeOpts::new(ep.max_packet().max(8) as usize, depth), sink).map_err(|e| format!("pipe: {e:?}"))?;
    set_idle(&d, iface, 1).map_err(|e| format!("SET_IDLE: {e:?}"))?;
    Ok(Some((d, iface, h, log)))
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

/// Interrupt IN pipes with 4 TDs queued on usb-kbd and usb-tablet: idle reports every 4 ms keep coming (300+,
/// the ring past its Link TRB), then stop ends each pipe once.
fn hid_pipes() -> TestResult {
    kassert!(settled(), "hotplug did not settle");
    let kbd = hid_pipe(1, 8, 4)?;
    let tab = hid_pipe(0, 5, 4)?;
    if kbd.is_none() && tab.is_none() {
        return ktest::skip("no unbound usb-kbd or usb-tablet (scenario us/40-xfer.sh)");
    }
    for (name, p) in [("keyboard", &kbd), ("tablet", &tab)] {
        let Some((d, iface, h, log)) = p else { continue };
        let got = until(10_000 * MS, || log.reports.load(Ordering::Acquire) >= 300);
        let n = log.reports.load(Ordering::Acquire);
        kassert!(got, "{name}: {n} reports in 10 s with idle 4 ms");
        kassert!(log.bad_len.load(Ordering::Acquire) == 0 && log.errors.load(Ordering::Acquire) == 0, "{name}: short reports or errors");
        set_idle(d, *iface, 0).map_err(|e| format!("SET_IDLE 0: {e:?}"))?;
        h.stop().map_err(|e| format!("stop: {e:?}"))?;
        kassert!(until(1000 * MS, || log.stopped.load(Ordering::Acquire) == 1), "{name}: Stopped not delivered once");
        let after = log.reports.load(Ordering::Acquire);
        sched::sleep_ns(100 * MS);
        kassert!(log.reports.load(Ordering::Acquire) == after, "{name}: reports after stop");
        klog::event!(Info, "usb", "ktest.hid_pipe", device = d.path(), reports = n, last = format!("{:x?}", *log.last.lock()));
    }
    Ok(())
}
ktest!("us.xfer.hid_pipes", QemuOnly, hid_pipes);

/// With `host/tests/us/40-xfer.sh`: the script presses "a" and moves the tablet through QMP after
/// `ktest.hid_ready`; both arrive through the pipes.
fn hid_input() -> TestResult {
    if !pci::registry::opted_in("usb_xfer_test") {
        return ktest::skip("scenario only: host/tests/us/40-xfer.sh boots with usb_xfer_test=on and drives QMP");
    }
    kassert!(settled(), "hotplug did not settle");
    let (Some((kd, ki, kh, kl)), Some((td, ti, th, tl))) = (hid_pipe(1, 8, 2)?, hid_pipe(0, 5, 2)?) else {
        return Err("usb-kbd and usb-tablet expected".into());
    };
    klog::event!(Info, "usb", "ktest.hid_ready", keyboard = kd.path(), tablet = td.path(); "press a, move the tablet");
    let ok = until(60_000 * MS, || kl.key_a.load(Ordering::Acquire) && tl.tablet_moved.load(Ordering::Acquire));
    let _ = (set_idle(&kd, ki, 0), set_idle(&td, ti, 0), kh.stop(), th.stop());
    kassert!(ok, "key a seen: {}, tablet moved: {} (last {:x?})", kl.key_a.load(Ordering::Acquire),
        tl.tablet_moved.load(Ordering::Acquire), *tl.last.lock());
    klog::event!(Info, "usb", "ktest.hid_input_seen");
    Ok(())
}
ktest!("us.xfer.hid_input", QemuOnly, hid_input);
