//! The tone check of the hardware day (GP-SOUND item 5): one quiet second of 750 Hz on a named USB output, and one
//! plain German line of what the device answered at each step (found, rate, stream started, bytes sent, stopped).
//! It is the only thing in BitsOS that makes a sound without a program asking; nothing calls it at boot. The console
//! command `snd ton` and the MCP tool `snd_ton` (both in `snd`) are its two doors. Both end here, where it is refused
//! unless the test key `ktest_georg=on` in CONFIG says Georg is there to listen (no sound unasked, on every machine).
//! It runs only while nothing is open on that output (`dev::ToneClaim`: a stream the mixer
//! retires counts until it has played out) and keeps the mixer off it meanwhile, because two streams on one endpoint
//! would interleave. A muted master switch is named in the line, since the device then takes the tone in silence.
//!
//! On a UAC2 jack the check redoes the start order (alternate setting 0, rate, clock valid, alternate setting) that
//! the probe did, so the line shows what the device answers to each request now. On a UAC1 device (the JBL) it only
//! reads the rate: its probe path was accepted on the Mini-PC and is not repeated.

use crate::dev::{self, OutputInfo, ToneClaim};
use crate::req::{self, Ctl};
use crate::stream::Playback;
use crate::uac2dev::{self, Jack, Started};
use crate::words;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::Mutex;
use uac2::desc::terminal;
use usb_core::UsbError;

/// One second at 48 kHz.
pub const TONE_FRAMES: u64 = 48_000;
/// The check gives up waiting for the device after this long and says how far it got.
pub const DEADLINE_NS: u64 = 3_000_000_000;
const RING: usize = 4_800;
const PERIOD: u32 = 480;
/// 10 ms in and out, so the tone does not click.
const FADE: u64 = 480;
/// A quarter of a 64-sample sine (750 Hz at 48 kHz) at 1000 of 32767, about -30 dB below full scale.
const QUARTER: [i16; 17] = [0, 98, 195, 290, 383, 471, 556, 634, 707, 773, 831, 882, 924, 957, 981, 995, 1000];

static ONE: Mutex<()> = Mutex::named("usb_audio.tone_check", ());

pub fn tone_frame(i: u64) -> [i16; 2] {
    let k = (i % 64) as usize;
    let v = match k {
        0..=16 => QUARTER[k],
        17..=32 => QUARTER[32 - k],
        33..=48 => -QUARTER[k - 32],
        _ => -QUARTER[64 - k],
    } as i64;
    let edge = i.min(TONE_FRAMES.saturating_sub(1 + i)).min(FADE) as i64;
    let s = (v * edge / FADE as i64) as i16;
    [s, s]
}

/// What the line calls an output.
pub fn label(o: &OutputInfo) -> String {
    let what = match (o.board, o.terminal_type) {
        (true, ty) => words::jack_name(ty),
        (false, 0x0301 | 0x0302 | 0x0402) => "USB-Headset",
        (false, _) => "USB-Ausgang",
    };
    String::from(what)
}

/// The name `snd` gives this output (`Endpoint::name`), which the mixer's status shows.
pub fn endpoint_name(o: &OutputInfo) -> String {
    format!("{}/usb/{}:{:#x}", o.key, o.control, o.iface)
}

pub fn listing(outs: &[OutputInfo]) -> String {
    outs.iter().enumerate().map(|(i, o)| format!("{} = {} ({})", i + 1, label(o), endpoint_name(o))).collect::<Vec<_>>().join(", ")
}

/// The output a name means: nothing (the first, which is the default), its number in the list, a word (headset,
/// lautsprecher, kopfhoerer, spdif) or its full name.
pub fn pick(outs: &[OutputInfo], name: Option<&str>) -> Result<OutputInfo, String> {
    if outs.is_empty() {
        return Err(String::from("Tontest: kein USB-Ausgang da (nichts gebunden)."));
    }
    let n = name.map(str::trim).unwrap_or("");
    let board = |ty: u16| outs.iter().find(|o| o.board && o.terminal_type == ty);
    let hit = match n {
        "" => outs.first(),
        "headset" => outs.iter().find(|o| !o.board),
        "lautsprecher" => board(terminal::SPEAKER),
        "kopfhoerer" | "kopfhörer" => board(terminal::HEADPHONES),
        "spdif" => board(terminal::SPDIF),
        _ => match n.parse::<usize>() {
            Ok(i) if i >= 1 => outs.get(i - 1),
            _ => outs.iter().find(|o| endpoint_name(o) == n),
        },
    };
    hit.cloned().ok_or_else(|| format!("Tontest: kein Ausgang „{n}“. Da sind: {}.", listing(outs)))
}

fn rate_text(j: &Jack, st: &Started) -> String {
    let now = if st.wrote {
        format!("war {}, auf {} Hz gesetzt, Gerät meldet {}", words::hz(st.before), j.plan.rate, words::hz(st.after))
    } else {
        format!("stand schon auf {}", words::hz(st.before))
    };
    format!("Takt {} {now}, gültig, Schnittstelle {} Einstellung {}", j.plan.clock, j.plan.iface, j.plan.alt)
}

/// What the check needs to know of the output beyond its name.
pub enum Kind {
    Uac2(Jack),
    Uac1 { ep: u8, rate_control: bool },
}

pub struct Env<'a> {
    pub now_ns: &'a dyn Fn() -> u64,
    pub sleep_ms: &'a dyn Fn(u64),
    /// The master switch is off: the device's feature units are muted (`dev::hw_mute`).
    pub muted: bool,
}

/// Where the check may run: only while the test key `ktest_georg=on` says Georg is there to listen.
pub fn allowed(georg: bool) -> Result<(), String> {
    if georg {
        return Ok(());
    }
    Err(String::from("Tontest: gesperrt; er läuft nur, wenn in CONFIG.TXT ktest_georg=on steht (Georg hört zu)."))
}

/// `allowed` with this machine's CONFIG.
pub fn allowed_here() -> Result<(), String> {
    allowed(pci::registry::config_value("ktest_georg").is_some_and(|v| v.trim() == "on"))
}

/// The line's part for a muted master switch.
pub const MUTED: &str = "Hauptschalter stumm: das Gerät bekommt den Ton, hörbar ist nichts";

/// Gives the stream what fits of the tone, never more than one second in all.
fn feed(p: &Playback, written: &mut u64) -> Result<(), UsbError> {
    while *written < TONE_FRAMES {
        let n = (TONE_FRAMES - *written).min(PERIOD as u64);
        let chunk: Vec<[i16; 2]> = (*written..*written + n).map(tone_frame).collect();
        let took = p.write(&chunk)?;
        *written += took as u64;
        if took < chunk.len() {
            break;
        }
    }
    Ok(())
}

fn line(label: &str, parts: &[String], end: &str) -> String {
    format!("Tontest {label}: {}; {end}.", parts.join("; "))
}

/// The steps over a control pipe and a way to open the stream; host tests give fakes of both.
pub fn run(label: &str, kind: &Kind, frame_bytes: usize, c: &dyn Ctl, open: &dyn Fn() -> Result<Playback, UsbError>, env: &Env<'_>) -> String {
    let mut parts = alloc::vec![String::from("gefunden")];
    if env.muted {
        parts.push(String::from(MUTED));
    }
    match kind {
        Kind::Uac2(j) => match uac2dev::start(c, &j.clock, &j.plan) {
            Ok(st) => parts.push(rate_text(j, &st)),
            Err(e) => {
                parts.push(words::start(&e, j.plan.clock, j.plan.iface, j.plan.alt, j.plan.rate));
                return line(label, &parts, "abgebrochen, kein Ton");
            }
        },
        Kind::Uac1 { rate_control: false, .. } => parts.push(String::from("Rate fest (das Gerät hat keine Einstellung dafür)")),
        Kind::Uac1 { ep, rate_control: true } => {
            let mut b = [0u8; 3];
            parts.push(match c.input(req::ep_rate(req::GET_CUR, *ep), &mut b) {
                Ok(3) => format!("Rate laut Gerät {} Hz", b[0] as u32 | (b[1] as u32) << 8 | (b[2] as u32) << 16),
                _ => String::from("das Gerät nennt seine Rate nicht (bei Headsets üblich)"),
            });
        }
    }
    let p = match open() {
        Ok(p) => p,
        Err(e) => {
            parts.push(format!("Strom nicht geöffnet: {}", words::usb(e)));
            return line(label, &parts, "abgebrochen, kein Ton");
        }
    };
    let mut written = 0u64;
    if let Err(e) = feed(&p, &mut written).and_then(|_| p.start()) {
        p.close();
        parts.push(format!("Strom nicht gestartet: {}", words::usb(e)));
        return line(label, &parts, "abgebrochen, kein Ton");
    }
    parts.push(String::from("Strom gestartet"));
    let t0 = (env.now_ns)();
    let (mut gone, mut pos) = (None, 0);
    let played = loop {
        pos = match p.position() {
            Ok(x) => x,
            Err(e) => {
                gone = Some(e);
                break pos;
            }
        };
        if pos >= TONE_FRAMES || (env.now_ns)().saturating_sub(t0) >= DEADLINE_NS {
            break pos;
        }
        if let Err(e) = feed(&p, &mut written) {
            gone = Some(e);
            break pos;
        }
        (env.sleep_ms)(10);
    };
    let ms = (env.now_ns)().saturating_sub(t0) / 1_000_000;
    let st = p.stats();
    let bytes = played * frame_bytes as u64;
    parts.push(match (played, gone) {
        (_, Some(e)) => format!("abgebrochen nach {bytes} Bytes: {}", words::usb(e)),
        (0, None) => format!("das Gerät hat in {ms} ms nichts abgenommen (0 Bytes)"),
        (n, None) if n < TONE_FRAMES => format!("nur {bytes} Bytes in {ms} ms gesendet"),
        _ => format!("{bytes} Bytes in {ms} ms gesendet"),
    });
    parts.push(format!("{} Pakete verpasst, {} Fehler", st.missed, st.errors));
    p.close();
    let t1 = (env.now_ns)();
    while p.queued_packets() > 0 && (env.now_ns)().saturating_sub(t1) < 300_000_000 {
        (env.sleep_ms)(10);
    }
    let end = match p.queued_packets() {
        0 if gone.is_none() && played >= TONE_FRAMES => String::from("gestoppt, fertig"),
        0 => String::from("gestoppt"),
        n => format!("gestoppt, {n} Pakete noch unterwegs"),
    };
    line(label, &parts, &end)
}

/// The kernel's check on output `name`; `busy(name)` says whether the mixer plays there now.
pub fn tone(name: Option<&str>, busy: &dyn Fn(&str) -> bool) -> String {
    if let Err(t) = allowed_here() {
        return t;
    }
    let Some(_one) = ONE.try_lock() else {
        return String::from("Tontest: läuft schon.");
    };
    let o = match pick(&dev::outputs(), name) {
        Ok(o) => o,
        Err(t) => return t,
    };
    let (lab, ep_name) = (label(&o), endpoint_name(&o));
    let busy_line = || format!("Tontest {lab}: dort spielt gerade etwas; noch einmal, wenn es 2 s still war.");
    if busy(&ep_name) {
        return busy_line();
    }
    let Some((f, s)) = dev::find(&o.key, o.iface, true) else {
        return format!("Tontest {lab}: gerade verschwunden.");
    };
    let Ok(claim) = ToneClaim::take(&f, o.iface) else {
        return busy_line();
    };
    let kind = match uac2dev::jack(f.dev.id, o.iface) {
        Some(j) => Kind::Uac2(j),
        None => Kind::Uac1 { ep: s.ep.address, rate_control: s.rate_control },
    };
    let open = || claim.open(RING, PERIOD, Arc::new(|| {}));
    let env = Env { now_ns: &time::now_ns, sleep_ms: &sched::sleep_ms, muted: dev::hw_mute() };
    let out = run(&lab, &kind, s.frame_bytes(), &f.dev, &open, &env);
    klog::event!(Info, "usb_audio", "uac.tone_check", output = ep_name.as_str(), line = out.as_str());
    out
}
