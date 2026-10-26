//! GX-4 kernel glue: the one mode switch of the attached scanout (`switch::Switch` over `live::KScanout`), the
//! 15 s confirmation timer (one `Normal` thread while a switch waits or a rollback is due: bounded sleeps, B4;
//! `switch::run_timer`), the list of modes (EDID from the BootInfo, else the GOP's mode list) and the screen's software side,
//! which the kernel installs (`set_follower`: console frame, fbdev geometry, VT grid live in DP's and IN's crates;
//! GX-4 change request 1). Used by the MCP tool `display_mode` and the console command `mode`.

use crate::live::{self, KScanout};
use crate::mode::{self, Offered, Request, Size, Source};
use crate::switch::{Follow, Outcome, Switch, SwitchError, TimerHost, Wake, run_timer};
use core::sync::atomic::{AtomicBool, Ordering};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use console::Console;
use ksync::{Mutex, OnceRef};

/// The screen's software side as the kernel wires it (change request 1): `check(w, h)` says whether it can
/// take that size now, `resize(w, h)` resizes the console's RAM frame, fbdev's geometry and the VT grid.
pub struct Follower {
    pub check: fn(u32, u32) -> Result<(), String>,
    pub resize: fn(u32, u32) -> Result<(), String>,
}

static FOLLOWER: OnceRef<Follower> = OnceRef::new();

/// Installs the software side (once, at boot). False if one was installed already.
pub fn set_follower(f: &'static Follower) -> bool {
    FOLLOWER.set(f)
}

/// The installed software side (ktests).
pub fn follower() -> Option<&'static Follower> {
    FOLLOWER.get()
}

struct Hook;

const NO_FOLLOWER: &str = "the console cannot follow a new size here (no follower installed: GX-4 change request 1)";

impl Follow for Hook {
    fn check(&self, to: Size) -> Result<(), String> {
        let f = FOLLOWER.get().ok_or_else(|| String::from(NO_FOLLOWER))?;
        (f.check)(to.w, to.h)
    }
    fn resize(&self, to: Size) -> Result<(), String> {
        let f = FOLLOWER.get().ok_or_else(|| String::from(NO_FOLLOWER))?;
        (f.resize)(to.w, to.h)
    }
}

/// One switch at a time; the timer thread and the callers take it (it may sleep: Mutex, never a spin lock).
static SWITCH: Mutex<Option<Switch>> = Mutex::named("gx.mode", None);

const NO_ENGINE: &str = "no display engine here (QEMU has no AMD display engine; the GOP mode stays)";

fn scanout() -> Result<&'static KScanout, String> {
    live::attach().ok_or_else(|| String::from(NO_ENGINE))
}

/// The link's active area and rate (KMS view of the pipe's OTG), else the scanout's own size and timing; the
/// home size is the scanout's at the first use (no switch has run before: this is the only maker).
fn new_switch(s: &KScanout) -> Switch {
    let k = s.otg();
    let crtc = crate::kms::read(s.regs(), false).crtcs.into_iter().find(|c| u32::from(c.id) == k && c.enabled);
    let (w, h) = s.size();
    let home = Size::new(w, h);
    match crtc {
        Some(c) if c.h_active > 0 && c.v_active > 0 => {
            Switch::new(Size::new(c.h_active, c.v_active), c.refresh_mhz.unwrap_or(s.timing().refresh_mhz()), home)
        }
        _ => Switch::new(home, s.timing().refresh_mhz(), home),
    }
}

fn with_switch<T>(f: impl FnOnce(&KScanout, &mut Switch) -> T) -> Result<T, String> {
    let s = scanout()?;
    let mut g = SWITCH.lock();
    let sw = g.get_or_insert_with(|| new_switch(s));
    Ok(f(s, sw))
}

/// The "no EDID" event is said once per boot.
static LIST_SAID: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The modes on offer and where they came from (EDID tag 21, else the GOP's MODE_LIST, else `home` alone).
pub fn offered(home: Size) -> (Vec<Offered>, Source) {
    let (v, src, _) = offered_with_header(home);
    (v, src)
}

/// `offered` plus the list's header line, both from one read of the BootInfo's EDID (so they cannot disagree).
fn offered_with_header(home: Size) -> (Vec<Offered>, Source, String) {
    let bi = kexec::boot::boot_info().map(|(b, _)| b);
    let edid = bi.as_ref().and_then(|b| b.edid());
    let gop: Vec<Size> = bi.as_ref().and_then(|b| b.mode_list()).map(|l| l.iter().map(|m| Size::new(m.width, m.height)).collect()).unwrap_or_default();
    let (v, src) = mode::offered(edid, &gop, home);
    if src != Source::Edid && !LIST_SAID.swap(true, core::sync::atomic::Ordering::AcqRel) {
        klog::event!(Info, "dcn", "mode.list", source = src.name(), modes = v.len(), edid = edid.is_some(),
            why = mode::edid_note(edid); "no EDID timings: the mode list is the GOP's (sizes only)");
    }
    let header = mode::list_header(src, edid);
    (v, src, header)
}

/// The tool's and the command's text: state and the modes (each marked when the switch can show it).
pub fn text() -> Result<String, String> {
    tick();
    with_switch(|_, sw| {
        let st = sw.status(time::now_ns());
        let (list, _, header) = offered_with_header(st.home);
        let mut out = format!(
            "link {} @ {} Hz (firmware timing, kept); framebuffer {}; shows {}; confirmed {}",
            st.link,
            mode::rate_text(st.link_mhz),
            st.home,
            st.shown,
            st.confirmed
        );
        if let Some(ns) = st.left_ns {
            out += &format!("; waiting for a confirmation, {} s left", ns.div_ceil(1_000_000_000));
        }
        if let Some(b) = &st.blocked {
            out += &format!("; BLOCKED: {b}");
        }
        if let Some(d) = st.follow_due {
            out += &format!("; console still to follow {d}");
        }
        out += &format!("\n{header}\n");
        for o in &list {
            let rates: Vec<String> = o.rates_mhz.iter().map(|&r| mode::rate_text(r)).collect();
            let ok = mode::choose(&Request { size: o.size, refresh_mhz: None }, &list, st.home, st.link_mhz).is_ok();
            out += &format!(
                "  {}{}{}{}\n",
                o.size,
                if rates.is_empty() { String::new() } else { format!(" @ {}", rates.join("/")) },
                if o.size == st.shown { " (shown)" } else { "" },
                if ok { "" } else { " (larger than the framebuffer or too small)" }
            );
        }
        out
    })
}

/// Switches to `req` ("WxH" or "WxH@R"). The answer names the deadline to confirm.
pub fn set(req: &str) -> Result<String, String> {
    let r = mode::parse(req)?;
    tick();
    let res = with_switch(|s, sw| {
        let (list, _) = offered(sw.home);
        let to = mode::choose(&r, &list, sw.home, sw.link_mhz).map_err(SwitchError::Unoffered)?;
        let o = sw.set(s, &Hook, to)?;
        Ok::<(Outcome, Size, Size, u32), SwitchError>((o, to, sw.link, sw.link_mhz))
    })?;
    arm_if_due();
    match res {
        Ok((Outcome::Unchanged, to, ..)) => Ok(format!("mode {to}: shown already")),
        Ok((Outcome::Back, to, ..)) => Ok(format!("mode {to}: back at the confirmed mode")),
        Ok((Outcome::Pending { .. }, to, link, mhz)) => {
            Ok(format!(
                "mode {to} (scaled to {link} @ {} Hz): keep it with `mode keep` (MCP display_mode {{\"confirm\":true}}) within {} s, else it goes back",
                mode::rate_text(mhz),
                crate::switch::CONFIRM_NS / 1_000_000_000
            ))
        }
        Err(e) => Err(format!("mode {}: {e}", r.size)),
    }
}

/// Keeps the new mode.
pub fn confirm() -> Result<String, String> {
    with_switch(|_, sw| sw.confirm())?.map(|s| format!("mode {s} kept")).map_err(|e| format!("{e}"))
}

/// Rolls a waiting switch back now.
pub fn undo() -> Result<String, String> {
    let r = with_switch(|s, sw| sw.undo(s, &Hook))?;
    arm_if_due();
    r.map(|s| format!("mode {s} back")).map_err(|e| format!("{e}"))
}

/// Starts the timer when a switch waits, a rollback is due or the software side still has to follow.
fn arm_if_due() {
    if !matches!(KTimer.wake(time::now_ns()), None | Some(Wake::Done)) {
        arm();
    }
}

/// Retries a software side that could not follow a rollback at once (each call, and the timer).
fn tick() -> bool {
    match scanout() {
        Ok(_) => SWITCH.lock().as_mut().is_none_or(|sw| sw.tick(&Hook)),
        Err(_) => true,
    }
}

/// The kernel's side of the confirmation timer (`switch::run_timer`).
struct KTimer;

impl TimerHost for KTimer {
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn sleep_ns(&self, ns: u64) {
        sched::sleep_ns(ns);
    }
    fn wake(&self, now_ns: u64) -> Option<Wake> {
        with_switch(|_, sw| sw.wake(now_ns)).ok()
    }
    fn expire(&self, serial: u64) -> Option<Result<Size, SwitchError>> {
        with_switch(|s, sw| sw.expire(s, &Hook, serial)).ok().flatten()
    }
    fn tick(&self) -> bool {
        tick()
    }
    fn rolled_back(&self, to: Size) {
        console::console().write_str(&format!("\nmode {to}: no confirmation, the previous mode is back\n"), console::FG);
    }
}

/// True while the one timer thread runs; it takes every later deadline itself (a second `set` moves the
/// deadline, `confirm` ends the wait), so there is never more than one thread.
static TIMER: AtomicBool = AtomicBool::new(false);

/// Makes sure the timer thread runs for a waiting switch.
fn arm() {
    if TIMER.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = sched::spawn("gx-mode", sched::Class::Normal, || {
        loop {
            run_timer(&KTimer);
            TIMER.store(false, Ordering::Release);
            // A switch armed between the timer's last look and the store above found TIMER still set.
            let due = !matches!(KTimer.wake(time::now_ns()), None | Some(Wake::Done));
            if !due || TIMER.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    });
    if let Err(e) = spawned {
        TIMER.store(false, Ordering::Release);
        // No timer means no automatic way back: roll back at once rather than keep an unconfirmed mode.
        klog::event!(Error, "dcn", "mode.timer_failed", error = alloc::format!("{e:?}"); "no timer thread: the new mode goes back now");
        let _ = undo();
    }
}

/// What the console command `mode` does with its arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd<'a> {
    List,
    Keep,
    Undo,
    Set(&'a str),
}

pub const USAGE: &str = "usage: mode | mode WxH[@R] | mode keep | mode undo";

/// Parses the command's arguments (the size itself is parsed by `set`).
pub fn cmd<'a>(args: &[&'a str]) -> Result<Cmd<'a>, &'static str> {
    match args {
        [] | ["list"] => Ok(Cmd::List),
        ["keep" | "yes" | "ja" | "j" | "confirm"] => Ok(Cmd::Keep),
        ["undo" | "back"] => Ok(Cmd::Undo),
        [m] => Ok(Cmd::Set(m)),
        _ => Err(USAGE),
    }
}

fn mode_cmd(c: &mut console::monitor::Ctx) -> Result<(), String> {
    let args: Vec<&str> = c.args.clone();
    let out = match cmd(&args).map_err(String::from)? {
        Cmd::List => text()?,
        Cmd::Keep => confirm()?,
        Cmd::Undo => undo()?,
        Cmd::Set(m) => set(m)?,
    };
    c.println(&out);
    Ok(())
}
console::command!(name: "mode", args: "[WxH[@R] | keep | undo]", help: "screen mode: list, switch (scaled, kept only with `mode keep` within 15 s), keep, undo", physical_only: false, run: mode_cmd);
