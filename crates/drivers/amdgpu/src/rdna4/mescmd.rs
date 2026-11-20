//! Wave 44, item ME: MES's commands as its stages send them, and the stop a queue error becomes: a command whose
//! failure stops the stage (Linux's `goto failure` in `mes_v12_0_hw_init`) and one whose answer Linux does not
//! check (`set_hw_resources_1`, mes_v12_0.c:2008-2011, :2076-2077).

use super::mesring::{Place, Port, Sent, said};
use super::nb::Io;
use super::sdmaq::QStop;
use super::stage::{Stop, stop};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::wb::WbPool;

pub(super) fn q_stop(e: QStop) -> Stop {
    let plain = match &e {
        QStop::Mem { .. } => "der Arbeitsspeicher für den Planer der Karte lässt sich nicht beschreiben",
        QStop::Doorbell { .. } => "der Planer der Karte lässt sich nicht anstoßen",
        QStop::Ring(_) | QStop::Slots(_) => "unerwarteter Fehler im Befehlskanal des Planers",
    };
    stop(plain, format!("{e:?}"))
}

/// A command whose failure stops the stage (Linux: `goto failure`), with its line.
pub(super) fn must(port: &mut Port, io: &mut Io, p: &Place, pool: &mut WbPool, mut frame: Vec<u32>, plain: &'static str, out: &mut Vec<String>) -> Result<(), Stop> {
    let sent = port.submit(io.card, p, pool, &mut frame).map_err(q_stop)?;
    let line = said(port.pipe, &frame, sent);
    out.push(line.clone());
    if sent.ok() {
        return Ok(());
    }
    klog::event!(Error, "gx", "rdna4.mes.command", what = line.as_str(); "MES did not answer a command of its start");
    Err(stop(plain, line))
}

/// A command whose answer Linux does not check: its line, and on.
pub(super) fn told(port: &mut Port, io: &mut Io, p: &Place, pool: &mut WbPool, mut frame: Vec<u32>, out: &mut Vec<String>) -> Result<Sent, Stop> {
    let sent = port.submit(io.card, p, pool, &mut frame).map_err(q_stop)?;
    let line = said(port.pipe, &frame, sent);
    if !sent.ok() {
        klog::event!(Warn, "gx", "rdna4.mes.unchecked", what = line.as_str(); "MES did not answer a command whose answer Linux does not check; going on as Linux does");
    }
    out.push(if sent.ok() { line } else { format!("{line}; Linux does not check this answer (mes_v12_0.c:2008-2011, :2076-2077) and goes on, so does this") });
    Ok(sent)
}
