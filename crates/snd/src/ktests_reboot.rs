//! SN-7: the reboot's Quiesce order with a stream of silence running (what a muted video leaves streaming). QEMU
//! has no HDMI codec, so the stream goes to hda-duplex's analog output; the controller side is the same code.

use crate::backend::{Port, Target};
use crate::ktests::{err, players, release_left, wait, SLACK_NS};
use crate::thread::Mixer;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::Quiesce;
use devmodel::audio::PcmFormat;
use hda::hw::Hw;
use hda::regs::*;
use hda::{Hda, HdaError};
use ktest::{kassert, ktest, TestResult};

const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };

fn running(d: &Hda) -> Vec<(usize, u32)> {
    let hw = &d.ctrl.hw;
    (0..d.ctrl.gcap().total()).filter(|&i| hw.rd8(sd(i) + SD_CTL) & SD_CTL_RUN != 0).map(|i| (i, hw.rd32(sd(i) + SD_LPIB))).collect()
}

fn lpibs(d: &Hda) -> Vec<u32> {
    (0..d.ctrl.gcap().total()).map(|i| d.ctrl.hw.rd32(sd(i) + SD_LPIB)).collect()
}

ktest!("sn.mix.reboot_order", QemuOnly, reboot_order);
/// A private mixer plays silence on the first analog controller; then the Quiesce steps in the order the MCP
/// `reboot` runs them (`devmodel::quiesce_all`, newest first: the mixer's flag, then the controller), then
/// the reset's sleeps (200 ms, the mixer thread free to run). The stream DMA and the rings stay stopped (the
/// position does not move), bus master off, the link out of reset (Linux's shutdown), the mixer closed its
/// output and opened nothing again. The controller is restarted afterwards for the next tests.
fn reboot_order() -> TestResult {
    release_left();
    let p = match players() {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Some(d) = hda::controllers().into_iter().find(|d| d.ctrl.name() == p[0].as_str()) else {
        return Err(format!("{} not in the controller list", p[0]));
    };
    if let Some(g) = crate::started() {
        kassert!(g.release(SLACK_NS), "the kernel's mixer kept an output open");
    }
    let m: Arc<Mixer> = Mixer::spawn("snd/mix-sn7", crate::thread::config(), crate::usbback::sys_backend).map_err(err("mixer"))?;
    let r = steps(&d, &m);
    m.stop();
    let up = d.restart();
    r?;
    up.map_err(|e| format!("restart: {e}"))?;
    kassert!(d.ctrl.state() == hda::State::Up);
    Ok(())
}

fn steps(d: &Arc<Hda>, m: &Mixer) -> TestResult {
    let dev = d.ctrl.name();
    m.set_target(Target::Fixed { dev: dev.into(), port: Port::Jack });
    let n = 96_000;
    let x = m.open(F48, n as u32, "ktest.sn7").map_err(err("open"))?;
    kassert!(x.write(&alloc::vec![0u8; n * 4]) == Ok(n * 4));
    x.start();
    kassert!(wait(|| m.status().endpoint.is_some_and(|e| e.starts_with(dev)), SLACK_NS), "the silence never reached {dev}");
    kassert!(wait(|| !running(d).is_empty(), SLACK_NS), "no stream DMA running on {dev}");
    let before = running(d);
    // The reboot's order: newest registration first.
    m.quiesce_handle().quiesce();
    kassert!(devmodel::quiesce_matching(dev) == 1, "{dev} not registered for Quiesce");
    kassert!(running(d).is_empty(), "stream DMA still running right after Quiesce: {:?}", running(d));
    // The reset's sleeps: the mixer thread wakes, sees its flag and winds down.
    sched::sleep_ms(200);
    let pos = lpibs(d);
    sched::sleep_ms(50);
    let hw = &d.ctrl.hw;
    kassert!(running(d).is_empty(), "the stream runs again after Quiesce: {:?}", running(d));
    kassert!(lpibs(d) == pos, "a position still moves after Quiesce");
    kassert!(hw.rd8(CORBCTL) & CORBCTL_RUN == 0 && hw.rd8(RIRBCTL) & RIRBCTL_DMA == 0, "rings running after Quiesce");
    kassert!(hw.rd32(GCTL) & GCTL_CRST != 0, "the link was reset (Linux's shutdown leaves it)");
    kassert!(hw.pci().command() & 0x4 == 0, "bus master on after Quiesce");
    kassert!(d.ctrl.state() == hda::State::Quiesced);
    // SN-12: the engines seen stopped and the codec shut down (asserted here: the log's tail may be lost).
    let q = d.ctrl.last_quiesce().ok_or("no quiesce note")?;
    kassert!(q.was == hda::State::Up && !q.stuck, "{q:?}");
    kassert!(matches!(q.codec, hda::CodecStop::Done | hda::CodecStop::NoPowerStates), "codec {:?}", q.codec);
    kassert!(m.stopped());
    let st = m.status();
    kassert!(st.streams == 0 && st.endpoint.is_none(), "the mixer still has an output: {} {:?}", st.streams, st.endpoint);
    if let Some(out) = d.pick(devmodel::audio::Output::Jack) {
        kassert!(d.ctrl.open(out, F48, 4096, 1024) == Err(HdaError::NotUp), "a quiesced controller opened a stream");
    }
    klog::event!(Info, "ktest", "mix.reboot_order", ctrl = dev, streams_before = before.len() as u64, opens = st.opens);
    drop(x);
    Ok(())
}
