//! SN-11: HDMI sound comes back by itself after a boot or a Reload. With `hdmi` stored (CONFIG `snd_output=hdmi`;
//! settingsd's `kernel.snd_output` writes that key), the thread `snd/boot-out` (Normal) waits, bounded, for the PCI
//! scan and for hda's bind of the GPU's HD Audio function (the display engine attaches at amdgpu's probe before
//! it), senses the monitor by reads only (`hda::hdmi::monitor`: the DP link's gates), then switches through the
//! same path as `/sys/bitsos/snd_output` and the tool (`output::apply_locked`: make before break, under
//! `output::APPLY`). No GPU HD Audio function, no display engine, no monitor lit or a refusal: the target stays
//! auto, logged. A request that came first (sysfs, MCP, the console) decides instead. The mixer's own start leaves
//! a stored `hdmi` to this thread (`thread::mixer`), so the target never says hdmi while the display side is off.

use crate::backend::{Port, Target};
use crate::output::{self, Request};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};
use hdacodec::dpaudio::Sense;

/// What the boot switch does with what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// No `hdmi` stored.
    Nothing,
    /// A request (sysfs, MCP, the console) came first: it decided.
    Decided,
    /// Switch HDMI sound on.
    Switch,
    /// Stay auto, and why.
    Auto(String),
}

impl Boot {
    /// For the log: `nothing`, `decided`, `hdmi` (switched on) or `auto`.
    pub fn word(&self) -> &'static str {
        match self {
            Boot::Nothing => "nothing",
            Boot::Decided => "decided",
            Boot::Switch => "hdmi",
            Boot::Auto(_) => "auto",
        }
    }
}

/// The decision, kernel-free (host test `tests_out::boot_*`). `gpu_audio`: hda bound the GPU's HD Audio function.
pub fn decision(stored: Option<&Target>, touched: bool, gpu_audio: bool, sense: Sense) -> Boot {
    if stored != Some(&Target::Prefer(Port::Hdmi)) {
        return Boot::Nothing;
    }
    if touched {
        return Boot::Decided;
    }
    if !gpu_audio {
        return Boot::Auto("no GPU HD Audio function".into());
    }
    match sense {
        Sense::Absent => Boot::Auto("no display engine".into()),
        Sense::Dark { gate, read } => Boot::Auto(format!("no monitor lit: {} (read {read:X})", gate.name)),
        Sense::Lit => Boot::Switch,
    }
}

/// The stored target (CONFIG `snd_output`), as the mixer's start reads it.
pub fn stored() -> Option<Target> {
    pci::registry::config_value("snd_output").and_then(|v| crate::parse_output(v.trim()))
}

const POLL_MS: u64 = 20;
/// From the Service initcalls to the PCI scan the kernel copies the initramfs into tmpfs (hundreds of MB with
/// Brave): generous, the thread only sleeps.
const SCAN_WAIT_MS: u64 = 120_000;
/// From the scan to hda's bind of the GPU function (drivers bind in function order; WLAN loads firmware).
const BIND_WAIT_MS: u64 = 30_000;

/// Polls `done` every [`POLL_MS`] until it holds (Some(waited ms)) or `bound_ms` passed (None). Clock and sleep
/// come in so the host test `tests_out::boot_wait` drives it.
pub fn wait_until(bound_ms: u64, mut done: impl FnMut() -> bool, mut now_ns: impl FnMut() -> u64, mut sleep_ms: impl FnMut(u64)) -> Option<u64> {
    let start = now_ns();
    loop {
        let waited = now_ns().saturating_sub(start) / 1_000_000;
        if done() {
            return Some(waited);
        }
        if waited >= bound_ms {
            return None;
        }
        sleep_ms(POLL_MS);
    }
}

fn wait_for(bound_ms: u64, done: impl FnMut() -> bool) -> Option<u64> {
    wait_until(bound_ms, done, time::now_ns, sched::sleep_ms)
}

/// `snd/boot-out`'s state: not started, running (its waits and the switch), done (whatever it decided).
const IDLE: u8 = 0;
const RUNNING: u8 = 1;
const DONE: u8 = 2;
static STATE: AtomicU8 = AtomicU8::new(IDLE);

/// Waits (sleeping, at most `bound_ms`) while `snd/boot-out` runs; the `sn.out.*` ktests call it first so a run
/// over MCP early after a boot neither counts as "a request came first" nor puts the target back over the switch.
/// Some(waited ms), or None when it still runs.
pub(crate) fn settle(bound_ms: u64) -> Option<u64> {
    wait_for(bound_ms, || STATE.load(Ordering::Acquire) != RUNNING)
}

/// The longest `snd/boot-out` may take after the scan (its bind wait plus the switch).
pub(crate) const SETTLE_MS: u64 = BIND_WAIT_MS + 10_000;

/// The GPU's HD Audio function (1002, class 04) bound by hda, with its controller registered.
fn gpu_audio_bound() -> bool {
    let Some(f) = pci::devices().into_iter().find(|f| f.vendor == 0x1002 && f.class_code() == 0x04) else {
        return false;
    };
    pci::binding(f.loc()).is_some_and(|b| b.state == pci::BindState::Bound) && hda::hdmi::gpu_controller().is_some()
}

fn has_gpu_audio_function() -> bool {
    pci::devices().iter().any(|f| f.vendor == 0x1002 && f.class_code() == 0x04)
}

/// Starts `snd/boot-out` (from the `snd-output` initcall: nothing waits for it).
pub(crate) fn spawn() {
    STATE.store(RUNNING, Ordering::Release);
    if let Err(e) = sched::spawn("snd/boot-out", sched::Class::Normal, run) {
        STATE.store(DONE, Ordering::Release);
        klog::event!(Warn, "snd", "snd.output_boot", outcome = "not_started", error = format!("{e:?}"); "a stored hdmi is not switched on");
    }
}

fn run() {
    run_steps();
    STATE.store(DONE, Ordering::Release);
}

fn run_steps() {
    // CONFIG is installed before the scan; the scan fills the function list at once.
    if wait_for(SCAN_WAIT_MS, || !pci::devices().is_empty()).is_none() {
        klog::event!(Warn, "snd", "snd.output_boot", outcome = "timeout", waited_for = "pci scan", bound_ms = SCAN_WAIT_MS);
        return;
    }
    let stored = stored();
    if stored != Some(Target::Prefer(Port::Hdmi)) {
        klog::event!(Debug, "snd", "snd.output_boot", outcome = "nothing", stored = format!("{stored:?}"));
        return;
    }
    let mut waited = 0;
    if has_gpu_audio_function() {
        match wait_for(BIND_WAIT_MS, gpu_audio_bound) {
            Some(ms) => waited = ms,
            None => klog::event!(Warn, "snd", "snd.output_boot_bind_timeout", bound_ms = BIND_WAIT_MS; "hda did not bind the GPU's HD Audio function"),
        }
    }
    let r = switch(stored.as_ref(), "boot", false);
    klog::event!(Info, "snd", "snd.output_boot_done", waited_ms = waited, outcome = r.word());
}

/// The boot switch's step after the waits (ktest `sn.out.boot` calls it with a stored value of its own and
/// `as_at_boot`: requests before it do not count). The sink from the loader's EDID first, if the kernel has not
/// handed it over yet (it does right after the binds).
pub(crate) fn switch(stored: Option<&Target>, source: &'static str, as_at_boot: bool) -> Boot {
    let gpu = gpu_audio_bound();
    if gpu
        && !hda::hdmi::sink_known()
        && let Some(e) = kexec::boot::edid()
    {
        let _ = hda::hdmi::set_sink_edid(e);
    }
    let _one = output::APPLY.lock();
    let touched = output::touched() && !as_at_boot;
    // Sensing attaches the display engine: only after the scan, and only when there is HDMI sound to have.
    let sense = if gpu && stored == Some(&Target::Prefer(Port::Hdmi)) && !touched { hda::hdmi::monitor() } else { Sense::Absent };
    act(decision(stored, touched, gpu, sense), source, output::apply_locked)
}

/// Logs the decision and, for [`Boot::Switch`], switches through `apply` (`output::apply_locked` with
/// `output::APPLY` held by the caller; a fake in the host test `tests_out::boot_act`). A refusal turns into auto.
pub fn act(d: Boot, source: &'static str, apply: impl FnOnce(&Request, &'static str) -> Result<String, output::Refusal>) -> Boot {
    match &d {
        Boot::Nothing => {}
        Boot::Decided => klog::event!(Info, "snd", "snd.output_boot", outcome = "decided", source = source; "a request came first: the stored hdmi is left"),
        Boot::Auto(why) => klog::event!(Info, "snd", "snd.output_boot", outcome = "auto", source = source, why = why.as_str(); "stored hdmi: plays auto"),
        Boot::Switch => match apply(&Request::Target(Target::Prefer(Port::Hdmi)), source) {
            Ok(line) => klog::event!(Info, "snd", "snd.output_boot", outcome = "hdmi", source = source, text = line.as_str()),
            Err(e) => {
                klog::event!(Warn, "snd", "snd.output_boot", outcome = "auto", source = source, why = e.text.as_str(); "HDMI sound refused: plays auto");
                return Boot::Auto(e.text);
            }
        },
    }
    d
}
