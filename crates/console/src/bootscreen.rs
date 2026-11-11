//! DS-4: the boot screen at run time. The console shows it from `configure` on (the first moment the kernel has
//! a picture and its CONFIG) when the desktop will start (`desktop=`) and the screen is big enough for the scene.
//!
//! - The step sink (`bootsteps`) gets every kernel log line, also the ones from before the console (replay), and
//!   only updates the model: it never draws, logs or waits (it runs in klogd, or in an emitter before klogd).
//! - IN-9: the step events the level filter drops (Georg's normal `loglevel=error` drops every Info line before
//!   the ring and any sink) reach the model through klog's watch (`step_wanted`/`step_line`, installed by the
//!   kernel before `kernel start`): at once, in the emitter's context, and never into the ring, the mirror or the
//!   console. So the list and the desktop start (`init_state`) work at every level. The model's lock is an
//!   `IrqSpinLock` because a watched line may come from any context (an IRQ handler, the boot before `sti`),
//!   and a leaf: `feed` never allocates (`stepdetail`) and takes no other lock, so a watched event may be logged
//!   under any lock but this one (nothing logs under it). Readers copy the model out (a few KB, no heap) and do
//!   their allocating work after the lock: interrupts are off only for that copy.
//! - The `bootscreen` thread (Background: the start never waits for its picture) draws the whole scene once and
//!   then only the rows that changed, polling the model's `seq` every `POLL_NS` while the screen is up; it ends
//!   with the screen. No frame is drawn while nothing changes.
//! - The screen ends when a program takes the screen (`vt::set_kd_mode`, DS-5 continues from `/proc/bitsos/boot`),
//!   on any key (the text console is one key away), when the desktop start gives up (`fallback`), on a panic
//!   (`end_for_panic`, lock-free) and at the latest `CAP_NS` after `kernel ready`.
//!
//! While it is up the VT keeps its text model but draws nothing (like KD_GRAPHICS) and redraws whole at the end.

use crate::bootdraw::{Scene, View};
use crate::bootsteps::{Init, Steps};
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use ksync::{IrqSpinLock, SpinLock};

/// How often the drawing thread looks at the model while the screen is up.
const POLL_NS: u64 = 15_000_000;
/// The longest the boot screen stays after `kernel ready` without anyone taking the screen.
const CAP_NS: u64 = 90_000_000_000;

static STEPS: IrqSpinLock<Steps> = IrqSpinLock::named("console.bootsteps", Steps::new());
klog::counter!(WATCHED, "console.bootsteps.watched");
klog::counter!(UNPARSED, "console.bootsteps.watch_unparsed");
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// 0 never shown, 1 shown, 2 handed to a program, 3 ended to the text console.
static SCREEN: AtomicU8 = AtomicU8::new(0);
static DRAWN: AtomicU64 = AtomicU64::new(0);
/// CONFIG `desktop=` resolved: the console starts the desktop (console-cmds reads it).
static DESKTOP: AtomicBool = AtomicBool::new(false);
static PROGRAM: SpinLock<Option<String>> = SpinLock::named("console.desktop_program", None);
static SINK_ADDED: AtomicBool = AtomicBool::new(false);
/// GX-8c: the desktop's start waits until this `time::now_ns` (0: nothing holds it).
static DESKTOP_HOLD: AtomicU64 = AtomicU64::new(0);
/// A panic ended the boot screen and the text console is not painted over it yet: every emergency line retries
/// until one gets the VT and the frame (the first may find either busy).
static PANIC_REPAINT: AtomicBool = AtomicBool::new(false);

/// The desktop program when CONFIG `desktop_program=` names none.
pub const DEFAULT_PROGRAM: &str = "/bin/bitscomp";

/// `desktop=`: (start the desktop, why). `auto` (default) = on real hardware, off under a hypervisor (QEMU tests
/// keep today's console unless they ask with `desktop=on`, like `serial=auto`).
pub fn resolve(value: Option<&str>, hypervisor: bool) -> Result<(bool, &'static str), ()> {
    match value.map(str::trim) {
        None | Some("auto") => Ok(if hypervisor { (false, "auto, hypervisor") } else { (true, "auto") }),
        Some("on") => Ok((true, "on")),
        Some("off") => Ok((false, "off")),
        Some(_) => Err(()),
    }
}

/// The boot screen is on the display (the VT draws nothing meanwhile).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// The console starts the desktop after boot (console-cmds' `desktop` thread).
pub fn desktop_on() -> bool {
    DESKTOP.load(Ordering::Acquire)
}

pub fn desktop_program() -> String {
    PROGRAM.lock().clone().unwrap_or_else(|| String::from(DEFAULT_PROGRAM))
}

/// What became of init so far (from the log).
/// GX-8c: a driver that comes up at boot holds the desktop's start until `until_ns` at the latest.
pub fn hold_desktop(until_ns: u64) {
    DESKTOP_HOLD.store(until_ns, Ordering::Release);
}

/// GX-8c: its end; the desktop may start.
pub fn release_desktop() {
    DESKTOP_HOLD.store(0, Ordering::Release);
}

/// GX-8c: until when the desktop's start waits, if anything holds it.
pub fn desktop_hold() -> Option<u64> {
    Some(DESKTOP_HOLD.load(Ordering::Acquire)).filter(|&t| t != 0)
}

pub fn init_state() -> Init {
    STEPS.lock().init
}

/// A copy of the step list.
pub fn steps() -> Steps {
    *STEPS.lock()
}

struct StepSink;
impl klog::Sink for StepSink {
    fn write_line(&self, _level: klog::Level, line: &str) {
        STEPS.lock().feed(line);
    }
}
static STEP_SINK: StepSink = StepSink;

/// IN-9, klog's watch (CR IN-9-1): is `(subsystem, name)` a step event? Asked for every Info/Warn event the level
/// filter drops, before its fields are built: compares only.
pub fn step_wanted(subsystem: &str, name: &str) -> bool {
    Steps::watched(subsystem, name)
}

/// IN-9, klog's watch: one rendered step line the level filter dropped (same text as a sink's line). Any context
/// except the panic/NMI path (klog never watches `try_event!`).
pub fn step_line(line: &str) {
    WATCHED.inc();
    if crate::bootsteps::Line::parse(line).is_none() {
        UNPARSED.inc();
        return;
    }
    STEPS.lock().feed(line);
}

/// (step lines that came through the watch since boot, of them not readable). 0 at loglevel info: nothing is
/// dropped there.
pub fn watched_lines() -> (u64, u64) {
    (WATCHED.get(), UNPARSED.get())
}

/// CONFIG and the start of the boot screen (from `configure`, before the console's log sink is attached so the
/// replayed boot lines go into the text model only). `desktop=auto|on|off`, `desktop_program=<path> [args]`.
pub(crate) fn configure(get: &dyn Fn(&str) -> Option<String>) {
    let value = get("desktop");
    let hv = hal::cpu::detect().hypervisor;
    let (on, why) = match resolve(value.as_deref(), hv) {
        Ok(r) => r,
        Err(()) => {
            klog::event!(Warn, "console", "config.bad", key = "desktop", value = value.as_deref().unwrap_or(""); "want auto, on or off; keeping auto");
            resolve(None, hv).unwrap_or((false, "auto"))
        }
    };
    if let Some(p) = get("desktop_program").filter(|p| p.trim().starts_with('/')) {
        *PROGRAM.lock() = Some(String::from(p.trim()));
    }
    DESKTOP.store(on, Ordering::Release);
    klog::event!(Info, "console", "desktop.mode", on = on, why = why, program = desktop_program().as_str());
    if !SINK_ADDED.swap(true, Ordering::AcqRel) {
        klog::add_deferred_sink("bootsteps", &STEP_SINK, true);
    }
    if on {
        begin();
    } else {
        // The list says why nothing starts (DS-5 and `/proc/bitsos/boot` read it).
        klog::event!(Info, "console", "desktop.off", why = why);
    }
}

fn begin() {
    let Some((w, h)) = display::with_frame(|f| (f.width, f.height)) else { return };
    if Scene::new(w, h).is_none() {
        klog::event!(Info, "console", "bootscreen.skipped", width = w, height = h, art = crate::bootart::ready(); "the scene needs 2560x1440; the text console shows the start");
        return;
    }
    ACTIVE.store(true, Ordering::Release);
    SCREEN.store(1, Ordering::Release);
    if let Err(e) = sched::spawn("bootscreen", sched::Class::Background, thread) {
        ACTIVE.store(false, Ordering::Release);
        SCREEN.store(3, Ordering::Release);
        klog::event!(Error, "console", "bootscreen.spawn_failed", error = alloc::format!("{e:?}"));
        return;
    }
    klog::event!(Info, "console", "bootscreen.begin", width = w, height = h);
}

/// Ends the boot screen (`reason` for the log); true if it was up. `handed`: a program took the screen (the VT
/// stays dark); else the caller shows the text console (`vt::show_text`).
pub fn stop(reason: &'static str, handed: bool) -> bool {
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return false;
    }
    SCREEN.store(if handed { 2 } else { 3 }, Ordering::Release);
    klog::event!(Info, "console", "bootscreen.end", reason = reason, drawn = DRAWN.load(Ordering::Acquire));
    true
}

/// Panic/NMI path: off without a lock, an allocation or a log line; true while the text console still has to be
/// painted over the boot picture (the caller calls `panic_repainted` once it did).
pub fn end_for_panic() -> bool {
    if ACTIVE.swap(false, Ordering::AcqRel) {
        SCREEN.store(3, Ordering::Release);
        PANIC_REPAINT.store(true, Ordering::Release);
    }
    PANIC_REPAINT.load(Ordering::Acquire)
}

/// The panic path painted the text console over the boot picture.
pub fn panic_repainted() {
    PANIC_REPAINT.store(false, Ordering::Release);
}

/// The text console still has to be painted over the boot picture after a panic.
pub fn panic_repaint_pending() -> bool {
    PANIC_REPAINT.load(Ordering::Acquire)
}

/// ktest `in.boot.emergency`: the boot screen's state without its thread (the VT draws nothing meanwhile), so the
/// panic path is tested on every QEMU run, not only with `desktop=on` at 2560x1440.
pub(crate) fn pretend_up() {
    if !ACTIVE.swap(true, Ordering::AcqRel) {
        SCREEN.store(1, Ordering::Release);
    }
}

/// The desktop start gave up (or cannot start): the text console with `message` (console-cmds).
pub fn fallback(reason: &'static str, message: &str) {
    if stop(reason, false) {
        crate::vt::show_text();
    }
    crate::vt::write(message, crate::screen::index_of(crate::FG_WARN));
}

fn screen_name() -> &'static str {
    match SCREEN.load(Ordering::Acquire) {
        1 => "shown",
        2 => "handed",
        3 => "text",
        _ => "off",
    }
}

/// `/proc/bitsos/boot`: the step list for DS-5 (bitscomp continues it) and for MCP/tests.
fn proc_render() -> String {
    let s = *STEPS.lock();
    s.proc_text(screen_name(), DRAWN.load(Ordering::Acquire))
}
vfs::procfile!("/proc/bitsos/boot", proc_render);

fn thread() {
    let t0 = time::now_ns();
    let mut drawn: Option<(View, u32, u32)> = None;
    let mut ready_at: Option<u64> = None;
    loop {
        if !active() {
            return;
        }
        let (view, seq, ready) = {
            let (seq, ready) = {
                let s = STEPS.lock();
                (s.seq, s.kernel_ready)
            };
            let need = drawn.is_none() || seq != DRAWN.load(Ordering::Acquire);
            // The copy may be newer than `seq`: then the next poll draws once more, never too little.
            (need.then(|| View::of(&steps())), seq, ready)
        };
        if ready && ready_at.is_none() {
            ready_at = Some(time::now_ns());
        }
        if let Some(v) = view {
            let t = time::now_ns();
            let first = drawn.is_none();
            let changed = drawn.as_ref().and_then(|(old, w, h)| {
                display::with_frame(|f| {
                    // Checked under the frame lock: a `stop` before it never sees a late boot picture.
                    if !active() || (f.width, f.height) != (*w, *h) {
                        return None;
                    }
                    Scene::new(f.width, f.height)?.draw_changes(f, old, &v);
                    Some((f.width, f.height))
                })
                .flatten()
            });
            let size = match changed {
                Some(s) => Some(s),
                None if active() => draw_whole(&v),
                None => None,
            };
            match size {
                // A program took the screen meanwhile: its scanout, not the console frame (as `vt::present_as`).
                Some(_) if crate::vt::graphics() || !active() => return,
                Some((w, h)) => {
                    display::present();
                    DRAWN.store(seq, Ordering::Release);
                    if first {
                        klog::event!(Info, "console", "bootscreen.drawn", us = (time::now_ns() - t) / 1000, since_begin_ms = (t - t0) / 1_000_000);
                    }
                    drawn = Some((v, w, h));
                }
                None if active() => {
                    // The mode changed to one the scene does not fit: the text console takes over.
                    if stop("size", false) {
                        crate::vt::show_text();
                    }
                    return;
                }
                None => return,
            }
        }
        if ready_at.is_some_and(|r| time::now_ns().saturating_sub(r) > CAP_NS) {
            if stop("timeout", false) {
                crate::vt::show_text();
            }
            return;
        }
        sched::sleep_ns(POLL_NS);
    }
}

/// The whole picture in bands of `BAND_ROWS` rows, the frame lock released between them (B4: the KernelRt input
/// thread waits at most one band). None when the screen ended or the frame no longer fits the scene; a mode
/// change in the middle starts again at the new size.
fn draw_whole(v: &View) -> Option<(u32, u32)> {
    'size: for _ in 0..4 {
        let (w, h) = display::with_frame(|f| (f.width, f.height))?;
        let scene = Scene::new(w, h)?;
        for band in Scene::bands(w, h) {
            let ok = display::with_frame(|f| {
                if !active() {
                    return Some(false);
                }
                if (f.width, f.height) != (w, h) {
                    return None;
                }
                scene.draw_band(f, v, band);
                Some(true)
            })
            .flatten();
            match ok {
                Some(true) => {}
                Some(false) => return None,
                None => continue 'size,
            }
        }
        return Some((w, h));
    }
    None
}
