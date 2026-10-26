//! USB audio through the kernel mixer (SN-8b): `sn.usb.tone` (Audible) plays 2 s of `testsig::c` on `Auto`,
//! which must pick the USB headset (QEMU: usb-audio into a WAV, checked by host/tests/sn/usbcheck.py);
//! `sn.usb.unplug` (QemuOnly) keeps a client playing while host/tests/sn/50-usbaudio.sh pulls the usb-audio
//! over QMP (`ktest.usb_playing`), waits for the mixer to be back on the jack (`ktest.usb_unplugged`), then
//! plugs it in again and waits for the mixer to follow (`ktest.usb_replugged`). Both skip without a USB output.

use crate::backend::Target;
use crate::ktests::{SLACK_NS, err, with_mixer};
use crate::testsig;
use alloc::string::String;
use devmodel::audio::PcmFormat;
use ktest::{TestResult, kassert, ktest};

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };

fn on_usb(m: &crate::Mixer) -> bool {
    m.status().endpoint.as_deref().is_some_and(|e| e.starts_with("usb:"))
}

ktest!("sn.usb.tone", Audible, tone);
fn tone() -> TestResult {
    usb_audio::ktest_settle();
    if usb_audio::outputs().is_empty() {
        return ktest::skip("no USB audio output (QEMU: -device usb-audio on a qemu-xhci)");
    }
    with_mixer(Target::Auto, |m| {
        let n = 96_000;
        let x = m.open(F48, n as u32, "ktest.usb").map_err(err("open"))?;
        kassert!(x.write_frames(&testsig::frames(testsig::c, 0, n)) == Ok(n));
        x.start();
        kassert!(crate::ktests::wait(|| on_usb(m) || x.state() != crate::State::Running, SLACK_NS), "never on the USB output");
        kassert!(on_usb(m), "Auto played on {:?}", m.status().endpoint);
        kassert!(x.drain(SLACK_NS), "drain: {}", x.state().name());
        let st = m.status();
        klog::event!(Info, "ktest", "usb_tone", endpoint = st.endpoint.as_deref().unwrap_or("none"), frames = n as u64, hw_underruns = st.hw_underruns);
        kassert!(x.underruns().0 == 0, "client underruns {:?}", x.underruns());
        Ok(())
    })
}

ktest!("sn.usb.unplug", QemuOnly, unplug);
fn unplug() -> TestResult {
    if !pci::registry::opted_in("usb_audio_unplug_test") {
        return ktest::skip("scenario only: host/tests/sn/50-usbaudio.sh boots with usb_audio_unplug_test=on and pulls the device");
    }
    usb_audio::ktest_settle();
    if usb_audio::outputs().is_empty() {
        return ktest::skip("no USB audio output");
    }
    if crate::ktests::players().is_err() {
        return ktest::skip("no jack to fall back to (QEMU: intel-hda + hda-duplex)");
    }
    with_mixer(Target::Auto, |m| {
        let x = m.open(F48, 48_000, "ktest.usb_unplug").map_err(err("open"))?;
        let mut next = 0u64;
        let mut feed = || -> Result<(), String> {
            let room = x.avail().min(4800);
            if room > 0 {
                let k = x.write_frames(&testsig::frames(testsig::c, next, room)).map_err(err("write"))?;
                next += k as u64;
            }
            Ok(())
        };
        feed()?;
        x.start();
        // Feeds the client like a player while waiting for `cond` (QEMU time: generous).
        let mut phase = |cond: &dyn Fn() -> bool, what: &str| -> TestResult {
            let end = time::now_ns() + 3 * SLACK_NS;
            while !cond() {
                feed()?;
                kassert!(time::now_ns() < end, "{what}: timed out on {:?}, client {}", m.status().endpoint, x.state().name());
                sched::sleep_ms(10);
            }
            Ok(())
        };
        phase(&|| on_usb(m), "on usb")?;
        let p = x.position();
        phase(&|| on_usb(m) && x.position() > p + 14_400, "300 ms on the headset")?;
        klog::event!(Info, "ktest", "usb_playing", endpoint = m.status().endpoint.as_deref().unwrap_or("none"), at = x.position());
        let p0 = x.position();
        phase(&|| m.status().endpoint.as_deref().is_some_and(|e| e.starts_with("hda")), "back on the jack")?;
        kassert!(x.state() == crate::State::Running, "client {}", x.state().name());
        klog::event!(Info, "ktest", "usb_unplugged", endpoint = m.status().endpoint.as_deref().unwrap_or("none"), at = x.position(), since = x.position() - p0);
        let p1 = x.position();
        phase(&|| x.position() > p1 + 4800, "the client plays on the jack")?;
        phase(&|| on_usb(m), "back on usb")?;
        klog::event!(Info, "ktest", "usb_replugged", endpoint = m.status().endpoint.as_deref().unwrap_or("none"), at = x.position());
        let p2 = x.position();
        phase(&|| x.position() > p2 + 9600, "the client plays on the headset again")?;
        x.start_drain();
        kassert!(x.drain(SLACK_NS), "drain: {}", x.state().name());
        klog::event!(Info, "ktest", "usb_unplug", frames = next, switches = m.status().switches, hw_underruns = m.status().hw_underruns,
            client_underruns = x.underruns().0);
        Ok(())
    })
}
