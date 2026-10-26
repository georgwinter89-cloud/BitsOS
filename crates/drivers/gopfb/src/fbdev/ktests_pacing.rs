//! In-kernel tests of DP-3 (`dp.pacing.*`): the exact refresh from EDID and CONFIG on a test device (the live
//! clock is never changed: on the Mini-PC it carries the real rate), the pacing ioctl's answer on the live
//! device, and `abi-flippace` measuring from a program. Safe and rerunnable; timing is judged on hardware only
//! (the measured period within 5 % of the clock's), never under QEMU.

use super::ktests::{fb, finish, text, MS, SEC};
use super::{pacing, vblank_rate, vblanks};
use crate::{GopFb, Layout, RamTarget};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::display::DisplayDevice;
use display::edid::sample;
use display::RefreshSource;
use ktest::{kassert, ktest, TestResult};
use proc::stdio::{Capture, Input};
use proc::SpawnOptions;

const FLIPPACE: &str = "/bin/abi-flippace";

/// EDID and CONFIG on a 640x480 test device: the EDID timing sets the exact period, CONFIG wins over it, a broken
/// or unmatched EDID changes nothing.
fn edid_rate() -> TestResult {
    let t = Arc::new(RamTarget::new(640 * 480 * 4, 0));
    let d = GopFb::new(t, 640, 480, 640, Layout::Bgrx, "ram").map_err(|e| format!("{e:?}"))?;
    kassert!(d.vblank_rate() == (60_000, 16_666_666, RefreshSource::Default) && d.current_mode().refresh_mhz == 0, "{:?}", d.vblank_rate());
    // VGA 640x480: 25.175 MHz (stored as 25.17), 800 x 525.
    let vga = sample::dtd(2517, 640, 160, 480, 45, 16, 96, 10, 2, 0x18);
    let fhd = sample::dtd(14850, 1920, 280, 1080, 45, 88, 44, 4, 5, 0x1e);
    kassert!(!d.apply_edid(&sample::edid(&[fhd], 0)), "an EDID without a 640x480 timing was taken");
    let mut broken = sample::edid(&[vga], 0);
    broken[60] ^= 1;
    kassert!(!d.apply_edid(&broken), "an EDID with a bad checksum was taken");
    kassert!(!d.apply_edid(&broken[..100]), "a short EDID was taken");
    // 640x480 at 59.93 Hz in the base block and at 72.81 Hz (31.5 MHz, 832 x 520) after it: GOP may run either.
    let vga72 = sample::dtd(3150, 640, 192, 480, 40, 24, 40, 9, 3, 0x18);
    kassert!(!d.apply_edid(&sample::edid(&[vga, vga72], 0)), "an EDID with two rates at 640x480 was taken");
    kassert!(d.vblank_rate().2 == RefreshSource::Default, "a refused EDID changed the rate: {:?}", d.vblank_rate());
    let before = d.last_vblank();
    kassert!(d.apply_edid(&sample::edid(&[fhd, vga], 0)), "the 640x480 timing was not found");
    // 25,170,000 / (800 * 525) = 59.9286 Hz; 420,000 / 25.17 MHz = 16,686,532.38 ns.
    kassert!(d.vblank_rate() == (59_929, 16_686_532, RefreshSource::Edid), "EDID rate {:?}", d.vblank_rate());
    kassert!(d.current_mode().refresh_mhz == 59_929);
    kassert!(d.last_vblank().sequence >= before.sequence, "the sequence went back at the switch");
    let a = d.last_vblank();
    sched::sleep_ns(40 * MS);
    let b = d.last_vblank();
    kassert!((b.time_ns - a.time_ns) % 16_686_532 == 0 && b.sequence > a.sequence, "vblanks {a:?} -> {b:?} not on the EDID period");
    let cfg = |k: &str| (k == "display_refresh").then(|| String::from("59.95"));
    d.apply_config(&cfg);
    kassert!(d.vblank_rate() == (59_950, 16_680_567, RefreshSource::Config), "CONFIG over EDID: {:?}", d.vblank_rate());
    // An EDID that comes after CONFIG (another boot order, a Reload) does not replace the explicit rate.
    kassert!(!d.apply_edid(&sample::edid(&[vga], 0)), "an EDID replaced CONFIG");
    kassert!(d.vblank_rate() == (59_950, 16_680_567, RefreshSource::Config), "EDID after CONFIG: {:?}", d.vblank_rate());
    Ok(())
}
ktest!("dp.pacing.edid", Safe, edid_rate);

fn word(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn quad(b: &[u8], at: usize) -> u64 {
    let mut q = [0u8; 8];
    q.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(q)
}

/// FBIO_BITSOS_PACING through the device: the live clock's rate and source, vblanks one period apart around
/// now, the sequence moving on across FBIO_WAITFORVSYNC.
fn ioctl() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    let io = super::CallerIo(None);
    let before = time::now_ns();
    let b = fb.ioctl(pacing::FBIO_BITSOS_PACING, 0, &io, None).map_err(|e| format!("pacing ioctl: {e:?}"))?;
    let after = time::now_ns();
    kassert!(b.len() == pacing::SIZE, "answer of {} bytes", b.len());
    let (mhz, period, source) = vblank_rate();
    kassert!(word(&b, 0) == pacing::VERSION, "version {}", word(&b, 0));
    let flags = word(&b, 4);
    // GX-3: a flipping display engine's vblank is its own (source Hardware), GOP's an estimate.
    kassert!(
        (flags & pacing::VBLANK_ESTIMATED != 0) == (source != RefreshSource::Hardware),
        "flags {flags:#x}, source {source:?}"
    );
    kassert!((flags & pacing::RATE_KNOWN != 0) == (source != RefreshSource::Default), "flags {flags:#x}, source {source:?}");
    kassert!(word(&b, 8) == mhz && word(&b, 12) == source as u32 && quad(&b, 16) == period, "rate {}/{}/{} vs {mhz}/{source:?}/{period}", word(&b, 8), word(&b, 12), quad(&b, 16));
    let (seq, vb, next) = (quad(&b, 24), quad(&b, 32), quad(&b, 40));
    kassert!(vb <= after && next == vb + period && next + period > before, "vblank {vb} next {next} period {period}, call {before}..{after}");
    klog::event!(Info, "dp", "ktest.pacing", refresh_mhz = mhz, period_ns = period, source = source.name(), vblank_seq = seq,
        present_seq = quad(&b, 48));
    kassert!(fb.ioctl(abi_wait(), 0, &io, None) == Ok(Vec::new()), "FBIO_WAITFORVSYNC");
    let woke = time::now_ns();
    kassert!(woke - after < SEC, "one vblank wait took {} ms", (woke - after) / MS);
    let (last, _) = vblanks().ok_or("no vblank clock")?;
    kassert!(last.sequence > seq && last.time_ns <= woke, "after the wait the last vblank is {last:?}, before it was {seq}");
    Ok(())
}
ktest!("dp.pacing.ioctl", Safe, ioctl);

fn abi_wait() -> u64 {
    super::abi::FBIO_WAITFORVSYNC
}

/// Runs `abi-flippace` with `argv` and `env` to its end; its stdout.
fn flippace(argv: &[&str], env: &[&str]) -> Result<String, String> {
    let out = Capture::new(1 << 16);
    let err = Capture::new(1 << 16);
    let mut o = SpawnOptions::new(argv, env, policy::ProgramId(1));
    o.stdin = Some(Input::new(Vec::new()) as Arc<dyn vfs::File>);
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(err.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(FLIPPACE, &o).map_err(|e| format!("spawn {FLIPPACE}: {e}"))?;
    let s = super::ktests::Started { p, out, err };
    let code = finish(&s, 120 * SEC)?;
    let got = text(&s.out);
    kassert!(code == 0, "abi-flippace ended with {code}:\n{got}\n{}", text(&s.err));
    for l in got.lines() {
        klog::event!(Info, "dp", "flippace.line", text = l);
    }
    Ok(got)
}

/// N of the line "measured: vsync period N ns = ...".
fn measured_period(got: &str) -> Option<u64> {
    let l = got.lines().find_map(|l| l.strip_prefix("measured: vsync period "))?;
    l.split(' ').next()?.parse().ok()
}

fn has_line(got: &str, start: &str) -> TestResult {
    kassert!(got.lines().any(|x| x.starts_with(start)), "no '{start}' line:\n{got}");
    Ok(())
}

/// `abi-flippace run`: a program measures the pacing on fb0 (vsync and free rounds) and reads the pacing ioctl;
/// the kernel checks what it says that does not depend on timing. A second run takes Linux's path (no pacing
/// ioctl: the period is measured from the waits alone).
fn program() -> TestResult {
    if display::device().is_none() {
        return ktest::skip("no display (headless)");
    }
    let fb = fb()?;
    if vfs::root_ns().lookup(FLIPPACE).is_err() {
        // Images built without the musl sysroot have no user/abi-tests (MC-5b, RB-7a requests).
        return ktest::skip("/bin/abi-flippace is not in this image (user/abi-tests need the musl sysroot)");
    }
    super::fix_node_owner();
    let (mhz, period, source) = vblank_rate();
    let pacing_line = format!("pacing: version 1, refresh {mhz} mHz, period {period} ns, source {}", source.name());
    let got = flippace(&["abi-flippace", "info"], &["HOME=/", "PATH=/bin"])?;
    has_line(&got, &pacing_line)?;
    for l in ["var timing: none (pixclock 0)", "PACING(kernel address) = -EFAULT", "PACING(unmapped 0x1) = -EFAULT",
        "PACING(read-only page) = -EFAULT", "PACING with size 32 = -ENOTTY", "PACING as _IOW = -ENOTTY", "PACING as _IO = -ENOTTY"] {
        kassert!(got.lines().any(|x| x == l), "no '{l}' line:\n{got}");
    }
    kassert!(fb.holder_pid() == 0, "info (read-only, nothing drawn) took the screen");
    let got = flippace(&["abi-flippace", "run", "12", "0,4"], &["HOME=/", "PATH=/bin"])?;
    has_line(&got, &pacing_line)?;
    for l in ["pacing: next vblank one period after the last: yes", "pacing: vblank sequence moved on with each wait: yes",
        "pacing: present seen after each pan: yes", "pacing: vblank times on CLOCK_MONOTONIC: yes"] {
        kassert!(got.lines().any(|x| x == l), "no '{l}' line:\n{got}");
    }
    for start in ["vsync logic 0 ms:", "vsync logic 4 ms:", "free logic 0 ms:", "free logic 4 ms:", "measured: vsync period "] {
        has_line(&got, start)?;
    }
    // The waits hold the clock's period: judged on hardware only (5 %, the median of back-to-back waits); under
    // a hypervisor the number is logged for orientation.
    let measured = measured_period(&got).ok_or_else(|| format!("no measured period in:\n{got}"))?;
    let off = measured.abs_diff(period);
    klog::event!(Info, "dp", "ktest.flippace.period", measured_ns = measured, period_ns = period, off_ns = off);
    if !hal::serial::under_hypervisor() {
        kassert!(off * 20 <= period, "measured vsync period {measured} ns, the clock's is {period} ns");
    }
    super::ktests::handed_back(fb, "abi-flippace ended")?;
    let got = flippace(&["abi-flippace", "run", "8", "0"], &["HOME=/", "PATH=/bin", "FLIPPACE_MEASURE_ONLY=1"])?;
    for start in ["pacing: not asked", "vsync logic 0 ms:", "free logic 0 ms:", "measured: vsync period "] {
        has_line(&got, start)?;
    }
    kassert!(!got.contains("pacing: next vblank"), "pacing checks without the ioctl:\n{got}");
    super::ktests::handed_back(fb, "the second abi-flippace run")?;
    klog::event!(Info, "dp", "ktest.flippace", refresh_mhz = mhz, period_ns = period, source = source.name());
    Ok(())
}
ktest!("dp.pacing.flippace", Safe, program);
