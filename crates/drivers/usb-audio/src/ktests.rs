//! In-kernel tests of the binding (no sound): `sn.usb.bound` (every bound function, its settings made at
//! probe) and `sn.usb.capture` (records 1 s from the first microphone, e.g. the JBL's chat mic; QEMU's
//! usb-audio has none: skip). Both `Safe` and rerunnable. Playback tests are in `snd` (`sn.usb.tone`,
//! `sn.usb.unplug`), through the mixer.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ktest::{TestResult, kassert, ktest};

static SETTLED: AtomicBool = AtomicBool::new(false);

/// Waits (once per boot) until no USB device came or went for 1 s, so devices plugged at boot are bound
/// before a test looks (enumeration runs in the controllers' hotplug workers; usb-hid's ktests do the same).
pub fn settle() {
    if SETTLED.load(Ordering::Acquire) {
        return;
    }
    let deadline = time::now_ns().saturating_add(15_000_000_000);
    let mut last = (usb_core::registry::generation(), time::now_ns());
    while time::now_ns() < deadline {
        sched::sleep_ms(50);
        let g = usb_core::registry::generation();
        if g != last.0 {
            last = (g, time::now_ns());
        } else if time::now_ns().saturating_sub(last.1) >= 1_000_000_000 {
            break;
        }
    }
    SETTLED.store(true, Ordering::Release);
}

ktest!("sn.usb.bound", Safe, bound);
fn bound() -> TestResult {
    settle();
    let f = crate::dev::functions();
    if f.is_empty() {
        return ktest::skip("no USB audio device bound (QEMU: -device usb-audio on a qemu-xhci)");
    }
    for x in &f {
        for s in &x.playback {
            let alt = x.dev.current_alt(s.iface);
            kassert!(alt == Some(s.alt), "{} interface {}: alternate setting {alt:?}, want {}", x.dev.path(), s.iface, s.alt);
        }
        let list = |v: &[crate::plan::StreamPlan]| v.iter().map(|s| format!("{}/{}@{:#x}x{}", s.iface, s.alt, s.ep.address, s.channels)).collect::<Vec<String>>().join(",");
        klog::event!(Info, "ktest", "usb_audio.bound", device = x.dev.path(), control = x.control, playback = list(&x.playback).as_str(),
            capture = list(&x.capture).as_str(), mute = crate::hw_mute());
    }
    kassert!(!crate::outputs().is_empty() || !crate::inputs().is_empty());
    Ok(())
}

ktest!("sn.usb.capture", Safe, capture);
fn capture() -> TestResult {
    settle();
    let Some(i) = crate::inputs().into_iter().next() else {
        return ktest::skip("no USB microphone (QEMU's usb-audio has none)");
    };
    let h = crate::open_capture(&i.key, i.iface, 48_000, None).map_err(|e| format!("open: {e:?}"))?;
    // A second open of the same input is refused: its close would set the interface to 0 under the first.
    let second = crate::open_capture(&i.key, i.iface, 48_000, None);
    let busy = matches!(second, Err(usb_core::UsbError::Busy));
    if let Ok(x) = second {
        x.close();
    }
    sched::sleep_ms(1000);
    let mut buf = alloc::vec![[0i16; 2]; 48_000];
    let n = h.cap.read(&mut buf).map_err(|e| format!("read: {e:?}"))?;
    let peak = buf[..n].iter().map(|f| (f[0] as i32).abs()).max().unwrap_or(0);
    let (got, over) = h.cap.counts();
    let st = h.cap.stats();
    h.close();
    klog::event!(Info, "ktest", "usb_audio.capture", input = i.key.as_str(), interface = i.iface, frames = n as u64, captured = got,
        overruns = over, missed = st.missed, peak = peak as u64);
    kassert!(busy, "a second open of the same input was not refused as busy");
    kassert!(n > 0, "no frames in 1 s");
    Ok(())
}

ktest!("sn.usb.mute", Safe, mute);
/// Two threads flip the wanted mute and apply it 20 times each, racing: the hardware ends as the last state
/// asked for (read back with GET_CUR where the feature unit answers), then the state before the test returns.
fn mute() -> TestResult {
    use alloc::sync::Arc;
    settle();
    let f = crate::dev::functions();
    if f.iter().all(|x| x.playback.is_empty()) {
        return ktest::skip("no USB audio output (QEMU: -device usb-audio on a qemu-xhci)");
    }
    let before = crate::hw_mute();
    let want = Arc::new(AtomicBool::new(before));
    let mut hs = Vec::new();
    for t in 0..2u32 {
        let w = want.clone();
        let h = sched::spawn(&format!("ktest/uac-mute{t}"), sched::Class::Normal, move || {
            for i in 0..20u32 {
                w.store((i + t) % 3 == 0, Ordering::SeqCst);
                crate::set_hw_mute(&|| w.load(Ordering::SeqCst));
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    for h in &hs {
        kassert!(h.join_timeout(10_000_000_000), "a mute thread did not finish in 10 s");
    }
    let last = want.load(Ordering::SeqCst);
    let (hw, mut read) = (crate::hw_mute(), Vec::new());
    for x in &f {
        for s in &x.playback {
            read.push(crate::req::get_mute(&x.dev, s));
        }
    }
    // Whatever the race ended with: muted reads back as muted.
    crate::set_hw_mute(&|| !last);
    let mut flipped = Vec::new();
    for x in &f {
        for s in &x.playback {
            flipped.push(crate::req::get_mute(&x.dev, s));
        }
    }
    crate::set_hw_mute(&|| before);
    klog::event!(Info, "ktest", "usb_audio.mute", wanted = last, applied = hw, read = format!("{read:?}").as_str(),
        flipped = format!("{flipped:?}").as_str());
    kassert!(flipped.iter().all(|r| r.is_none_or(|m| m == !last)), "after asking {}: feature units read {flipped:?}", !last);
    kassert!(hw == last, "applied {hw}, last asked {last}");
    kassert!(read.iter().all(|r| r.is_none_or(|m| m == last)), "feature units read {read:?}, last asked {last}");
    kassert!(crate::hw_mute() == before);
    Ok(())
}
