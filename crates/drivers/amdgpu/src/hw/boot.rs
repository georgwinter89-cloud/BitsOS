//! GX-8c: the chip comes up by itself at boot. CONFIG `gpu=on` (WY-21: the default; `gpu=off` keeps the chip down; not a
//! guarded key) runs what
//! `gx_bringup {"until":"backend"}` runs (`psplive::LiveBoot`), in a thread of its own, beside init. The desktop's start
//! (`console_cmds::desktop`, the only one who starts bitscomp) waits for its end until `BOUND_NS` after the start
//! (`console::bootscreen::hold_desktop`). MCP and the console's shell never wait.
//!
//! - Every failure after a write ends with what `until=stop` does (a refusal before the first write stops nothing); no node appears, the reason is in the log and in
//!   `gx_bringup {"until":"boot"}`, and the desktop starts as without the GPU. The machine always reaches a desktop and MCP.
//! - The chain's own waits are bounded (polls, the jobs' 70 s), but their sum may pass the bound when a chip misbehaves.
//!   The desktop then goes on, and the thread ends its chain behind it. No stage starts and no node appears past the
//!   bound: a node that comes while the desktop runs without it would help nothing, the bound is one number for both.
//! - Stopping needs no `again`: a boot is a new kernel, its statics count no earlier run, and `classify` reads what the
//!   Quiesce hook left before the Reload. `until=stop` and `until=backend` work on a chip that came up here as on one
//!   the tool brought up: the same `LIVE`, `NODE` and `TOOL`.
//!
//! The firmware lies on the stick, which mounts after the drivers bound (at most a few seconds on the Mini-PC, init waits
//! up to 15 s for it), so the wait for it is part of the bound. Measured on the Mini-PC (plan/hw/gx-780m-minipc-2026-10-28/g6-log-gx.txt,
//! `gx state.classified` to the last SDMA rung; 11-03's captures for MES): the stages take under 2 s, of which GC's boot 30 ms; the
//! proof has never run on the chip. 30 s leaves room for the stick's 15 s and for a slow first proof, and is what a
//! failing chip can cost the desktop at most.

use super::live::ToolGate;
use crate::gpucore::psp::Until;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::Mutex;

/// From the thread's start to the last moment for a node and for the desktop's wait.
pub const BOUND_NS: u64 = 30_000_000_000;
/// How often the thread looks for the stick.
pub const STICK_POLL_NS: u64 = 100_000_000;
/// Where the stick's files lie.
pub const STICK_DIR: &str = "/boot/BITSOS2";
/// The reason's longest form in the log (the whole text stays in `answer`).
const WHY_MAX: usize = 200;

/// Where a boot's bring-up ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// The stick did not mount within the bound: no firmware, nothing touched.
    NoStick,
    /// A `gx_bringup` over MCP holds the chip: it is that call's, nothing here touches it.
    Busy,
    /// Refused before anything was written: nothing to stop, and a chain that is kept is the `gx_bringup` call's that kept it
    /// (a refusal after a stage of this run is `Stage`).
    Refused,
    /// A stage of `Until::BACKEND_STAGES` failed.
    Stage(Until),
    /// The chip's facts could not be read on the kept chain.
    Device,
    /// `hw::check` failed.
    Proof,
    /// The picture moved (GX-D9).
    Picture,
    /// Past the bound.
    Late,
    /// The render node could not be registered.
    Node,
}

impl Fail {
    /// The word in the log and on the boot screen.
    pub fn word(self) -> &'static str {
        match self {
            Fail::NoStick => "stick",
            Fail::Busy => "busy",
            Fail::Refused => "refused",
            Fail::Stage(Until::Sdma) => "sdma",
            Fail::Stage(Until::GfxMes) => "gfx_mes",
            Fail::Stage(_) => "stage",
            Fail::Device => "device",
            Fail::Proof => "proof",
            Fail::Picture => "picture",
            Fail::Late => "late",
            Fail::Node => "node",
        }
    }

    /// Whether this run may have left the chip in any state but the one it was found in.
    pub fn stops(self) -> bool {
        !matches!(self, Fail::NoStick | Fail::Busy | Fail::Refused)
    }
}

/// How the stop after a failure ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopEnd {
    /// Nothing was touched.
    NotNeeded,
    Stopped,
    /// It refused or did not prove the engines quiet (its lines say what): a reboot before more GPU work.
    Left,
}

impl StopEnd {
    pub fn word(self) -> &'static str {
        match self {
            StopEnd::NotNeeded => "not_needed",
            StopEnd::Stopped => "done",
            StopEnd::Left => "left",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The node is up on this render minor.
    Up(u32),
    Failed { fail: Fail, why: String, stop: StopEnd },
}

/// What one boot's bring-up did; the lines are `until=backend`'s, then `until=stop`'s.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub lines: Vec<String>,
    pub took_ns: u64,
}

/// What the boot's bring-up runs on: the chip (`psplive::LiveBoot`) or a fake.
pub trait Chip {
    /// `gx_bringup`'s single-flight gate.
    fn gate(&self) -> &ToolGate;
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
    fn stick_ready(&self) -> bool;
    /// `until=backend` with no stage begun and no node made past `node_by`: the render minor.
    fn backend(&self, node_by: u64, lines: &mut Vec<String>) -> Result<u32, Fail>;
    /// `until=stop`.
    fn stop(&self, lines: &mut Vec<String>) -> Result<bool, ()>;
}

/// The stage or the node may still begin.
pub fn in_time(now_ns: u64, node_by: u64) -> bool {
    now_ns <= node_by
}

/// The line that says why: the first that says FAIL or FAILED, else the last.
pub fn why_of(lines: &[String]) -> String {
    let line = lines.iter().find(|l| l.contains("FAIL")).or(lines.last()).map_or("", String::as_str);
    let end = line.char_indices().nth(WHY_MAX).map_or(line.len(), |(i, _)| i);
    String::from(&line[..end])
}

/// The bring-up, start to end. Never leaves a chain up without a node.
pub fn run(c: &dyn Chip, deadline: u64) -> Report {
    let t0 = c.now_ns();
    let mut lines = Vec::new();
    let end = |c: &dyn Chip, outcome, lines| Report { outcome, lines, took_ns: c.now_ns().saturating_sub(t0) };
    while !c.stick_ready() {
        if !in_time(c.now_ns(), deadline) {
            lines.push(format!("boot: {STICK_DIR} did not come within the bound: no firmware, the chip was not touched"));
            let why = why_of(&lines);
            return end(c, Outcome::Failed { fail: Fail::NoStick, why, stop: StopEnd::NotNeeded }, lines);
        }
        c.sleep_ns(STICK_POLL_NS);
    }
    let Some(_one) = c.gate().enter() else {
        lines.push(String::from("boot: a gx_bringup over MCP runs: the chip is that call's, nothing was touched"));
        let why = why_of(&lines);
        return end(c, Outcome::Failed { fail: Fail::Busy, why, stop: StopEnd::NotNeeded }, lines);
    };
    let fail = match c.backend(deadline, &mut lines) {
        Ok(m) => return end(c, Outcome::Up(m), lines),
        Err(f) => f,
    };
    let why = why_of(&lines);
    let stop = if !fail.stops() {
        StopEnd::NotNeeded
    } else if c.stop(&mut lines) == Ok(true) {
        StopEnd::Stopped
    } else {
        StopEnd::Left
    };
    end(c, Outcome::Failed { fail, why, stop }, lines)
}

enum State {
    /// CONFIG `gpu` is not on.
    Off,
    /// No AMD GPU function on this machine.
    NoChip,
    Running,
    Done(Report),
}

static STATE: Mutex<State> = Mutex::named("amdgpu.hw.boot", State::Off);

/// GX-9a: CONFIG `gx_mode2=on`, read once at boot, opens the mode-2 rung (`mode2_open`).
static MODE2: AtomicBool = AtomicBool::new(false);

/// What the desk's measurement names when the rung is open (GX-D9: mode 2 is unmeasured until then).
pub const MODE2_PROOF: &str = "CONFIG gx_mode2=on: the desk's measurement of what mode 2 does to DCN";

/// The rung is open. The chain asks again at the message itself: this is a second lock next to `Policy`'s.
pub fn mode2_open() -> bool {
    MODE2.load(Ordering::Acquire)
}

/// `gx_mode2=on` opens it, anything else leaves it closed.
pub(crate) fn read_mode2(get: &dyn Fn(&str) -> Option<String>) -> bool {
    match get("gx_mode2").as_deref().map(str::trim) {
        None | Some("") | Some("off") => false,
        Some("on") => {
            klog::event!(Warn, "gx", "boot.mode2_open"; "CONFIG gx_mode2=on: an unrecoverable hang may send GfxDeviceDriverReset(2); what it does to DCN is what the desk measures");
            true
        }
        Some(other) => {
            klog::event!(Warn, "gx", "boot.config_unknown", value = other; "gx_mode2= takes on or off: off");
            false
        }
    }
}

/// The kernel's call before init starts: CONFIG `gpu=on`, or no `gpu=` (WY-21), starts the thread; `gpu=off` costs nothing,
/// and so does a machine without the chip.
pub fn start(get: &dyn Fn(&str) -> Option<String>) {
    MODE2.store(read_mode2(get), Ordering::Release);
    match get("gpu").as_deref().map(str::trim) {
        Some("off") => return,
        None | Some("") | Some("on") => {}
        Some(other) => {
            klog::event!(Warn, "gx", "boot.config_unknown", value = other; "gpu= takes on or off: off");
            return;
        }
    }
    // The scenario's stand-in (a machine with a chip has none to spare, and no test may play it there).
    if let Some(s) = get("gpu_boot_fake").as_deref().and_then(Script::parse) {
        if hal::cpu::detect().hypervisor {
            spawn(ScriptChip { script: s, gate: ToolGate::new() }, BOUND_NS);
            return;
        }
        klog::event!(Warn, "gx", "boot.fake_refused"; "gpu_boot_fake only under a hypervisor");
    }
    match crate::psplive::live_boot() {
        Some(chip) => spawn(chip, BOUND_NS),
        None => {
            *STATE.lock() = State::NoChip;
            klog::event!(Info, "gx", "boot.no_chip"; "gpu=on, and this machine has no AMD GPU function");
        }
    }
}

/// The desktop is held from here, whatever happens to the thread: the hold ends at the deadline by itself.
pub(crate) fn spawn<C: Chip + Send + 'static>(chip: C, bound_ns: u64) {
    let deadline = chip.now_ns().saturating_add(bound_ns);
    *STATE.lock() = State::Running;
    console::bootscreen::hold_desktop(deadline);
    klog::event!(Info, "gx", "boot.start", bound_s = bound_ns / 1_000_000_000);
    let started = sched::spawn("gpu-boot", sched::Class::Normal, move || {
        let r = run(&chip, deadline);
        finish(r);
    });
    if let Err(e) = started {
        finish(Report { outcome: Outcome::Failed { fail: Fail::Refused, why: format!("no thread: {e:?}"), stop: StopEnd::NotNeeded }, lines: Vec::new(), took_ns: 0 });
    }
}

/// The log's event, the answer kept, the desktop let go (in this order: what the desktop may look at is written).
fn finish(r: Report) {
    let ms = r.took_ns / 1_000_000;
    match &r.outcome {
        Outcome::Up(m) => klog::event!(Info, "gx", "boot.up", minor = *m, after_ms = ms),
        Outcome::Failed { fail, why, stop } => klog::event!(Error, "gx", "boot.failed", stage = fail.word(), stop = stop.word(), after_ms = ms, why = why.as_str();
            "the desktop starts without the GPU"),
    }
    *STATE.lock() = State::Done(r);
    console::bootscreen::release_desktop();
}

/// GX-9a: a hang ended the device for this boot, after the desktop came up on it. The node stays and answers ENODEV: what the
/// GPU's users do with that is the fallback GX-8c gives them at boot, and this is what the tool says from now on.
pub fn note_device_lost() {
    DEVICE_LOST.store(true, Ordering::Release);
}

static DEVICE_LOST: AtomicBool = AtomicBool::new(false);

/// `gx_bringup {"until":"boot"}`: how the boot's bring-up went, with all its lines. Changes nothing.
pub fn answer() -> String {
    const NOTHING: &str = "gx_bringup: REFUSED (no PSP or SMU register written)\n";
    match &*STATE.lock() {
        State::Off => format!("{NOTHING}boot: CONFIG gpu=off\n"),
        State::NoChip => format!("{NOTHING}boot: no AMD GPU function on this machine\n"),
        State::Running => format!("{NOTHING}boot: still running\n"),
        State::Done(r) => {
            let mut s = render(r);
            if DEVICE_LOST.load(Ordering::Acquire) {
                s += "boot: a hang ended the device afterwards: submissions answer ENODEV until until=stop and until=backend, or the next boot\n";
            }
            s
        }
    }
}

/// The answer for one report; its first line says PASS or FAIL, as every `gx_bringup`.
pub fn render(r: &Report) -> String {
    let (head, what) = match &r.outcome {
        Outcome::Up(m) => ("gx_bringup: PASS", format!("boot: up on renderD{m}")),
        Outcome::Failed { fail, why, stop } => ("gx_bringup: FAIL", format!("boot: failed at {}, stop {}: {why}", fail.word(), stop.word())),
    };
    let mut s = format!("{head}\n{what} after {} ms\n", r.took_ns / 1_000_000);
    for l in &r.lines {
        s += l;
        s.push('\n');
    }
    s
}

/// `gpu_boot_fake=`: what the scenario host/tests/gx/10-boot.sh has the boot's bring-up do under QEMU, which has no chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Script {
    /// Passes, on the fake's node (`drm=fake`).
    Up,
    /// A stage fails.
    Fail,
    /// The chain outlives the bound.
    Hang,
}

impl Script {
    fn parse(v: &str) -> Option<Script> {
        match v.trim() {
            "up" => Some(Script::Up),
            "fail" => Some(Script::Fail),
            "hang" => Some(Script::Hang),
            _ => None,
        }
    }
}

pub(crate) struct ScriptChip {
    pub(crate) script: Script,
    pub(crate) gate: ToolGate,
}

impl Chip for ScriptChip {
    fn gate(&self) -> &ToolGate {
        &self.gate
    }
    fn now_ns(&self) -> u64 {
        super::clock_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns);
    }
    fn stick_ready(&self) -> bool {
        vfs::root_ns().lookup(STICK_DIR).is_ok()
    }
    fn backend(&self, node_by: u64, lines: &mut Vec<String>) -> Result<u32, Fail> {
        match self.script {
            Script::Up => drm::device::devices().first().map(|d| d.render_minor()).ok_or(Fail::Node),
            Script::Fail => {
                lines.push(String::from("script: stage FAIL"));
                Err(Fail::Stage(Until::Sdma))
            }
            Script::Hang => {
                sched::sleep_ns(node_by.saturating_sub(self.now_ns()).saturating_add(3_000_000_000));
                lines.push(String::from("script: the chain ended past the bound: FAIL"));
                Err(Fail::Late)
            }
        }
    }
    fn stop(&self, lines: &mut Vec<String>) -> Result<bool, ()> {
        lines.push(String::from("script: stopped as until=stop does"));
        Ok(true)
    }
}
