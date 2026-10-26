//! The mixer thread `snd/mix` (KernelRt, below the hda service threads: they read positions and call the
//! period callbacks that wake it) and the kernel's one mixer on `hda` and USB audio (`usbback`), made on first use.

use crate::backend::{Backend, Port, Target, WakeFn};
use crate::client::{self, Playback, SndError};
use crate::mixer::{Config, Core, Shared, Status};
use alloc::sync::Arc;
use core::sync::atomic::Ordering;
use devmodel::audio::PcmFormat;
use ksync::SpinLock;

/// Below hda's service threads (`sched::RT_PRIO_DEFAULT` 50).
pub const RT_PRIO: u8 = 45;
pub const DEFAULT_LATENCY_MS: u32 = 40;

pub struct Mixer {
    sh: Arc<Shared>,
    cfg: Config,
    thread: SpinLock<Option<sched::JoinHandle>>,
}

impl Mixer {
    /// Starts a mixer thread `name` on the back end `make` builds (it gets a function that wakes the thread).
    pub fn spawn<B: Backend + 'static>(name: &str, cfg: Config, make: impl FnOnce(WakeFn) -> B) -> Result<Arc<Mixer>, SndError> {
        let sh = Arc::new(Shared::new());
        let w = Arc::downgrade(&sh);
        let kick: WakeFn = Arc::new(move || {
            if let Some(s) = w.upgrade() {
                s.kick()
            }
        });
        let b = make(kick);
        let s2 = sh.clone();
        let opts = sched::SpawnOpts { rt_prio: RT_PRIO, ..Default::default() };
        let h = sched::spawn_with(name, sched::Class::KernelRt, opts, move || run(Core::new(b, s2.clone(), cfg), s2)).map_err(|e| {
            klog::event!(Error, "snd", "mix.thread_failed", name = name, error = alloc::format!("{e:?}"));
            SndError::NoMixer
        })?;
        klog::event!(Info, "snd", "mix.thread", name = name, class = "kernel_rt", prio = RT_PRIO, lead = cfg.lead, floor = cfg.floor, period = cfg.period,
            ring = cfg.ring, latency_us = cfg.lead as u64 * 1_000_000 / 48_000);
        Ok(Arc::new(Mixer { sh, cfg, thread: SpinLock::named("snd.mix.thread", Some(h)) }))
    }

    pub fn config(&self) -> Config {
        self.cfg
    }
    pub fn open(&self, fmt: PcmFormat, ring_frames: u32, name: &str) -> Result<Playback, SndError> {
        self.open_with(fmt, ring_frames, name, None)
    }
    /// `open` with the owner's poll queue (SN-4).
    pub fn open_with(&self, fmt: PcmFormat, ring_frames: u32, name: &str, poll: Option<Arc<vfs::poll::PollQueue>>) -> Result<Playback, SndError> {
        if self.sh.stop.load(Ordering::Acquire) {
            return Err(SndError::Closed);
        }
        client::open_with(&self.sh, fmt, ring_frames, name, poll)
    }
    pub fn start_together(&self, list: &[&Playback]) {
        crate::mixer::start_together(&self.sh, list)
    }
    pub fn set_target(&self, t: Target) {
        self.sh.set_target(t)
    }
    pub fn target(&self) -> Target {
        self.sh.target()
    }
    /// Per-channel Q15 master gain (`client::UNITY` = 0 dB).
    pub fn set_master(&self, g: [u32; 2]) {
        self.sh.set_master(g)
    }
    pub fn master(&self) -> [u32; 2] {
        self.sh.master()
    }
    pub fn status(&self) -> Status {
        self.sh.status()
    }
    pub fn clients(&self) -> alloc::vec::Vec<crate::mixer::ClientInfo> {
        self.sh.client_infos()
    }
    /// The thread was told to end (`stop`, or Quiesce before a Reload).
    pub fn stopped(&self) -> bool {
        self.sh.stop.load(Ordering::Acquire)
    }

    /// Closes the outputs as soon as no client runs (after their real frames are out) and waits up to
    /// `timeout_ns` for it; true when no output is open.
    pub fn release(&self, timeout_ns: u64) -> bool {
        self.sh.release.store(true, Ordering::Release);
        self.sh.kick();
        let end = time::now_ns().saturating_add(timeout_ns);
        while self.sh.status().streams > 0 {
            if time::now_ns() >= end {
                return false;
            }
            sched::sleep_ms(10);
        }
        true
    }

    /// Ends the thread (closing every output); for test instances.
    pub fn stop(&self) {
        self.sh.stop.store(true, Ordering::Release);
        self.sh.kick();
        let h = self.thread.lock().take();
        if let Some(h) = h
            && !h.join_timeout(2_000_000_000)
        {
            klog::event!(Warn, "snd", "mix.stop_timeout");
        }
    }

    /// The mixer thread's id (ktests check its class).
    pub fn thread_id(&self) -> Option<u64> {
        self.thread.lock().as_ref().map(|h| h.id())
    }
}

fn run<B: Backend>(mut core: Core<B>, sh: Arc<Shared>) {
    loop {
        if sh.stop.load(Ordering::Acquire) {
            core.shutdown();
            return;
        }
        let t = core.step(time::now_ns());
        sh.wake.wait_until_timeout(|| sh.take_kick() || sh.stop.load(Ordering::Acquire), t);
    }
}

static GLOBAL: SpinLock<Option<Arc<Mixer>>> = SpinLock::named("snd.mixer", None);

/// CONFIG `snd_latency_ms` (5..=500, default 40): frames kept queued at the output.
pub fn config() -> Config {
    let ms = pci::registry::config_value("snd_latency_ms").and_then(|v| v.trim().parse::<u32>().ok()).unwrap_or(DEFAULT_LATENCY_MS);
    Config::latency_ms(ms)
}

/// The kernel's mixer if something started it (never starts it: for status readers).
pub fn started() -> Option<Arc<Mixer>> {
    GLOBAL.lock().clone()
}

/// The kernel's mixer on `hda` (thread `snd/mix`), started on first use; it opens a controller only while a
/// client runs.
pub fn mixer() -> Result<Arc<Mixer>, SndError> {
    if let Some(m) = GLOBAL.lock().clone() {
        return Ok(m);
    }
    let m = Mixer::spawn("snd/mix", config(), crate::usbback::sys_backend)?;
    let mut g = GLOBAL.lock();
    if let Some(old) = g.clone() {
        drop(g);
        m.stop();
        return Ok(old);
    }
    *g = Some(m.clone());
    drop(g);
    let stored = crate::boot::stored();
    if stored == Some(Target::Prefer(Port::Hdmi)) {
        klog::event!(Debug, "snd", "snd.output_config_hdmi"; "stored hdmi: the boot switch turns HDMI on (snd/boot-out), auto until then");
    } else if let Some(t) = stored {
        klog::event!(Info, "snd", "snd.output", target = alloc::format!("{t:?}"), source = "config");
        m.set_target(t);
    }
    // Registered after hda's controllers (they register at probe, the mixer on first use), so Quiesce, which
    // goes newest first, stops the mixer's hda calls before hda stops the DMA.
    devmodel::register_quiesce(Arc::new(MixQuiesce(Arc::downgrade(&m.sh))));
    Ok(m)
}

/// The target the mixer starts with for a stored one: a stored `hdmi` starts auto (SN-11: only the boot switch
/// turns it on, with the display side, so the target never says hdmi while HDMI sound is off).
pub(crate) fn start_target(stored: Option<Target>) -> Target {
    match stored {
        Some(Target::Prefer(Port::Hdmi)) | None => Target::Auto,
        Some(t) => t,
    }
}

/// `auto`, `jack`, `hdmi` or `usb` (CONFIG `snd_output`, [`set_output`]).
pub fn parse_output(v: &str) -> Option<Target> {
    match v {
        "auto" => Some(Target::Auto),
        "jack" => Some(Target::Prefer(Port::Jack)),
        "hdmi" => Some(Target::Prefer(Port::Hdmi)),
        "usb" => Some(Target::Prefer(Port::Usb)),
        _ => None,
    }
}

/// Where the kernel's mixer plays: `auto` (follows plug state), `jack`, `hdmi` or `usb`. `hdmi` first turns HDMI sound
/// on (`hda::hdmi::hdmi_on`: the display side's 48 writes); refused, the target stays as it was. Thread context; may sleep. For a monitor command or MCP tool (change request in SN-5's notes).
pub fn set_output(v: &str) -> Result<(), alloc::string::String> {
    let t = parse_output(v).ok_or_else(|| alloc::format!("snd output: {v:?} is not auto, jack, hdmi or usb"))?;
    let _one = crate::output::APPLY.lock();
    crate::output::touch();
    set_target_from(t, "set_output")
}

/// [`set_output`] for a parsed target; `source` names the caller in the log (SN-10: `sysfs`, `mcp`). The caller
/// holds `output::APPLY`. The mixer first: if it cannot start, HDMI sound is not turned on for nothing.
pub(crate) fn set_target_from(t: Target, source: &'static str) -> Result<(), alloc::string::String> {
    let m = mixer().map_err(|e| alloc::format!("{e:?}"))?;
    if t == Target::Prefer(Port::Hdmi) {
        hda::hdmi::hdmi_on()?;
    }
    klog::event!(Info, "snd", "snd.output", target = alloc::format!("{t:?}"), source = source);
    m.set_target(t);
    Ok(())
}

/// Quiesce for the kernel's mixer: only raises the stop flag. No wake and no join, because Quiesce runs with
/// interrupts off, maybe on the panic path, where a run queue lock may be held; the thread sees the flag at its
/// next timed wake (at most one period) and closes its outputs, which hda then refuses as NotUp.
struct MixQuiesce(alloc::sync::Weak<Shared>);

impl devmodel::Quiesce for MixQuiesce {
    fn name(&self) -> &str {
        "snd/mix"
    }
    fn quiesce(&self) {
        if let Some(s) = self.0.upgrade() {
            s.stop.store(true, Ordering::Release);
        }
    }
}
