//! GX-8c: the chip comes up by itself at every boot (Georg 18.11.: no key keeps it down). The kernel runs what
//! `gx_bringup {"until":"backend"}` runs (`psplive::LiveBoot`, VCN included), in a thread of its own, beside init. The
//! desktop's start (`console_cmds::desktop`, the only one who starts bitscomp) waits for its end until `BOUND_NS` after
//! the start (`console::bootscreen::hold_desktop`). MCP and the console's shell never wait.
//!
//! - Every failure after a write ends with what `until=stop` does (a refusal before the first write stops nothing); no
//!   node appears, the reason is in the log and in `gx_bringup {"until":"boot"}`, and there is no desktop: the boot
//!   says so in one line on the text console (`console::bootscreen::desktop_no_gpu`). MCP always answers.
//! - The chain's own waits are bounded (polls, the jobs' 70 s), but their sum may pass the bound when a chip misbehaves.
//!   The desktop's start then gives up on the GPU (the same one line), and the thread ends its chain behind it. No
//!   stage starts and no node appears past the bound: the bound is the hang guard of the boot.
//! - A machine without a function the 780M's driver drives tells the desktop's start the same (no desktop without a
//!   GPU: there is no software drawing).
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

/// From the thread's start to the last moment for a node and for the desktop's wait (the hang guard).
pub const BOUND_NS: u64 = 30_000_000_000;
/// How often the thread looks for the stick. GS-1: the desktop waits for this thread, so a look every 100 ms cost up to
/// 100 ms of the start for nothing; a path lookup every 10 ms for the few seconds of the mount costs nothing.
pub const STICK_POLL_NS: u64 = 10_000_000;
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
    /// `start` has not looked at this slot yet.
    NotYet,
    /// No AMD GPU function on this machine.
    NoChip,
    Running,
    Done(Report),
}

/// GP-GPU1 (plan/spec/gpu-rdna4.md piece 3): how one device's bring-up at boot went (the statics of before). A device has one
/// (`devs::Device::boot`); the registry has one for the machine itself, which says "no AMD GPU function" where there
/// is no device to ask, and takes the scenario's stand-in.
pub struct BootSlot {
    state: Mutex<State>,
    lost: AtomicBool,
}

impl BootSlot {
    pub const fn new() -> BootSlot {
        BootSlot { state: Mutex::named("amdgpu.hw.boot", State::NotYet), lost: AtomicBool::new(false) }
    }
}

impl Default for BootSlot {
    fn default() -> BootSlot {
        BootSlot::new()
    }
}

/// The kernel's call before init starts: every device the 780M's driver drives gets its thread; a machine without one
/// costs nothing and tells the desktop's start that there is no GPU. `get` is CONFIG, for the test-only stand-in.
pub fn start(get: &dyn Fn(&str) -> Option<String>) {
    let reg = crate::devs::registry();
    // The scenario's stand-in (a machine with a chip has none to spare, and no test may play it there).
    if let Some(s) = get("gpu_boot_fake").as_deref().and_then(Script::parse) {
        if hal::cpu::detect().hypervisor {
            spawn(&reg.machine, ScriptChip { script: s, gate: ToolGate::new() }, BOUND_NS);
            return;
        }
        klog::event!(Warn, "gx", "boot.fake_refused"; "gpu_boot_fake only under a hypervisor");
    }
    // Every device the 780M's driver drives comes up by itself; a card attached read-only has no bring-up (GP-GPU1).
    let mut any = false;
    for dev in reg.all() {
        match crate::psplive::live_boot(dev) {
            Some(chip) => {
                any = true;
                spawn(&dev.boot, chip, BOUND_NS);
            }
            // A function the 780M's driver looked at and refused answers as a machine without the chip always did.
            None => *dev.boot.state.lock() = State::NoChip,
        }
    }
    if !any {
        *reg.machine.state.lock() = State::NoChip;
        match reg.all().iter().filter(|d| d.family == crate::devs::Family::Rdna4).count() {
            0 => klog::event!(Info, "gx", "boot.no_chip"; "this machine has no AMD GPU function: no desktop, the text console stays"),
            cards => klog::event!(Info, "gx", "boot.read_only", cards = cards; "the cards here are attached read-only (GP-GPU1), nothing is brought up: the text console stays"),
        }
        console::bootscreen::desktop_no_gpu(String::from(NO_CHIP_LINE));
    }
}

/// The desktop is held from here, whatever happens to the thread: the hold ends at the deadline by itself.
pub(crate) fn spawn<C: Chip + Send + 'static>(slot: &'static BootSlot, chip: C, bound_ns: u64) {
    let deadline = chip.now_ns().saturating_add(bound_ns);
    *slot.state.lock() = State::Running;
    console::bootscreen::hold_desktop(deadline);
    // GS-1: `at_ms` is the monotonic clock (CLOCK_MONOTONIC in user space): the desk's start table lines up on it.
    klog::event!(Info, "gx", "boot.start", bound_s = bound_ns / 1_000_000_000, at_ms = time::now_ns() / 1_000_000);
    let started = sched::spawn("gpu-boot", sched::Class::Normal, move || {
        let r = run(&chip, deadline);
        finish(slot, r);
    });
    if let Err(e) = started {
        finish(slot, Report { outcome: Outcome::Failed { fail: Fail::Refused, why: format!("no thread: {e:?}"), stop: StopEnd::NotNeeded }, lines: Vec::new(), took_ns: 0 });
    }
}

/// The text console's one line on a machine without a GPU the driver brings up.
pub const NO_CHIP_LINE: &str = "Dieser Rechner hat keine Grafik, die BitsOS hochfährt: keine Arbeitsfläche, Textkonsole.";

/// The text console's one line after a failed bring-up (`stage`: `Fail::word`).
pub fn failed_line(stage: &str) -> String {
    format!("Die Grafik ist nicht hochgekommen ({stage}): keine Arbeitsfläche, Textkonsole.")
}

/// The log's event, the answer kept, the desktop told (in this order: what the desktop may look at is written).
fn finish(slot: &BootSlot, r: Report) {
    let ms = r.took_ns / 1_000_000;
    let at_ms = time::now_ns() / 1_000_000;
    let line = match &r.outcome {
        Outcome::Up(m) => {
            klog::event!(Info, "gx", "boot.up", minor = *m, after_ms = ms, at_ms = at_ms);
            None
        }
        Outcome::Failed { fail, why, stop } => {
            klog::event!(Error, "gx", "boot.failed", stage = fail.word(), stop = stop.word(), after_ms = ms, at_ms = at_ms, why = why.as_str();
                "no desktop: the text console stays");
            Some(failed_line(fail.word()))
        }
    };
    *slot.state.lock() = State::Done(r);
    match line {
        None => console::bootscreen::desktop_gpu_up(),
        Some(l) => console::bootscreen::desktop_no_gpu(l),
    }
}

impl BootSlot {
    /// GX-9a: a hang ended the device for this boot, after the desktop came up on it. The node stays and answers ENODEV
    /// (the GPU's users see a lost device, as on Linux), and this is what the tool says from now on.
    pub fn note_device_lost(&self) {
        self.lost.store(true, Ordering::Release);
    }

    /// `gx_bringup {"until":"boot"}`: how the boot's bring-up went, with all its lines. Changes nothing.
    pub fn answer(&self) -> String {
        const NOTHING: &str = "gx_bringup: REFUSED (no PSP or SMU register written)\n";
        match &*self.state.lock() {
            State::NotYet => format!("{NOTHING}boot: not started yet\n"),
            State::NoChip => format!("{NOTHING}boot: no AMD GPU function on this machine\n"),
            State::Running => format!("{NOTHING}boot: still running\n"),
            State::Done(r) => {
                let mut s = render(r);
                if self.lost.load(Ordering::Acquire) {
                    s += "boot: a hang ended the device afterwards: submissions answer ENODEV until until=stop and until=backend, or the next boot\n";
                }
                s
            }
        }
    }
}

/// The answer for a tool call without a device of its own: the 780M's bring-up, else the machine's slot (no device,
/// or the scenario's stand-in).
pub fn answer() -> String {
    let reg = crate::devs::registry();
    match reg.first(crate::devs::Family::Phoenix) {
        Some(d) => d.boot.answer(),
        None => {
            let mut s = reg.machine.answer();
            let cards = reg.all().iter().filter(|d| d.family == crate::devs::Family::Rdna4).count();
            if cards > 0 {
                s += &format!("boot: {cards} RX 9070 XT attached read-only (GP-GPU1): they have no bring-up yet; gpu {{\"device\":\"list\"}} names them\n");
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

/// `gpu_boot_fake=` (test only, refused outside a hypervisor): what the scenario host/tests/gx/10-boot.sh has the
/// boot's bring-up do under QEMU, which has no chip.
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
            Script::Up => {
                // GS-1: the chip's mark between its engines and its proof, so a scenario sees the whole chain of marks.
                klog::event!(Info, "gx", "boot.engines_up", wrote = true, at_ms = time::now_ns() / 1_000_000);
                drm::device::devices().first().map(|d| d.render_minor()).ok_or(Fail::Node)
            }
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
