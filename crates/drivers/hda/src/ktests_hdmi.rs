//! SN-5 ktests: HDMI sound. `sn.dpaudio.absent` (Safe) proves that nothing is written without a display engine
//! (QEMU) and only reads on hardware. `on` and `off` write display registers (class `Display`, R8c: no switch)
//! and `tone` plays sound (`Audible`); they skip without the display engine. Each leaves HDMI sound as it found
//! it, so a full run over MCP changes nothing on the machine.

use crate::ctrl::DP_AUDIO_PIN;
use crate::hdmi::{self, display_state, gpu_controller, index_selects, write_calls};
use crate::{OutputId, signal};
use alloc::format;
use amdgpu::dcn::{AZ_ENDPOINT_INDEX, DcnRegs};
use devmodel::audio::PcmFormat;
use hdacodec::dpaudio::{self, State};
use ktest::{TestResult, kassert, ktest};

fn state_name(s: State) -> &'static str {
    match s {
        State::Off => "off",
        State::On => "on",
        State::Absent => "absent",
    }
}

ktest!("sn.dpaudio.absent", Safe, absent);
/// Without a display engine every call answers "no display engine" and not one write reaches `DcnRegs`; with
/// one (the Mini-PC after GX's attach) the state is only read. Rerunnable.
fn absent() -> TestResult {
    let before = write_calls();
    let sel_before = index_selects();
    let st = display_state();
    let gpu = gpu_controller();
    klog::event!(Info, "ktest", "dpaudio.state", state = state_name(st), attached = DcnRegs::attached().is_some(),
        gpu = gpu.as_ref().map_or("none", |d| d.ctrl.name()),
        streaming = gpu.as_ref().is_some_and(|d| !d.ctrl.read_only()));
    if DcnRegs::attached().is_none() {
        kassert!(st == State::Absent, "state {:?} without a display engine", st);
        let on = hdmi::display_on();
        kassert!(on.as_ref().is_err_and(|e| e.contains("no display engine")), "display_on: {on:?}");
        let off = hdmi::display_off();
        kassert!(off.as_ref().is_err_and(|e| e.contains("no display engine")), "display_off: {off:?}");
        kassert!(write_calls() == before, "{} writes reached DcnRegs", write_calls() - before);
        // SN-13 moved paired EP_INDEX writes to their own counter; without an engine there is none of either.
        kassert!(index_selects() == sel_before, "{} index selects reached DcnRegs", index_selects() - sel_before);
        if let Some(d) = &gpu {
            kassert!(d.ctrl.read_only(), "{} streams without a display side", d.ctrl.name());
        }
        return Ok(());
    }
    kassert!(st != State::Absent, "the segment bases are not confirmed");
    // Reading the state selects the endpoint; the index register must be as found afterwards.
    let r = DcnRegs::attached().ok_or("display engine gone")?;
    let index = || r.with_az_endpoint_pair(|r| r.read(AZ_ENDPOINT_INDEX));
    let was = index();
    let _ = display_state();
    kassert!(index() == was, "endpoint index {:?}, found {was:?}", index());
    Ok(())
}

fn ready() -> Result<(), TestResult> {
    if DcnRegs::attached().is_none() {
        return Err(ktest::skip("no display engine (QEMU, or no GPU attach yet)"));
    }
    if gpu_controller().is_none() {
        return Err(ktest::skip("no GPU HD Audio function"));
    }
    Ok(())
}

/// HDMI sound off and checked: the GPU controller back to enumerate-only, the 41-write undo, then every register
/// the undo wrote reads back as the firmware had it (saved-5.json) in the bits the undo checks.
fn off_checked() -> TestResult {
    let o = hdmi::hdmi_off()?;
    kassert!(o.mismatched == 0, "{} writes did not read back", o.mismatched);
    kassert!(display_state() == State::Off, "state {:?} after off", display_state());
    let now = hdmi::undo_registers()?;
    // A register the undo writes twice (a step value, then the saved one) is checked against its last write only.
    let undo = &dpaudio::UNDO;
    for (i, (w, (loc, v))) in undo.iter().zip(now).enumerate() {
        if undo[i + 1..].iter().any(|later| later.loc == w.loc) {
            continue;
        }
        kassert!((v ^ w.or) & w.check == 0, "{loc:?} reads {v:#x}, the firmware had {:#x}", w.or);
    }
    kassert!(gpu_controller().is_none_or(|d| d.ctrl.read_only()), "the GPU controller still streams");
    Ok(())
}

ktest!("sn.dpaudio.on", Display, on);
/// HDMI sound on: the display side's 48 writes (or "on already"), the GPU controller streaming, pin 3 playable;
/// then off again if it was off.
fn on() -> TestResult {
    if let Err(r) = ready() {
        return r;
    }
    let was_on = display_state() == State::On;
    hdmi::hdmi_on()?;
    let r = playable();
    if !was_on {
        off_checked()?;
    }
    r
}

fn playable() -> TestResult {
    kassert!(display_state() == State::On, "state {:?} after on", display_state());
    let d = gpu_controller().ok_or("GPU controller gone")?;
    let out = d.ctrl.outputs().into_iter().find(|o| o.id.pin == DP_AUDIO_PIN).ok_or("no pin 3")?;
    let sense = d.ctrl.verb(out.id.cad, DP_AUDIO_PIN, hdacodec::verb::GET_PIN_SENSE).unwrap_or(!0);
    klog::event!(Info, "ktest", "dpaudio.on", ctrl = d.ctrl.name(), pin = klog::Hex(DP_AUDIO_PIN as u64), playable = out.playable,
        sense = klog::Hex(sense as u64), plugged = format!("{:?}", out.plugged).as_str());
    kassert!(out.playable, "pin 3 not playable");
    Ok(())
}

ktest!("sn.dpaudio.tone", Audible, tone);
/// Three seconds of a quiet 1 kHz tone on pin 3 (the monitor's speakers); HDMI sound on for it and off again
/// if it was off.
fn tone() -> TestResult {
    if let Err(r) = ready() {
        return r;
    }
    let was_on = display_state() == State::On;
    hdmi::hdmi_on()?;
    let r = play();
    if !was_on {
        hdmi::hdmi_off()?;
    }
    r
}

fn play() -> TestResult {
    let d = gpu_controller().ok_or("GPU controller gone")?;
    let id = OutputId { cad: d.ctrl.outputs().into_iter().find(|o| o.id.pin == DP_AUDIO_PIN).ok_or("no pin 3")?.id.cad, pin: DP_AUDIO_PIN };
    let fmt = PcmFormat { rate: 48000, channels: 2, bits: 16 };
    let sid = d.ctrl.open(id, fmt, 16384, 4096).map_err(|e| format!("open: {e:?}"))?;
    let total = 3 * 48000u64;
    let mut sent = 0u64;
    let fill = |sent: &mut u64| -> Result<(), crate::HdaError> {
        loop {
            let n = 1024u64;
            let chunk = if *sent < total { signal::bytes(*sent, n.min(total - *sent) as usize, true) } else { alloc::vec![0u8; n as usize * 4] };
            let took = d.ctrl.write(sid, &chunk)? as u64 / 4;
            *sent += took;
            if took < chunk.len() as u64 / 4 {
                return Ok(());
            }
        }
    };
    let r = (|| -> Result<(u64, u64), alloc::string::String> {
        fill(&mut sent).map_err(|e| format!("prefill: {e:?}"))?;
        d.ctrl.start(sid).map_err(|e| format!("start: {e:?}"))?;
        let t0 = time::now_ns();
        let (ring, _) = d.ctrl.stream_layout(sid).unwrap_or((0, 0));
        loop {
            fill(&mut sent).map_err(|e| format!("write: {e:?}"))?;
            let pos = d.ctrl.position(sid).unwrap_or(0);
            if pos >= total + ring as u64 {
                return Ok((pos, time::now_ns() - t0));
            }
            if time::now_ns() - t0 > 6_000_000_000 {
                return Err(format!("position {pos} after 6 s"));
            }
            sched::sleep_ns(5_000_000);
        }
    })();
    let _ = d.ctrl.stop(sid);
    let _ = d.ctrl.close(sid);
    let (pos, ns) = r?;
    // The link's real rate (usb.md: 47895-47902 samples/s through the converter); judged only on the Mini-PC.
    let rate = pos * 1_000_000_000 / ns.max(1);
    klog::event!(Info, "ktest", "dpaudio.tone", ctrl = d.ctrl.name(), frames = total, position = pos, ms = ns / 1_000_000, rate = rate;
        "3 s 1 kHz on the monitor's speakers");
    Ok(())
}

ktest!("sn.dpaudio.off", Display, off);
/// The undo, checked (`off_checked`); HDMI sound on first if it was off, and on again afterwards if it was on.
fn off() -> TestResult {
    if let Err(r) = ready() {
        return r;
    }
    let was_on = display_state() == State::On;
    if !was_on {
        hdmi::hdmi_on()?;
    }
    off_checked()?;
    if was_on {
        hdmi::hdmi_on()?;
    }
    Ok(())
}
