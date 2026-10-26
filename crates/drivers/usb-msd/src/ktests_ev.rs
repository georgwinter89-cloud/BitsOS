//! US-ev (B4) in QEMU: HID reports carry the controller interrupt's time and reach their sink on the KernelRt
//! event thread, and stick reads go on while the HID pipes flood. Needs HID devices no driver took
//! (`usbhid_raw=on`; `host/tests/us/60-ev.sh` adds usb-kbd, usb-tablet and usb-mouse). Timing is logged, never
//! judged here: the Mini-PC check list compares the speeds.

use crate::ktests::{now, read, ready_disks, settle, Crc32};
use crate::UsbDisk;
use devmodel::block::BlockDevice;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ktest::{kassert, ktest, TestResult};
use usb_core::{registry, Data, Device, EpKind, PipeEvent, PipeHandle, PipeOpts, Setup};

const MS: u64 = 1_000_000;
const MIB: usize = 1 << 20;

/// What one pipe's sink saw.
#[derive(Default)]
struct Flow {
    reports: AtomicU64,
    /// A stamp earlier than the one before it.
    back: AtomicU64,
    /// A stamp before the pipe started or after the moment it was delivered.
    outside: AtomicU64,
    /// Delivered on a thread that is not KernelRt.
    off_rt: AtomicU64,
    last: AtomicU64,
    lag_max: AtomicU64,
    lag_sum: AtomicU64,
}

struct Pipe {
    dev: Arc<Device>,
    iface: u8,
    handle: PipeHandle,
    flow: Arc<Flow>,
}

fn set_idle(d: &Device, iface: u8, four_ms: u8) -> Result<(), String> {
    d.control(Setup::new(0x21, 0x0A, (four_ms as u16) << 8, iface as u16, 0), Data::None).map(|_| ()).map_err(|e| format!("SET_IDLE: {e:?}"))
}

/// Timed pipes with idle 4 ms (a report every 4 ms without input) on every HID interface no driver took.
fn start_pipes() -> Result<Vec<Pipe>, String> {
    let mut out = Vec::new();
    for d in registry::devices() {
        if !registry::bindings_of(d.id).is_empty() {
            continue;
        }
        let Some(cfg) = d.config.as_ref() else { continue };
        let found: Vec<_> = cfg.defaults().filter(|f| f.class == 3).filter_map(|f| {
            f.endpoints.iter().find(|e| e.is_in() && e.kind() == EpKind::Interrupt).map(|e| (f.number, *e))
        }).collect();
        for (iface, ep) in found {
            let flow = Arc::new(Flow::default());
            let (f, start) = (flow.clone(), now());
            let sink = Arc::new(move |e: PipeEvent<'_>, at: u64| {
                if !matches!(e, PipeEvent::Data(_)) {
                    return;
                }
                let t = now();
                f.reports.fetch_add(1, Ordering::AcqRel);
                if at < f.last.swap(at, Ordering::AcqRel) {
                    f.back.fetch_add(1, Ordering::AcqRel);
                }
                if at < start || at > t {
                    f.outside.fetch_add(1, Ordering::AcqRel);
                }
                if sched::current_thread().map(|th| th.class()) != Some(sched::Class::KernelRt) {
                    f.off_rt.fetch_add(1, Ordering::AcqRel);
                }
                let lag = t.saturating_sub(at);
                f.lag_max.fetch_max(lag, Ordering::AcqRel);
                f.lag_sum.fetch_add(lag, Ordering::AcqRel);
            });
            let len = (ep.max_packet() as usize).max(8);
            let handle = d.interrupt_pipe_at(ep.address, PipeOpts::new(len, 4), sink).map_err(|e| format!("{}: pipe: {e:?}", d.path()))?;
            set_idle(&d, iface, 1)?;
            out.push(Pipe { dev: d.clone(), iface, handle, flow });
        }
    }
    Ok(out)
}

fn stop_pipes(pipes: &[Pipe]) {
    for p in pipes {
        let _ = set_idle(&p.dev, p.iface, 0);
        let _ = p.handle.stop();
    }
}

fn check(pipes: &[Pipe], min_reports: u64) -> TestResult {
    for p in pipes {
        let f = &p.flow;
        let n = f.reports.load(Ordering::Acquire);
        let name = format!("{} if{}", p.dev.path(), p.iface);
        klog::event!(Info, "usb", "ktest.ev_stamps", device = name.as_str(), reports = n,
            lag_max_us = f.lag_max.load(Ordering::Acquire) / 1000, lag_avg_us = f.lag_sum.load(Ordering::Acquire) / n.max(1) / 1000);
        kassert!(n >= min_reports, "{name}: {n} reports (want {min_reports})");
        kassert!(f.back.load(Ordering::Acquire) == 0, "{name}: {} stamps went back", f.back.load(Ordering::Acquire));
        kassert!(f.outside.load(Ordering::Acquire) == 0, "{name}: {} stamps outside start..delivery", f.outside.load(Ordering::Acquire));
        kassert!(f.off_rt.load(Ordering::Acquire) == 0, "{name}: {} reports delivered off the KernelRt thread", f.off_rt.load(Ordering::Acquire));
    }
    Ok(())
}

fn until(ns: u64, f: impl Fn() -> bool) -> bool {
    let deadline = now().saturating_add(ns);
    while now() < deadline {
        if f() {
            return true;
        }
        sched::sleep_ns(10 * MS);
    }
    f()
}

// ---- us.ev.stamps

fn stamps() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let pipes = start_pipes()?;
    if pipes.is_empty() {
        return ktest::skip("no unbound HID device (usbhid_raw=on, scenario us/60-ev.sh)");
    }
    // `check` names the pipe that fell short if 10 s were not enough.
    until(10_000 * MS, || pipes.iter().all(|p| p.flow.reports.load(Ordering::Acquire) >= 200));
    stop_pipes(&pipes);
    check(&pipes, 200)
}
ktest!("us.ev.stamps", QemuOnly, stamps);

// ---- us.ev.flood_read

/// 64 MiB (or the whole disk) in 1 MiB reads: (checksum, KiB/s inside read_blocks).
fn read_span(d: &UsbDisk) -> Result<(u32, u64), String> {
    let info = d.info();
    let bs = info.block_size as usize;
    let span = (info.blocks.saturating_mul(bs as u64).min(64 * MIB as u64) as usize) / bs * bs;
    let (mut crc, mut busy, mut at) = (Crc32::new(), 0u64, 0usize);
    while at < span {
        let k = (span - at).min(MIB);
        let t0 = now();
        let v = read(d, (at / bs) as u64, k)?;
        busy += now().saturating_sub(t0);
        crc.update(&v);
        at += k;
    }
    Ok((crc.value(), (span as u64 * 1_000_000_000 / busy.max(1)) / 1024))
}

fn flood_read() -> TestResult {
    kassert!(settle(), "USB did not settle");
    let Some(d) = ready_disks().into_iter().next() else { return ktest::skip("no USB disk with a medium") };
    let (crc_quiet, quiet) = read_span(&d)?;
    let pipes = start_pipes()?;
    if pipes.is_empty() {
        return ktest::skip("no unbound HID device (usbhid_raw=on, scenario us/60-ev.sh)");
    }
    let before: u64 = pipes.iter().map(|p| p.flow.reports.load(Ordering::Acquire)).sum();
    let r = read_span(&d);
    let during = pipes.iter().map(|p| p.flow.reports.load(Ordering::Acquire)).sum::<u64>() - before;
    stop_pipes(&pipes);
    let (crc_flood, flood) = r?;
    klog::event!(Info, "usb", "ktest.ev_flood_read", disk = String::from(d.name()), quiet_kib_per_s = quiet, flood_kib_per_s = flood,
        percent = flood * 100 / quiet.max(1), reports_during = during, pipes = pipes.len());
    kassert!(crc_flood == crc_quiet, "{}: the read under input gave other bytes ({crc_flood:#x} vs {crc_quiet:#x})", d.name());
    kassert!(during > 0, "no report arrived while the stick was read");
    check(&pipes, 1)
}
ktest!("us.ev.flood_read", QemuOnly, flood_read);
