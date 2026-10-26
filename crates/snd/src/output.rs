//! SN-10: where the kernel's mixer plays, reachable the Linux way and over MCP: `/sys/bitsos/snd_output` (CL-1's
//! attribute files, `vfs_sys::bitsos`; one value, as sysfs wants), the read-only `/sys/bitsos/snd_output_status`
//! (what plays where) and the MCP tool `snd` (`tool.rs`). Values: `auto`, `jack`, `hdmi`, `usb` (the targets of
//! [`crate::set_output`]) and `hdmi-off` (HDMI sound off, the monitor's `snd hdmi off`).
//! Refusals: an unknown value EINVAL; `hdmi`/`hdmi-off` without a GPU HD Audio function or display engine (QEMU)
//! ENODEV; a refusal of `hdmi_on`/`hdmi_off` on the hardware (a gate, a read-back mismatch, the controller not
//! coming up) EIO. A refused value leaves the target as it was. Status readers never start the mixer. One request
//! at a time ([`APPLY`]), so the target and the display side change together. The file alone (SN-12 fixer, its
//! request runs in `helper`): EBUSY when another request held [`APPLY`] past the bound, EINTR when a signal came
//! before the request began (nothing done in both), EIO when a begun request missed the bound (it runs on).

use crate::backend::{Port, Target};
use crate::mixer::Status;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use hdacodec::dpaudio::State as DpState;
use ksync::Mutex;
use uapi::errno::{EACCES, EBUSY, EINTR, EINVAL, EIO, ENODEV, ENOMEM};
use vfs::Errno;

/// Serializes [`apply`] and [`crate::set_output`] (the console's `snd output`): without it a `hdmi` and a
/// `hdmi-off` at once could end with target hdmi and the display side off. The outermost lock of the sound
/// side (SN-12: class level `ksync::order::SND_OUTPUT`, above `hda.hdmi`): every path takes `hda.hdmi`, the GPU
/// controller's PCI locks and the mixer's under it and sleeps under it (bounded); taken with nothing held.
pub(crate) static APPLY: Mutex<()> = Mutex::named("snd.output", ());

/// SN-11: a request came (sysfs, MCP, the console; under [`APPLY`]): the boot switch then leaves the target to it.
static TOUCHED: AtomicBool = AtomicBool::new(false);

pub(crate) fn touch() {
    TOUCHED.store(true, Ordering::Release);
}

pub(crate) fn touched() -> bool {
    TOUCHED.load(Ordering::Acquire)
}

/// Puts the flag back (the `sn.out.*` ktests: their own requests are not a viewer's, and they rerun in one boot).
pub(crate) fn set_touched(v: bool) {
    TOUCHED.store(v, Ordering::Release);
}

/// What a write asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Target(Target),
    HdmiOff,
}

/// The values a write takes (for usage texts and the tool's schema).
pub const VALUES: &str = "auto, jack, hdmi, usb or hdmi-off";

/// Surrounding ASCII white space is ignored (an MCP argument, `echo` with a CR); `hdmi off` (the monitor's
/// spelling) is the same as `hdmi-off`. Otherwise exact, as `parse_output` and CONFIG.
pub fn parse_request(v: &str) -> Option<Request> {
    let v = v.trim_matches(|c: char| c.is_ascii_whitespace());
    match v {
        "hdmi-off" | "hdmi off" => Some(Request::HdmiOff),
        _ => crate::parse_output(v).map(Request::Target),
    }
}

/// Why a request was not done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Not one of [`VALUES`].
    BadValue,
    /// No HDMI sound on this machine (no GPU HD Audio function or no display engine).
    NoHdmi,
    /// The hardware refused (hdmi_on/hdmi_off's text says why).
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub why: Why,
    pub text: String,
}

impl Refusal {
    pub fn errno(&self) -> Errno {
        match self.why {
            Why::BadValue => EINVAL,
            Why::NoHdmi => ENODEV,
            Why::Failed => EIO,
        }
    }
}

/// The word a target is written as (`fixed:<dev>/<port>` only comes from tests and is not writable).
pub fn target_name(t: &Target) -> String {
    match t {
        Target::Auto => "auto".into(),
        Target::Prefer(p) => port_name(*p).into(),
        Target::Fixed { dev, port } => format!("fixed:{dev}/{}", port_name(*port)),
    }
}

pub fn port_name(p: Port) -> &'static str {
    match p {
        Port::Jack => "jack",
        Port::Hdmi => "hdmi",
        Port::Usb => "usb",
    }
}

/// The display side's audio as a word; None (busy: `hdmi_on`/`hdmi_off` runs) gives `busy`.
pub fn display_name(s: Option<DpState>) -> &'static str {
    match s {
        Some(DpState::On) => "on",
        Some(DpState::Off) => "off",
        Some(DpState::Absent) => "absent",
        None => "busy",
    }
}

/// `/sys/bitsos/snd_output_status`'s line (without the newline the attribute layer adds).
pub fn status_text(mixer: &str, playing: Option<&str>, clients: usize, hdmi: &str) -> String {
    format!("mixer={mixer} playing={} clients={clients} hdmi={hdmi}", playing.unwrap_or("none"))
}

/// The mixer's target now: the running mixer's, else what its start will take from CONFIG `snd_output` (a stored
/// `hdmi` is the boot switch's: the mixer starts auto, SN-11).
pub fn current_target() -> Target {
    match crate::started() {
        Some(m) => m.target(),
        None => crate::thread::start_target(crate::boot::stored()),
    }
}

/// `running`, `stopped` (Quiesce or a test stopped it) or `idle` (not started yet).
pub fn mixer_state() -> &'static str {
    mixer_word(crate::started().map(|m| m.stopped()))
}

/// The word for a mixer: None not started, Some(stopped).
pub fn mixer_word(stopped: Option<bool>) -> &'static str {
    match stopped {
        Some(true) => "stopped",
        Some(false) => "running",
        None => "idle",
    }
}

/// No HDMI sound possible: no GPU HD Audio function, or its display side is absent (QEMU, gates 1/2).
fn no_hdmi() -> Option<String> {
    if hda::hdmi::gpu_controller().is_none() {
        return Some("HDMI audio: no GPU HD Audio function".into());
    }
    (hda::hdmi::display_state_now() == Some(DpState::Absent)).then(|| String::from("HDMI audio: no display engine"))
}

/// Does `r` (thread context; `hdmi` and `hdmi-off` sleep while the GPU controller restarts). `source` names the
/// caller in the log (`sysfs`, `mcp`). Ok: a line saying what was done.
pub fn apply(r: &Request, source: &'static str) -> Result<String, Refusal> {
    let _one = APPLY.lock();
    touch();
    apply_locked(r, source)
}

/// [`apply`] with [`APPLY`] held by the caller (the boot switch decides and switches under one hold).
pub(crate) fn apply_locked(r: &Request, source: &'static str) -> Result<String, Refusal> {
    let refuse = |why: Why, text: String| {
        let e = Refusal { why, text };
        klog::event!(Info, "snd", "snd.output_refused", source = source, request = format!("{r:?}"), errno = e.errno(), text = e.text.as_str());
        Err(e)
    };
    match r {
        Request::Target(t) => {
            if *t == Target::Prefer(Port::Hdmi)
                && let Some(why) = no_hdmi()
            {
                return refuse(Why::NoHdmi, why);
            }
            match crate::thread::set_target_from(t.clone(), source) {
                Ok(()) => Ok(format!("snd output {}", target_name(t))),
                Err(e) => refuse(Why::Failed, e),
            }
        }
        Request::HdmiOff => {
            if let Some(why) = no_hdmi() {
                return refuse(Why::NoHdmi, why);
            }
            // A stopped mixer (Quiesce) plays nowhere: nothing to move.
            let m = crate::started().filter(|m| !m.stopped());
            let prev = m.as_ref().map(|m| m.target());
            let moved = prev == Some(Target::Prefer(Port::Hdmi));
            if let Some(m) = &m {
                if moved {
                    klog::event!(Info, "snd", "snd.output", target = "Auto", source = source, why = "hdmi_off");
                    m.set_target(Target::Auto);
                }
                if let Some(gpu) = hda::hdmi::gpu_controller() {
                    leave_controller(m, gpu.ctrl.name(), moved, source);
                }
            }
            match hda::hdmi::hdmi_off() {
                Ok(o) => Ok(o.line()),
                Err(e) => {
                    if let (true, Some(m), Some(p)) = (moved, &m, prev) {
                        klog::event!(Info, "snd", "snd.output", target = format!("{p:?}"), source = source, why = "hdmi_off_refused");
                        m.set_target(p);
                    }
                    refuse(Why::Failed, e)
                }
            }
        }
    }
}

/// Does the mixer still play on controller `dev` (its active output, or one retiring after a switch)? A retiring
/// output is not named in the status, so any retiring one counts; an active null output (open failed) looks like
/// one too, which only costs the wait's bound.
pub fn on_controller(s: &Status, dev: &str) -> bool {
    plays_on(s, dev) || s.streams > s.endpoint.is_some() as usize
}

/// Is the mixer's active output on controller `dev` (endpoint `<dev>/<port>/<cad>:<pin>`)?
fn plays_on(s: &Status, dev: &str) -> bool {
    s.endpoint.as_deref().is_some_and(|e| e.strip_prefix(dev).is_some_and(|r| r.starts_with('/')))
}

/// How long `hdmi-off` waits for the mixer to leave the GPU controller: twice what a retiring output takes to
/// drain (lead + drain margin) plus four periods, from CONFIG `snd_latency_ms`.
pub fn leave_bound_ns(c: &crate::mixer::Config) -> u64 {
    let frames = c.lead as u64 + c.drain_margin as u64 + 4 * c.period as u64;
    2 * frames * 1_000_000_000 / crate::resample::OUT_RATE as u64
}

/// Make before break for `hdmi-off`: the target was moved off HDMI, so wait (bounded) until the mixer has
/// switched and its HDMI output has drained, before the controller stops under it. When the target did not move
/// and the mixer plays there (auto picked HDMI), nothing can move it: the controller's stop takes the stream
/// (hda refuses it, the mixer reopens by its target), as the monitor's `snd hdmi off` does.
fn leave_controller(m: &crate::Mixer, dev: &str, moved: bool, source: &'static str) {
    let s = m.status();
    if !on_controller(&s, dev) {
        return;
    }
    if !moved && plays_on(&s, dev) {
        klog::event!(Info, "snd", "snd.output_hdmi_playing", source = source, endpoint = s.endpoint.as_deref().unwrap_or("");
            "the target keeps HDMI: the stop takes the stream, the mixer reopens by its target");
        return;
    }
    let bound = leave_bound_ns(&crate::thread::config());
    let start = time::now_ns();
    loop {
        sched::sleep_ms(5);
        let s = m.status();
        if m.stopped() || !on_controller(&s, dev) {
            klog::event!(Debug, "snd", "snd.output_left", source = source, ctrl = dev, waited_us = (time::now_ns() - start) / 1000);
            return;
        }
        if time::now_ns().saturating_sub(start) >= bound {
            klog::event!(Warn, "snd", "snd.output_leave_timeout", source = source, ctrl = dev, bound_ms = bound / 1_000_000,
                endpoint = s.endpoint.as_deref().unwrap_or("none"), streams = s.streams; "turning HDMI off anyway");
            return;
        }
    }
}

fn sys_get() -> String {
    target_name(&current_target())
}

fn sys_status() -> String {
    let (playing, clients) = match crate::started() {
        Some(m) => {
            let s = m.status();
            (s.endpoint, s.clients)
        }
        None => (None, 0),
    };
    let hdmi = if hda::hdmi::gpu_controller().is_none() { "absent" } else { display_name(hda::hdmi::display_state_now()) };
    status_text(mixer_state(), playing.as_deref(), clients, hdmi)
}

/// Read-only: Linux's kernfs refuses a write open of a file without a store with EACCES (linux-6.6/fs/kernfs/
/// file.c kernfs_fop_open), root too; the attribute layer only checks the uid at open, so root gets it here.
fn sys_status_set(_v: &str) -> Result<(), Errno> {
    Err(EACCES)
}

fn sys_set(v: &str) -> Result<(), Errno> {
    let r = parse_request(v).ok_or_else(|| {
        klog::event!(Info, "snd", "snd.output_refused", source = "sysfs", errno = EINVAL; "not auto, jack, hdmi, usb or hdmi-off");
        EINVAL
    })?;
    let n = sysfs_wait_ns();
    let rq = r.clone();
    let w = crate::helper::run("snd/output-req", n, move |t| {
        let _one = APPLY.lock();
        if !t.begin() {
            klog::event!(Info, "snd", "snd.output_withdrawn", source = "sysfs", request = format!("{rq:?}"); "the writer gave up first: nothing done");
            return None;
        }
        touch();
        Some(apply_locked(&rq, "sysfs"))
    });
    sys_errno(w, &r, n)
}

/// The writer's answer to its request (SN-12 fixer: an error means nothing changed, as a Linux store's does,
/// except `Late`, which only a request step missing its own bound causes). Logs, then [`waited_errno`].
fn sys_errno(w: crate::helper::Waited<Result<String, Refusal>>, r: &Request, n: u64) -> Result<(), Errno> {
    use crate::helper::Waited;
    match &w {
        Waited::Done(_) => {}
        Waited::NoThread => klog::event!(Warn, "snd", "snd.output_refused", source = "sysfs", errno = ENOMEM; "no thread for the request"),
        Waited::Cancelled { signal: true } => klog::event!(Info, "snd", "snd.output_refused", source = "sysfs", errno = EINTR, request = format!("{r:?}");
            "a signal came before the request began: nothing done"),
        Waited::Cancelled { signal: false } => klog::event!(Warn, "snd", "snd.output_refused", source = "sysfs", errno = EBUSY, bound_ms = n / 1_000_000,
            request = format!("{r:?}"); "another output request held snd.output past the bound: nothing done"),
        Waited::Late => klog::event!(Warn, "snd", "snd.output_slow", source = "sysfs", bound_ms = n / 1_000_000, request = format!("{r:?}");
            "the request began and missed the bound: it runs on, its outcome is in the log and in snd_output"),
    }
    waited_errno(w)
}

/// What the write(2) answers for each outcome of the file request.
pub(crate) fn waited_errno(w: crate::helper::Waited<Result<String, Refusal>>) -> Result<(), Errno> {
    use crate::helper::Waited;
    match w {
        Waited::Done(res) => res.map(|_| ()).map_err(|e| e.errno()),
        Waited::NoThread => Err(ENOMEM),
        Waited::Cancelled { signal: true } => Err(EINTR),
        Waited::Cancelled { signal: false } => Err(EBUSY),
        Waited::Late => Err(EIO),
    }
}

/// SN-12: a write(2) holds the file's position lock (`vfs.file.pos`, a subsystem lock) while the attribute
/// stores, and the request takes `snd.output` and `hda.hdmi`, which sit outside every subsystem lock: so the
/// request runs in a helper thread of its own with nothing held (`helper`), and the writer waits for it,
/// bounded (B4: a settings program may be Interactive), once for [`APPLY`] and once for the begun request. The
/// bound is far above what a request takes (the leave wait plus 10 s for hda's restart and the display writes,
/// which have their own bounds).
fn sysfs_wait_ns() -> u64 {
    leave_bound_ns(&crate::thread::config()) + 10_000_000_000
}

fn start() -> Result<(), &'static str> {
    vfs_sys::bitsos::register("snd_output", sys_get, sys_set);
    vfs_sys::bitsos::register("snd_output_status", sys_status, sys_status_set);
    crate::boot::spawn();
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "snd-output", start);

/// The `snd` tool's status: target, mixer, HDMI, every output with its plug state (`*` = playing now), every client.
pub fn status_lines() -> Vec<String> {
    let mut out = Vec::new();
    let m = crate::started();
    let st = m.as_ref().map(|m| m.status());
    let playing = st.as_ref().and_then(|s| s.endpoint.clone());
    out.push(format!("target={} (write: {VALUES})", target_name(&current_target())));
    match &st {
        Some(s) => out.push(format!(
            "mixer={} playing={} streams={} clients={} played={} hw_underruns={} switches={} opens={} open_failures={}",
            mixer_state(),
            playing.as_deref().unwrap_or("none"),
            s.streams,
            s.clients,
            s.played,
            s.hw_underruns,
            s.switches,
            s.opens,
            s.open_failures
        )),
        None => out.push(String::from("mixer=idle playing=none (starts with the first client)")),
    }
    match hda::hdmi::gpu_controller() {
        Some(d) => out.push(format!(
            "hdmi display={} controller={} streaming={}",
            display_name(hda::hdmi::display_state_now()),
            d.ctrl.name(),
            if d.ctrl.read_only() { "no" } else { "yes" }
        )),
        None => out.push(String::from("hdmi display=absent (no GPU HD Audio function)")),
    }
    let mark = |name: &str| if playing.as_deref() == Some(name) { " *" } else { "" };
    let yn = |b: Option<bool>| match b {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    };
    for d in hda::controllers() {
        for o in d.ctrl.outputs() {
            let port = if o.port == devmodel::audio::Output::Hdmi { Port::Hdmi } else { Port::Jack };
            let name = format!("{}/{}/{}:{:#x}", d.ctrl.name(), port_name(port), o.id.cad, o.id.pin);
            out.push(format!(
                "output {name} {} kind={:?} plugged={} playable={}{}",
                d.ctrl.kind().name(),
                o.kind,
                yn(o.plugged),
                if o.playable { "yes" } else { "no" },
                mark(&name)
            ));
        }
    }
    for o in usb_audio::outputs() {
        let name = format!("{}/usb/{}:{:#x}", o.key, o.control, o.iface);
        out.push(format!("output {name} usb channels={} terminal={:#06x} plugged=yes playable=yes{}", o.channels, o.terminal_type, mark(&name)));
    }
    if let Some(m) = &m {
        for c in m.clients() {
            out.push(format!(
                "client {} {:?} {} Hz {} ch {} bit {} volume={}/{} consumed={} underruns={}",
                c.id,
                c.name,
                c.fmt.rate,
                c.fmt.channels,
                c.fmt.bits,
                c.state.name(),
                c.volume[0],
                c.volume[1],
                c.consumed,
                c.underruns
            ));
        }
    }
    out
}
