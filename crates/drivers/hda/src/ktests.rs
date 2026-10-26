//! In-kernel tests (SN-2). Under QEMU with `-device intel-hda -device hda-duplex,audiodev=..` and a `wav`
//! audiodev (`host/tests/sn/10-hda.sh`, `cargo xtask ktest sn.hda -- ..`); without a controller every test
//! ends as `KTEST skip` (the gate has none).
//! On the Mini-PC: `sn.hda.probe`, `sn.hda.dump` and `sn.hda.jack` are `Safe` (they read what bring-up kept
//! and the pin-sense verbs; the GPU controller is never touched by them); `sn.hda.play` makes sound
//! (`Audible`: refused there unless Georg allows); the restart tests are `QemuOnly`.

use crate::ctrl::{kind_name, HdaError, OutputId};
use crate::hw::Hw;
use crate::regs::*;
use crate::{signal, Hda};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::audio::{Output as Port, PcmFormat};
use ktest::{kassert, ktest, TestResult};

/// Generous: QEMU timing is never judged (CONTRACTS §4).
const SLACK_NS: u64 = 15_000_000_000;
const F48: PcmFormat = PcmFormat { rate: 48000, channels: 2, bits: 16 };

fn controllers() -> Result<Vec<Arc<Hda>>, TestResult> {
    let list = crate::controllers();
    if list.is_empty() {
        return Err(if pci::devices().iter().any(crate::is_hda) {
            Err("an HD Audio function is present but the driver kept none (see hda.probe_failed)".into())
        } else {
            ktest::skip("no HD Audio controller in this machine")
        });
    }
    Ok(list)
}

/// Controllers that can play, with the output `AudioDevice` would pick.
fn players() -> Result<Vec<(Arc<Hda>, OutputId)>, TestResult> {
    let v: Vec<(Arc<Hda>, OutputId)> = controllers()?.into_iter().filter_map(|d| d.pick(Port::Jack).map(|o| (d, o))).collect();
    if v.is_empty() {
        return Err(ktest::skip("no controller with a playable analog output"));
    }
    Ok(v)
}

ktest!("sn.hda.probe", Safe, probe);
fn probe() -> TestResult {
    let list = match controllers() {
        Ok(l) => l,
        Err(r) => return r,
    };
    for d in &list {
        let c = &d.ctrl;
        kassert!(c.state() == crate::State::Up, "{} is {}", c.name(), c.state().name());
        let outs = c.outputs();
        let mode = c.link_mode().map_or("none", |m| m.name());
        let irq = d.irq_info().map(|(m, n)| format!("{}:{n}", m.name())).unwrap_or_else(|| "polled".into());
        klog::event!(Info, "ktest", "hda.ctrl", ctrl = c.name(), kind = c.kind().name(), link = mode, codecs = c.codecs().len(),
            outputs = outs.len(), irq = irq.as_str(), snoop = c.snooped(), timeouts = c.link_stats().0);
        for o in &outs {
            klog::event!(Info, "ktest", "hda.out", ctrl = c.name(), cad = o.id.cad, pin = klog::Hex(o.id.pin as u64), kind = kind_name(o.kind),
                converter = klog::Hex(o.converter as u64), formats = o.formats.len(), playable = o.playable,
                plugged = match o.plugged { Some(true) => "yes", Some(false) => "no", None => "unknown" });
        }
        if c.read_only() {
            // The GPU controller is back in reset and never got bus master.
            kassert!(c.hw.rd32(GCTL) & GCTL_CRST == 0, "{} left out of reset", c.name());
            kassert!(c.hw.pci().command() & 0x4 == 0, "{} has bus master on", c.name());
            kassert!(outs.iter().all(|o| !o.playable));
        } else {
            kassert!(!c.codecs().is_empty(), "{}: no codec answered", c.name());
        }
    }
    Ok(())
}

ktest!("sn.hda.dump", Safe, dump);
/// Every codec's read walk (taken at bring-up) as `hdacodec` dump lines in the log (`ktest hda.dump`),
/// compared with SN-1's synthetic fixture where the vendor matches.
fn dump() -> TestResult {
    let list = match controllers() {
        Ok(l) => l,
        Err(r) => return r,
    };
    for d in &list {
        for c in d.ctrl.codecs() {
            let check = hdacodec::walk::check(&c.dump, c.cad);
            kassert!(check.complete(), "{} codec {}: walk incomplete {:?}", d.ctrl.name(), c.cad, check);
            let text = c.dump.to_text();
            for line in text.lines() {
                klog::event!(Info, "ktest", "hda.dump", ctrl = d.ctrl.name(), cad = c.cad, line = line);
            }
            let fixture = match c.model.vendor_id {
                0x10EC_0269 => Some(include_str!("../../../hdacodec/fixtures/alc269-synthetic.txt")),
                0x1002_AA01 => Some(include_str!("../../../hdacodec/fixtures/aa01-synthetic.txt")),
                _ => None,
            };
            if let Some(f) = fixture.and_then(|t| hdacodec::dump::Dump::parse(t).ok()) {
                let differ = c.dump.lines().iter().filter(|l| f.get(0, l.nid, l.verb) != Some(l.answer)).count();
                let same = hdacodec::model::Codec::read(&mut f.source(0), 0).ok().map(|m| hdacodec::path::outputs(&m)) == Some(c.outputs.clone());
                klog::event!(Info, "ktest", "hda.dump_compare", ctrl = d.ctrl.name(), cad = c.cad, reads = c.dump.lines().len(),
                    fixture_reads = f.lines().len(), differ = differ, same_paths = same);
            }
        }
    }
    Ok(())
}

ktest!("sn.hda.jack", Safe, jack);
/// Reads each sensing pin of the playable controllers again and compares with the state the driver keeps.
fn jack() -> TestResult {
    let list = match controllers() {
        Ok(l) => l,
        Err(r) => return r,
    };
    let mut pins = 0;
    for d in list.iter().filter(|d| !d.ctrl.read_only()) {
        for c in d.ctrl.codecs() {
            for o in c.outputs.iter() {
                let Some(j) = o.jack else { continue };
                if let Some(hdacodec::setup::Action::Set { nid, verb }) = j.before_read() {
                    kassert!(d.ctrl.verb(c.cad, nid, verb).is_ok(), "pin sense trigger");
                }
                let v = d.ctrl.verb(c.cad, j.pin, j.read_verb()).map_err(|e| format!("pin {:#x}: {e:?}", j.pin))?;
                let now = hdacodec::path::JackSense::plugged(v);
                let kept = d.ctrl.jack_present(OutputId { cad: c.cad, pin: o.pin });
                klog::event!(Info, "ktest", "hda.jack_read", ctrl = d.ctrl.name(), pin = klog::Hex(o.pin as u64), sense = klog::Hex(v as u64),
                    plugged = now, kept = alloc::format!("{kept:?}"), unsol = j.unsol);
                pins += 1;
                kassert!(kept.is_some(), "no state kept for pin {:#x}", o.pin);
            }
        }
    }
    if pins == 0 {
        return ktest::skip("no output with presence detect (QEMU's hda-duplex has none)");
    }
    Ok(())
}

struct Play {
    dev: Arc<Hda>,
    sid: u32,
    rate: u32,
    total: u64,
    sent: u64,
    periods: Arc<AtomicU64>,
    irqs: u64,
}

impl Play {
    /// Writes until the ring is full: the signal up to `total` frames, then silence.
    fn fill(&mut self, tone: bool) -> Result<(), HdaError> {
        loop {
            let n = 1024u64;
            let chunk = if self.sent < self.total { signal::bytes(self.sent, n.min(self.total - self.sent) as usize, tone) } else { alloc::vec![0u8; n as usize * 4] };
            let took = self.dev.ctrl.write(self.sid, &chunk)? as u64 / 4;
            self.sent += took;
            if took < chunk.len() as u64 / 4 {
                return Ok(());
            }
        }
    }
}

/// Opens every player (the first at 48 kHz, the second at 44.1 kHz, ...), plays `secs` of the test signal on
/// all at once, then silence until each played its signal plus a ring.
fn play_all(secs: u64, tone: bool) -> Result<Vec<Play>, String> {
    release_left();
    let list = players().map_err(|r| r.err().unwrap_or_default())?;
    let mut plays = Vec::new();
    for (i, (dev, out)) in list.into_iter().enumerate() {
        let rate = if i % 2 == 0 { 48000 } else { 44100 };
        let fmt = PcmFormat { rate, channels: 2, bits: 16 };
        let sid = dev.ctrl.open(out, fmt, 16384, 4096).map_err(|e| format!("{} open {out:?}: {e:?}", dev.ctrl.name()))?;
        let periods = Arc::new(AtomicU64::new(0));
        let p = periods.clone();
        let _ = dev.ctrl.set_period_callback(sid, Some(Arc::new(move |_, _| {
            p.fetch_add(1, Ordering::AcqRel);
        })));
        let irqs = dev.ctrl.irq_count();
        plays.push(Play { dev, sid, rate, total: rate as u64 * secs, sent: 0, periods, irqs });
    }
    for p in plays.iter_mut() {
        p.fill(tone).map_err(|e| format!("prefill: {e:?}"))?;
    }
    for p in &plays {
        p.dev.ctrl.start(p.sid).map_err(|e| format!("start: {e:?}"))?;
    }
    let t0 = time::now_ns();
    loop {
        let mut done = true;
        for p in plays.iter_mut() {
            p.fill(tone).map_err(|e| format!("write: {e:?}"))?;
            let pos = p.dev.ctrl.position(p.sid).unwrap_or(0);
            let (ring, _) = p.dev.ctrl.stream_layout(p.sid).unwrap_or((0, 0));
            done &= pos >= p.total + ring as u64;
        }
        if done {
            break;
        }
        if time::now_ns() - t0 > secs * 1_000_000_000 + SLACK_NS {
            let pos: Vec<u64> = plays.iter().map(|p| p.dev.ctrl.position(p.sid).unwrap_or(0)).collect();
            for p in &plays {
                let _ = p.dev.ctrl.close(p.sid);
            }
            return Err(format!("positions {pos:?} after {} ms", (time::now_ns() - t0) / 1_000_000));
        }
        sched::sleep_ns(5_000_000);
    }
    for p in &plays {
        let _ = p.dev.ctrl.stop(p.sid);
    }
    Ok(plays)
}

ktest!("sn.hda.play", Audible, play);
/// Two seconds of the test signal on every player at once (QEMU: the frame counter on the right for the WAV
/// check; on hardware a quiet 1 kHz tone on both sides).
fn play() -> TestResult {
    let tone = !hal::serial::under_hypervisor();
    let plays = match players() {
        Ok(_) => play_all(2, tone)?,
        Err(r) => return r,
    };
    for p in &plays {
        let c = &p.dev.ctrl;
        let periods = p.periods.load(Ordering::Acquire);
        let irqs = c.irq_count() - p.irqs;
        klog::event!(Info, "ktest", "hda.played", ctrl = c.name(), rate = p.rate, frames = p.total, position = c.position(p.sid).unwrap_or(0),
            periods = periods, irqs = irqs, irq_driven = c.irq_driven(), delay = c.delay(p.sid).unwrap_or(0));
        kassert!(periods >= 2, "{}: {periods} period callbacks", c.name());
        kassert!(!c.irq_driven() || irqs > 0, "{}: MSI armed but no interrupt", c.name());
    }
    for p in &plays {
        let _ = p.dev.ctrl.close(p.sid);
    }
    Ok(())
}

ktest!("sn.hda.quiesce", QemuOnly, quiesce);
/// Quiesce with a stream running: every DMA stops, the link is in reset, bus master is off; then the
/// controller comes up again and plays (the Reload path without a Reload).
fn quiesce() -> TestResult {
    release_left();
    let list = match players() {
        Ok(l) => l,
        Err(r) => return r,
    };
    for (d, out) in list {
        let c = &d.ctrl;
        let sid = c.open(out, F48, 4096, 1024).map_err(|e| format!("open: {e:?}"))?;
        c.start(sid).map_err(|e| format!("start: {e:?}"))?;
        kassert!(devmodel::quiesce_matching(c.name()) == 1);
        let running = (0..c.gcap().total()).filter(|&i| c.hw.rd8(sd(i) + SD_CTL) & SD_CTL_RUN != 0).count();
        kassert!(running == 0, "{running} descriptors still running");
        kassert!(c.hw.rd8(CORBCTL) & CORBCTL_RUN == 0 && c.hw.rd8(RIRBCTL) & RIRBCTL_DMA == 0, "rings still running");
        kassert!(c.hw.rd32(GCTL) & GCTL_CRST == 0, "link not in reset");
        kassert!(c.hw.pci().command() & 0x4 == 0, "bus master still on");
        kassert!(c.state() == crate::State::Quiesced);
        d.restart().map_err(|e| format!("restart: {e}"))?;
        kassert!(c.position(sid) == Err(HdaError::NoSuchStream), "old stream survived the restart");
        let sid = c.open(out, F48, 4096, 1024).map_err(|e| format!("open after restart: {e:?}"))?;
        c.write(sid, &signal::bytes(0, 4096, false)).map_err(|e| format!("{e:?}"))?;
        c.start(sid).map_err(|e| format!("{e:?}"))?;
        let t0 = time::now_ns();
        while c.position(sid).unwrap_or(0) < 1024 {
            kassert!(time::now_ns() - t0 < SLACK_NS, "no progress after restart");
            sched::sleep_ns(5_000_000);
        }
        c.close(sid).map_err(|e| format!("{e:?}"))?;
    }
    Ok(())
}

ktest!("sn.hda.stale", QemuOnly, stale);
/// A bring-up over a running stream (what a Reload leaves when Quiesce could not run) stops it first.
fn stale() -> TestResult {
    release_left();
    let list = match players() {
        Ok(l) => l,
        Err(r) => return r,
    };
    for (d, out) in list {
        let c = &d.ctrl;
        let sid = c.open(out, F48, 4096, 1024).map_err(|e| format!("{e:?}"))?;
        c.start(sid).map_err(|e| format!("{e:?}"))?;
        let _ = c.hw.pci().disable_device();
        d.restart().map_err(|e| format!("restart: {e}"))?;
        let running = (0..c.gcap().total()).filter(|&i| c.hw.rd8(sd(i) + SD_CTL) & SD_CTL_RUN != 0).count();
        kassert!(running == 0, "{running} descriptors running after bring-up");
        kassert!(c.state() == crate::State::Up);
    }
    Ok(())
}

/// The stream `sn.hda.leave_running` left (closed by its next run).
static LEFT: ksync::SpinLock<Vec<(Arc<Hda>, u32)>> = ksync::SpinLock::named("hda.ktest_left", Vec::new());

/// Closes what `sn.hda.leave_running` left, so every test that opens a stream runs in any order in one boot.
fn release_left() {
    let old: Vec<(Arc<Hda>, u32)> = core::mem::take(&mut *LEFT.lock());
    for (d, sid) in old {
        let _ = d.ctrl.close(sid);
    }
}

ktest!("sn.hda.leave_running", QemuOnly, leave_running);
/// Starts a looping stream on every player and returns with it running, for the Reload scenario
/// (`host/tests/sn/20-hda-reload.sh`): the old kernel's Quiesce must stop it.
fn leave_running() -> TestResult {
    release_left();
    let list = match players() {
        Ok(l) => l,
        Err(r) => return r,
    };
    for (d, out) in list {
        let sid = d.ctrl.open(out, F48, 4800, 1200).map_err(|e| format!("open: {e:?}"))?;
        d.ctrl.write(sid, &signal::bytes(0, 4800, true)).map_err(|e| format!("{e:?}"))?;
        d.ctrl.start(sid).map_err(|e| format!("{e:?}"))?;
        klog::event!(Info, "ktest", "hda.left_running", ctrl = d.ctrl.name(), stream = sid);
        LEFT.lock().push((d, sid));
    }
    // SN-9: an input stream too (its DMA writes into memory: Quiesce must stop it before the new kernel).
    for d in crate::controllers().into_iter().filter(|d| !d.ctrl.read_only()) {
        let Some(i) = d.ctrl.inputs().into_iter().find(|i| i.formats.contains(&F48)) else { continue };
        let sid = d.ctrl.open_input(i.id, F48, 4800, 1200).map_err(|e| format!("open_input: {e:?}"))?;
        d.ctrl.start(sid).map_err(|e| format!("{e:?}"))?;
        klog::event!(Info, "ktest", "hda.left_running", ctrl = d.ctrl.name(), stream = sid, input = true);
        LEFT.lock().push((d, sid));
    }
    Ok(())
}
