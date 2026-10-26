//! GX-4: the mode switch's state machine over the scanout (`scanout::Scanout`, generic over its register owner and
//! clock) and the screen's software side (`Follow`: console frame, fbdev, VT), host-tested in `tests::mode`.
//!
//! A switch: gates (reads), the plan written under the OTG lock, then it must take effect (no update pending
//! within 50 ms), read back, the OTG must keep counting frames and HUBP must not newly underflow (V1 G3-PLAN §3.4,
//! §3.5); only then the software side follows. Any failure puts the previous size back the same way (and that
//! must land too); a failed way back tries the boot (native) state as the last resort, and if that fails too the
//! switch is blocked until restart with the screen as it is (V1: "one image ... blocked until the next restart").
//! A new size stays only when it is confirmed within `CONFIRM_NS` (V1 F6: 15 s "keep? J"); `expire` rolls it
//! back otherwise. Going back to the last confirmed size needs no confirmation. Rollbacks are forced: they skip
//! the software side's "can follow now" check; when the software side cannot follow at once (a program holds
//! the screen), it is retried by `tick` until it can.

use crate::flip::{self, Rw};
use crate::mode::{self, Gate, PipeRegs, Plan, Size, Unoffered};
use crate::scanout::{Clock, LATE_NS, Scanout};
use crate::attach::Pipe;
use alloc::format;
use alloc::string::String;

/// The confirmation window (V1 G3-PLAN §3.5 step 8).
pub const CONFIRM_NS: u64 = 15_000_000_000;
/// Frames the OTG must count after the update took effect (the picture runs; underflow is read after them).
pub const WATCH_FRAMES: u32 = 3;
const POLL_NS: u64 = 1_000_000;

/// The screen's software side: the console's RAM frame, fbdev's geometry, the VT grid.
pub trait Follow {
    /// Err(reason) when the software side cannot take size `to` now (a program holds the screen...).
    fn check(&self, to: Size) -> Result<(), String>;
    /// Resizes the software side to `to`.
    fn resize(&self, to: Size) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// Not a mode this switch can show.
    Unoffered(Unoffered),
    /// The gates refused (nothing written).
    Gate(Gate),
    /// The software side cannot follow now (nothing written).
    Follow(String),
    /// The new size did not come up; the previous one is back.
    RolledBack(String),
    /// Neither the way back nor the boot state landed: one image, no switches until restart.
    Blocked(String),
    /// `confirm` without a switch waiting for it.
    NothingPending,
}

impl core::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SwitchError::Unoffered(u) => write!(f, "refused: {u}"),
            SwitchError::Gate(g) => write!(f, "refused, nothing written: {g}"),
            SwitchError::Follow(w) => write!(f, "refused, nothing written: {w}"),
            SwitchError::RolledBack(w) => write!(f, "rolled back: {w}"),
            SwitchError::Blocked(w) => write!(f, "blocked until restart: {w}"),
            SwitchError::NothingPending => write!(f, "no new mode is waiting for a confirmation"),
        }
    }
}

/// What a successful `set` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The size is shown already (nothing written).
    Unchanged,
    /// Back at the last confirmed size (no confirmation needed).
    Back,
    /// The new size shows; it stays only with `confirm` before `deadline_ns`. `serial` names this switch for the timer.
    Pending { serial: u64, deadline_ns: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    serial: u64,
    deadline_ns: u64,
}

/// The mode state of one scanout.
pub struct Switch {
    pub link: Size,
    pub link_mhz: u32,
    /// The GOP framebuffer's size: what the screen showed at boot, the largest size a switch can show.
    pub home: Size,
    boot: Option<PipeRegs>,
    shown: Size,
    confirmed: Size,
    pending: Option<Pending>,
    serial: u64,
    blocked: Option<String>,
    /// The software side still has to follow this size (a forced rollback it could not take at once).
    follow_due: Option<Size>,
}

/// A snapshot for the tool and the console command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub link: Size,
    pub link_mhz: u32,
    pub home: Size,
    pub shown: Size,
    pub confirmed: Size,
    /// Nanoseconds left to confirm (None = nothing waits).
    pub left_ns: Option<u64>,
    pub blocked: Option<String>,
    pub follow_due: Option<Size>,
}

impl Switch {
    /// The link's active area and rate as the firmware lit it, and the GOP framebuffer's size `home`, which the
    /// screen shows now.
    pub fn new(link: Size, link_mhz: u32, home: Size) -> Switch {
        Switch { link, link_mhz, home, boot: None, shown: home, confirmed: home, pending: None, serial: 0, blocked: None, follow_due: None }
    }

    pub fn status(&self, now_ns: u64) -> Status {
        Status {
            link: self.link,
            link_mhz: self.link_mhz,
            home: self.home,
            shown: self.shown,
            confirmed: self.confirmed,
            left_ns: self.pending.map(|p| p.deadline_ns.saturating_sub(now_ns)),
            blocked: self.blocked.clone(),
            follow_due: self.follow_due,
        }
    }

    /// The pending switch's generation and deadline.
    pub fn pending(&self) -> Option<(u64, u64)> {
        self.pending.map(|p| (p.serial, p.deadline_ns))
    }

    /// Shows `to` (already chosen from the offered modes). A second `set` while one waits replaces it; the way
    /// back stays the last confirmed size.
    pub fn set<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, f: &dyn Follow, to: Size) -> Result<Outcome, SwitchError> {
        if let Some(b) = &self.blocked {
            return Err(SwitchError::Blocked(b.clone()));
        }
        if !to.fits_in(self.home) {
            return Err(SwitchError::Unoffered(Unoffered::Larger { max: self.home }));
        }
        if to == self.shown && self.follow_due.is_none() {
            if to == self.confirmed {
                self.pending = None;
            }
            return Ok(Outcome::Unchanged);
        }
        let from = self.shown;
        klog::event!(Info, "dcn", "mode.set", from = alloc::format!("{from}"), to = alloc::format!("{to}"), link = alloc::format!("{}", self.link);
            "mode switch: new viewport, scaled to the link's timing");
        self.go(s, f, to, false)?;
        if to == self.confirmed {
            self.pending = None;
            klog::event!(Info, "dcn", "mode.back", size = alloc::format!("{to}"); "back at the last confirmed mode");
            return Ok(Outcome::Back);
        }
        self.serial += 1;
        let deadline_ns = s.clock().now_ns().saturating_add(CONFIRM_NS);
        self.pending = Some(Pending { serial: self.serial, deadline_ns });
        klog::event!(Info, "dcn", "mode.landed", size = alloc::format!("{to}"), serial = self.serial, confirm_ms = CONFIRM_NS / 1_000_000;
            "new mode shows; it goes back unless confirmed");
        Ok(Outcome::Pending { serial: self.serial, deadline_ns })
    }

    /// Keeps the new size.
    pub fn confirm(&mut self) -> Result<Size, SwitchError> {
        self.pending.take().ok_or(SwitchError::NothingPending)?;
        self.confirmed = self.shown;
        klog::event!(Info, "dcn", "mode.confirm", size = alloc::format!("{}", self.shown); "new mode kept");
        Ok(self.shown)
    }

    /// The timer: rolls switch `serial` back when it is still waiting and its deadline passed (None: nothing due).
    pub fn expire<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, f: &dyn Follow, serial: u64) -> Option<Result<Size, SwitchError>> {
        let p = self.pending?;
        if p.serial != serial || s.clock().now_ns() < p.deadline_ns {
            return None;
        }
        Some(self.roll_back(s, f, "timeout"))
    }

    /// Rolls a waiting switch back now (`mode undo`; also the timer's way).
    pub fn undo<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, f: &dyn Follow) -> Result<Size, SwitchError> {
        if self.pending.is_none() {
            return Err(SwitchError::NothingPending);
        }
        self.roll_back(s, f, "undo")
    }

    /// A gate refusing the way back (a flip pending, the lock held for a moment) keeps the switch due: the
    /// caller tries again (the timer thread every 200 ms), nothing was written.
    fn roll_back<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, f: &dyn Follow, why: &'static str) -> Result<Size, SwitchError> {
        let waiting = self.pending.take();
        let to = self.confirmed;
        klog::event!(Warn, "dcn", "mode.rollback", reason = why, from = alloc::format!("{}", self.shown), to = alloc::format!("{to}");
            "no confirmation: the last confirmed mode goes back");
        let r = self.go(s, f, to, true).map(|()| to);
        match &r {
            Err(SwitchError::Gate(_)) => self.pending = waiting,
            // The way back did not land and the unconfirmed size was written back (it still shows): the
            // rollback stays due at once, so the timer tries again instead of keeping the unconfirmed mode.
            Err(_) if self.blocked.is_none() && self.shown != self.confirmed => {
                let now = s.clock().now_ns();
                self.pending = waiting.map(|p| Pending { serial: p.serial, deadline_ns: p.deadline_ns.min(now) });
                klog::event!(Warn, "dcn", "mode.rollback_due", size = alloc::format!("{}", self.shown);
                    "the way back did not land; the unconfirmed mode still shows and the rollback is retried");
            }
            _ => {}
        }
        r
    }

    /// Retries a software side that could not follow a forced rollback at once. True when nothing is due.
    pub fn tick(&mut self, f: &dyn Follow) -> bool {
        let Some(to) = self.follow_due else { return true };
        match f.resize(to) {
            Ok(()) => {
                self.follow_due = None;
                klog::event!(Info, "dcn", "mode.followed", size = alloc::format!("{to}"); "the console and fbdev follow the rolled-back mode now");
                true
            }
            Err(_) => false,
        }
    }

    /// Hardware to `to`, then the software side; any failure goes back to `self.shown`.
    fn go<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, f: &dyn Follow, to: Size, forced: bool) -> Result<(), SwitchError> {
        let first = self.boot.is_none();
        // A flip still on its way lands first (bounded by the flip's own 50 ms), before the gates look at
        // FLIP_PENDING; a forced rollback waits once more for a flip ordered in between.
        settle_flip(s);
        let mut found = mode::gates(s.regs(), s.pipes(), self.link, self.home, first);
        if forced && found == Err(Gate::FlipPending) {
            settle_flip(s);
            found = mode::gates(s.regs(), s.pipes(), self.link, self.home, first);
        }
        let found = found.map_err(|g| {
            klog::event!(Warn, "dcn", "mode.refused", reason = alloc::format!("{g}"), forced = forced);
            SwitchError::Gate(g)
        })?;
        let p = s.pipes()[0];
        let boot = match self.boot {
            Some(b) => b,
            None => {
                // Kept before the first write: Quiesce puts exactly this back.
                s.keep_mode_boot(&found, self.home);
                self.boot = Some(found);
                found
            }
        };
        if !forced {
            f.check(to).map_err(SwitchError::Follow)?;
        }
        let uf = mode::underflow(s.regs(), &p).unwrap_or(0);
        let plan = mode::plan(&boot, p.hubp, self.home, to);
        if let Err(why) = s.with_pair(|r| mode::apply(r, &p, &plan)).map_err(|e| crate::scanout::reg_error_text(e)).and_then(|_| land(s, &p, &plan, Some(uf))) {
            return Err(self.hw_back(s, &p, &boot, why, forced));
        }
        if let Err(e) = f.resize(to) {
            if !forced {
                let why = format!("the console could not follow: {e}");
                let _ = f.resize(self.shown);
                return Err(self.hw_back(s, &p, &boot, why, false));
            }
            // A forced rollback keeps the hardware where it must be; the software side follows later.
            self.follow_due = Some(to);
            klog::event!(Warn, "dcn", "mode.follow_later", size = alloc::format!("{to}"), error = e.as_str();
                "the console or fbdev cannot follow now; retried");
        } else {
            self.follow_due = None;
        }
        s.set_size(to.w, to.h);
        self.shown = to;
        Ok(())
    }

    /// The hardware back to `self.shown`; the boot state as the last resort; blocked when neither lands.
    /// `forced` (a rollback to the confirmed size that did not land): the boot state first, when it is another
    /// size than the one that failed, then `self.shown` (the caller keeps the rollback due).
    fn hw_back<R: Rw, C: Clock>(&mut self, s: &Scanout<R, C>, p: &Pipe, boot: &PipeRegs, why: String, forced: bool) -> SwitchError {
        let write = |to: Size| -> Result<(), String> {
            let plan = mode::plan(boot, p.hubp, self.home, to);
            s.with_pair(|r| mode::apply(r, p, &plan)).map_err(crate::scanout::reg_error_text).and_then(|_| land(s, p, &plan, None))
        };
        let native_first = forced && self.confirmed != self.home && self.shown != self.home;
        let mut r = Err(String::new());
        if !native_first {
            r = write(self.shown);
            klog::event!(Warn, "dcn", "mode.undone", why = why.as_str(), back = alloc::format!("{}", self.shown), ok = r.is_ok();
                "the new mode did not come up: the previous one is written back");
            if r.is_ok() {
                return SwitchError::RolledBack(why);
            }
        }
        if self.shown != self.home {
            let n = write(self.home);
            if n.is_ok() {
                klog::event!(Warn, "dcn", "mode.native", why = why.as_str(); "the way back failed; the boot mode is back");
                s.set_size(self.home.w, self.home.h);
                self.shown = self.home;
                self.confirmed = self.home;
                self.pending = None;
                // The software side is at the size that did not come up: `tick` takes it to the boot size.
                self.follow_due = Some(self.home);
                return SwitchError::RolledBack(format!("{why}; the way back failed, the boot mode is back"));
            }
            if native_first {
                r = write(self.shown);
                klog::event!(Warn, "dcn", "mode.undone", why = why.as_str(), back = alloc::format!("{}", self.shown), ok = r.is_ok();
                    "neither the confirmed nor the boot mode came up: the unconfirmed one is written back");
                if r.is_ok() {
                    return SwitchError::RolledBack(why);
                }
            }
        }
        let text = format!("{why}; the way back failed too ({})", r.err().unwrap_or_default());
        klog::event!(Error, "dcn", "mode.blocked", why = text.as_str(); "one image: no mode switch until restart");
        self.pending = None;
        self.blocked = Some(text.clone());
        SwitchError::Blocked(text)
    }
}

/// Lets a flip still on its way land (`Scanout::wait`, bounded by the flip's own `LATE_NS`; a late flip falls back).
fn settle_flip<R: Rw, C: Clock>(s: &Scanout<R, C>) {
    let st = s.state();
    if st.ordered > st.landed {
        let _ = s.wait(st.ordered);
    }
}

/// What the confirmation timer does next (`Switch::wake`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Nothing due before this many nanoseconds.
    Sleep(u64),
    /// Switch `serial` is due for its rollback.
    Expire(u64),
    /// The software side still has to follow a rollback.
    Follow,
    /// Nothing waits: the timer ends.
    Done,
}

impl Switch {
    pub fn wake(&self, now_ns: u64) -> Wake {
        match (self.pending, self.follow_due) {
            (Some(p), _) if now_ns >= p.deadline_ns => Wake::Expire(p.serial),
            (Some(p), _) => Wake::Sleep(p.deadline_ns - now_ns),
            (None, Some(_)) => Wake::Follow,
            (None, None) => Wake::Done,
        }
    }
}

/// The first retry of a refused rollback or a software side that cannot follow yet comes after 200 ms, then
/// each wait doubles up to 5 s.
pub const RETRY_MIN_NS: u64 = 200_000_000;
pub const RETRY_MAX_NS: u64 = 5_000_000_000;
/// After this many tries in a row the timer says once that it is still trying.
pub const RETRY_SAY: u32 = 10;

pub fn backoff_ns(tries: u32) -> u64 {
    RETRY_MIN_NS.saturating_mul(1u64 << tries.saturating_sub(1).min(6)).min(RETRY_MAX_NS)
}

/// What the confirmation timer needs (the kernel: the `gx.mode` lock, `sched::sleep_ns`, the console; tests: fakes).
pub trait TimerHost {
    fn now_ns(&self) -> u64;
    fn sleep_ns(&self, ns: u64);
    /// The switch's next step (None: no switch here; the timer ends).
    fn wake(&self, now_ns: u64) -> Option<Wake>;
    fn expire(&self, serial: u64) -> Option<Result<Size, SwitchError>>;
    /// `Switch::tick`: true when the software side follows (or nothing is due).
    fn tick(&self) -> bool;
    /// A rollback landed (the console says so).
    fn rolled_back(&self, to: Size);
}

/// The one confirmation timer: sleeps until the waiting switch's deadline (a later `set` moves it, `confirm`
/// ends it), rolls it back, retries a rollback that did not land or a software side that cannot follow yet with
/// a growing wait, and ends when nothing waits. Every wait is bounded (B4).
pub fn run_timer(h: &dyn TimerHost) {
    let mut tries = 0u32;
    loop {
        let Some(w) = h.wake(h.now_ns()) else { return };
        match w {
            Wake::Done => return,
            Wake::Sleep(ns) => {
                tries = 0;
                h.sleep_ns(ns.saturating_add(1));
            }
            Wake::Expire(serial) => match h.expire(serial) {
                Some(Ok(to)) => {
                    tries = 0;
                    h.rolled_back(to);
                }
                Some(Err(e)) if h.wake(h.now_ns()) == Some(Wake::Expire(serial)) => {
                    tries = tries.saturating_add(1);
                    if tries == RETRY_SAY {
                        klog::event!(Warn, "dcn", "mode.rollback_retry", serial = serial, tries = tries, error = alloc::format!("{e}");
                            "the rollback still does not go through; the unconfirmed mode shows, retried every 5 s");
                    }
                    h.sleep_ns(backoff_ns(tries));
                }
                Some(Err(e)) => {
                    klog::event!(Error, "dcn", "mode.rollback_failed", serial = serial, error = alloc::format!("{e}"));
                }
                // Confirmed or replaced in between: look again (a short pause keeps two clocks from spinning).
                None => h.sleep_ns(1_000_000),
            },
            Wake::Follow => {
                if h.tick() {
                    tries = 0;
                } else {
                    tries = tries.saturating_add(1);
                    if tries == RETRY_SAY {
                        klog::event!(Warn, "dcn", "mode.follow_stuck", tries = tries;
                            "the console or fbdev still cannot follow the rolled-back mode (a program holds the screen?)");
                    }
                    h.sleep_ns(backoff_ns(tries));
                }
            }
        }
    }
}

/// The plan took effect: no update pending within `LATE_NS`, every write reads back, the OTG counts
/// `WATCH_FRAMES` frames and (when `uf_before` is known and was clear) HUBP did not start to underflow.
fn land<R: Rw, C: Clock>(s: &Scanout<R, C>, p: &Pipe, plan: &Plan, uf_before: Option<u32>) -> Result<(), String> {
    let r = s.regs();
    let c = s.clock();
    let t0 = c.now_ns();
    loop {
        match mode::pending(r, p) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return Err(crate::scanout::reg_error_text(e)),
        }
        if c.now_ns().saturating_sub(t0) >= LATE_NS {
            return Err(String::from("the update did not take effect in 50 ms"));
        }
        c.sleep_ns(POLL_NS);
    }
    mode::verify(r, plan).map_err(|b| format!("register {b:#07x} read back different"))?;
    let k = u32::from(p.otg);
    let f0 = flip::frame_count(r, k).map_err(crate::scanout::reg_error_text)?;
    let t1 = c.now_ns();
    let period = s.timing().period_ns().max(1);
    let limit = period.saturating_mul(u64::from(WATCH_FRAMES) + 2).saturating_add(LATE_NS);
    loop {
        let f = flip::frame_count(r, k).map_err(crate::scanout::reg_error_text)?;
        if f.wrapping_sub(f0) & 0xFF_FFFF >= WATCH_FRAMES {
            break;
        }
        if c.now_ns().saturating_sub(t1) >= limit {
            return Err(String::from("the OTG stopped counting frames"));
        }
        c.sleep_ns(period.min(POLL_NS * 4));
    }
    if uf_before == Some(0) {
        let u = mode::underflow(r, p).map_err(crate::scanout::reg_error_text)?;
        if u != 0 {
            return Err(format!("HUBP underflow ({u:#x}) after the switch"));
        }
    }
    Ok(())
}
