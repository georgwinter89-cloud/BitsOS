//! Wave 44: the taker of the card's thermal interrupts, Linux's `smu_v14_0_irq_process`
//! (`S:` ../pm/swsmu/smu14/smu_v14_0.c:807-882) and the software CTF check it schedules
//! (`smu_swctf_delayed_work_handler`, ../pm/swsmu/amdgpu_smu.c:1303-1330). The IH reader hands it exactly the
//! three sources Linux registers (S:891-922, `thm14::Source`), in Linux's order of handling:
//!
//! - THM source 0, the temperature rose above the upper threshold: the check is scheduled 50 ms ahead
//!   (`schedule_delayed_work`, which leaves a pending check where it is). Linux touches no register here.
//! - THM source 1, it fell below the lower threshold: Linux's line, nothing else.
//! - MP1 source 0xFE: the acknowledge first, then by the context id in `src_data[0]`: FAN_ABNORMAL lowers the
//!   upper threshold to the table's software shutdown temperature plus its offset, FAN_RECOVERY puts it back;
//!   every other id was acknowledged and is only counted.
//!
//! The check, when due: the hotspot temperature read as Linux reads it (one metrics transfer, `thermalhw`); when
//! the table names no limit, the read fails, or the hotspot is at or above the limit, the machine is switched off
//! in order (Linux's `orderly_poweroff(true)`). The decision is made under the session's lock; the power-off
//! itself is the caller's, after the lock is dropped (`take_power_off`, `OffWatch`).
//!
//! The registers (the acknowledge) are known from the stage `pmfw` on, also when its start stopped: Linux's
//! handler acknowledges MP1 whenever its interrupt comes. The table transfer waits for a start that passed (the
//! item's rule: a sensor read is the one message after Linux's start); a check before it reads nothing and
//! switches off, which Linux would not do without its read (section 7, `thm-shutdown`).
//!
//! The register part needs a reader that reaches the card (`ihring::IhIo::thermal_hw`): the IH thread and the
//! tests' model do, the stage `sdmatest`'s own reader does not (its guard lists no thermal register). There an
//! MP1 entry's register part waits in order in `queue` and a due check waits too; both run at the next pass of a
//! reader that reaches the card (plan/spec/gpu-rdna4.md section 7, `thm-stage-delay`). Nothing is dropped
//! silently: a full queue counts what it lost and says so.

use super::regs14::Regs14;
use super::thermalhw;
use crate::gpucore::psp::Hw;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;
use rdna4_core::ih7::Iv;
use rdna4_core::metrics14::Temps;
use rdna4_core::smu14::ThermalRange;
use rdna4_core::thm14::{self, Source};

pub use super::thermallook::Look;

/// A bound against an interrupt storm, not a count of things: MP1 entries whose register part waits for a
/// reader that reaches the card. Past it an entry's acknowledge is lost, counted and said.
const QUEUE_BOUND: usize = 64;

/// Where the sensors' read finds its table: the driver table (a VRAM offset), once the power controller's start
/// passed (a table transfer before it is no message of Linux's start).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sensors {
    pub driver_at: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Thermal {
    /// Linux's `smu->thermal_range` of the PMFW that runs now, from its table (`pmfw`).
    pub range: Option<ThermalRange>,
    /// The stage `pmfw` wrote all four registers of the thermal alert.
    pub alert_armed: bool,
    /// The card's registers, from the stage `pmfw` on (the acknowledge needs no passed start).
    pub regs: Option<Regs14>,
    pub sensors: Option<Sensors>,
    /// Entries per source, `thm14::Source`'s order.
    pub count: [u64; 3],
    /// MP1 entries per context id.
    pub contexts: BTreeMap<u32, u64>,
    /// THM or MP1 entries of a source Linux's handler does not register, per (client, source).
    pub foreign: BTreeMap<(u8, u8), u64>,
    /// When the software CTF check is due, on the reader's clock (us).
    pub due_us: Option<u64>,
    /// Checks that came due while the reader could not reach the card and ran later.
    pub late: u64,
    /// The pending check came due under a reader that does not reach the card.
    pub waited: bool,
    /// MP1 entries (their context ids) whose acknowledge waits for a reader that reaches the card.
    pub queue: VecDeque<u32>,
    pub lost: u64,
    /// The upper threshold the handler last wrote (degrees).
    pub high_now: Option<u32>,
    /// The last temperatures read, and when (us).
    pub last: Option<(Temps, u64)>,
    pub reads: u64,
    pub read_failed: Option<String>,
    pub checks: u64,
    /// The last check's result, in words.
    pub outcome: Option<String>,
    /// A power-off the last check decided: the caller runs it once the session's lock is dropped.
    pub power_off: Option<String>,
    pub offs: u64,
    /// A power-off that did not happen, and how often it was tried again (`OffWatch`).
    pub off_failed: Option<String>,
    pub off_tries: u64,
    /// Accesses the handler's gate refused or the card did not take.
    pub failed: u64,
    /// Checks and acknowledges dropped by a load of the PMFW's firmware (Linux's cancel_delayed_work_sync).
    pub cancelled: u64,
}

fn idx(s: Source) -> usize {
    match s {
        Source::High => 0,
        Source::Low => 1,
        Source::Smu => 2,
    }
}

impl Thermal {
    /// What the stage `pmfw` learned: the range (also of a start that stopped after the table), whether the
    /// alert's four registers went, the card's registers, and the driver table once the start passed.
    pub fn arm(&mut self, range: Option<ThermalRange>, alert: bool, regs: Regs14, sensors: Option<Sensors>) {
        (self.range, self.alert_armed, self.regs, self.sensors) = (range, alert, Some(regs), sensors);
    }

    /// The stage `fw` loads the PMFW's firmware again: a pending check is dropped as Linux's hw_fini drops it
    /// (amdgpu_smu.c:2159), and what waited for the old PMFW with it. The counts stay for `state`.
    pub fn reloaded(&mut self) {
        self.cancelled += u64::from(self.due_us.is_some()) + self.queue.len() as u64;
        (self.range, self.alert_armed, self.regs, self.sensors, self.due_us, self.waited) = (None, false, None, None, None, false);
        self.queue.clear();
    }

    /// The upper threshold the table asks for, with the offset Linux adds for FAN_ABNORMAL.
    fn fan_high(&self, ctx: u32) -> Option<u32> {
        let r = self.range?;
        match ctx {
            thm14::CTX_FAN_ABNORMAL => Some(thm14::high_fan_abnormal(r.software_shutdown_temp, r.software_shutdown_temp_offset)),
            thm14::CTX_FAN_RECOVERY => Some(thm14::high_fan_recovery(r.software_shutdown_temp)),
            _ => None,
        }
    }

    /// An entry of one of the three sources.
    pub fn take(&mut self, hw: Option<&mut dyn Hw>, src: Source, iv: &Iv, now_us: u64) {
        let n = &mut self.count[idx(src)];
        *n += 1;
        let n = *n;
        match src {
            Source::High => {
                let limit = self.range.map_or(0, |r| r.software_shutdown_temp);
                if self.due_us.is_none() {
                    self.due_us = Some(now_us + thm14::SWCTF_DELAY_MS * 1000);
                    klog::event!(Error, "gx", "rdna4.thermal.high", entries = n, limit = limit;
                        "the card's temperature rose above its upper threshold (the table's software shutdown temperature): BitsOS reads the hotspot temperature in 50 ms, as Linux does, and switches the PC off if it is still that hot");
                }
                if self.range.is_none() {
                    klog::event!(Error, "gx", "rdna4.thermal.no_range", source = "high";
                        "a thermal alert before the power controller's start named the limits in this boot: the check will find no limit and switch the PC off, as Linux's would with a range of zeros");
                }
            }
            // Linux's `dev_emerg` at every entry (S:821-822), no limit.
            Source::Low => klog::event!(Warn, "gx", "rdna4.thermal.low", entries = n; "ERROR: GPU under temperature range detected (Linux's words: the card's temperature fell below its lower threshold, 0 degrees on this card; nothing is done)"),
            Source::Smu => {
                let ctx = iv.src_data[0];
                *self.contexts.entry(ctx).or_insert(0) += 1;
                match hw {
                    Some(hw) => {
                        self.replay(hw);
                        self.acknowledge(hw, ctx);
                    }
                    None if self.queue.len() < QUEUE_BOUND => self.queue.push_back(ctx),
                    None => {
                        self.lost += 1;
                        if self.lost == 1 {
                            klog::event!(Error, "gx", "rdna4.thermal.lost", bound = QUEUE_BOUND;
                                "more power controller interrupts waited for their acknowledge than the bound holds: this one is counted and not acknowledged");
                        }
                    }
                }
            }
        }
    }

    /// A THM or MP1 entry of a source Linux's handler does not take: counted and said once per pair.
    pub fn foreign(&mut self, iv: &Iv) {
        let n = self.foreign.entry((iv.client_id, iv.src_id)).or_insert(0);
        *n += 1;
        if *n == 1 {
            klog::event!(Error, "gx", "rdna4.thermal.foreign", client = iv.client_id, source = iv.src_id, data = klog::Hex(u64::from(iv.src_data[0]));
                "an interrupt of the card's thermal block or power controller from a source Linux's handler does not register (counted from here on, logged this once)");
        }
    }

    /// The acknowledges that waited, in order.
    pub fn replay(&mut self, hw: &mut dyn Hw) {
        while let Some(ctx) = self.queue.pop_front() {
            self.acknowledge(hw, ctx);
        }
    }

    fn acknowledge(&mut self, hw: &mut dyn Hw, ctx: u32) {
        let Some(regs) = self.regs.as_ref() else {
            self.failed += 1;
            klog::event!(Error, "gx", "rdna4.thermal.unreached", context = ctx;
                "a power controller interrupt before the stage pmfw ran in this boot, or after a load of its firmware: the driver does not know the card's registers for it, nothing is acknowledged");
            return;
        };
        let high = self.fan_high(ctx);
        match thermalhw::acknowledge(hw, regs, ctx, high) {
            Ok(()) => {
                if high.is_some() {
                    self.high_now = high;
                }
            }
            Err(why) => {
                self.failed += 1;
                klog::event!(Error, "gx", "rdna4.thermal.ack_failed", context = ctx, why = why.as_str(); "the acknowledge of a power controller interrupt did not go through");
                return;
            }
        }
        let offset = self.range.map_or(0, |r| r.software_shutdown_temp_offset);
        match ctx {
            thm14::CTX_FAN_ABNORMAL => klog::event!(Error, "gx", "rdna4.thermal.fan_abnormal", limit = high.unwrap_or(0), offset = offset;
                "Reduce soft CTF limit (Linux's words): the power controller reports a fan fault; the card's upper threshold is now the software shutdown temperature plus the table's offset"),
            thm14::CTX_FAN_RECOVERY => klog::event!(Warn, "gx", "rdna4.thermal.fan_recovery", limit = high.unwrap_or(0);
                "Recover soft CTF limit (Linux's words): the power controller reports the fan works again; the upper threshold is the software shutdown temperature again"),
            other => klog::event!(Info, "gx", "rdna4.thermal.context", context = other, name = thm14::context_name(other);
                "a power controller interrupt Linux acknowledges and does nothing else with"),
        }
    }

    /// How long until the check is due (`None`: none pending).
    pub fn due_in_us(&self, now_us: u64) -> Option<u64> {
        self.due_us.map(|d| d.saturating_sub(now_us))
    }

    /// The software CTF check, once it is due and a reader reaches the card.
    pub fn check(&mut self, hw: Option<&mut dyn Hw>, now_us: u64) {
        if !self.due_us.is_some_and(|d| now_us >= d) {
            return;
        }
        let Some(hw) = hw else {
            self.waited = true;
            return;
        };
        self.replay(hw);
        self.late += u64::from(core::mem::take(&mut self.waited));
        (self.due_us, self.checks) = (None, self.checks + 1);
        let limit = self.range.map_or(0, |r| r.software_shutdown_temp);
        // Linux reads the sensor only when the table names a limit (amdgpu_smu.c:1318-1323).
        let read = if limit == 0 { Err(String::from("the table names no software shutdown temperature")) } else { self.read(hw, now_us) };
        let hotspot = read.as_ref().ok().map(Temps::hotspot_milli);
        if !thm14::swctf_powers_off(limit, hotspot) {
            self.outcome = Some(format!("hotspot {} C below the software shutdown temperature {limit} C after 50 ms: nothing to do (Linux's check)", hotspot.unwrap_or(0) / 1000));
            klog::event!(Warn, "gx", "rdna4.thermal.ok", hotspot = hotspot.unwrap_or(0) / 1000, limit = limit; "the card cooled below its software shutdown temperature within 50 ms: nothing is done");
            return;
        }
        let why = match (&read, hotspot) {
            (_, Some(h)) => format!("hotspot {} C at or above the software shutdown temperature {limit} C after 50 ms", h / 1000),
            (Err(e), _) => format!("the hotspot temperature could not be read after 50 ms ({e})"),
            _ => String::from("no hotspot temperature"),
        };
        klog::event!(Error, "gx", "rdna4.thermal.swctf", why = why.as_str(); "ERROR: GPU over temperature range(SW CTF) detected!");
        klog::event!(Error, "gx", "rdna4.thermal.shutdown"; "ERROR: System is going to shutdown due to GPU SW CTF! (Linux's words: BitsOS writes the files back, stops the card's engines and switches the PC off)");
        self.outcome = Some(format!("{why}: the PC is switched off in order"));
        (self.power_off, self.offs) = (Some(why), self.offs + 1);
    }

    /// One read of the temperatures through the handler's gate (`thermalhw::read_temps`).
    pub fn read(&mut self, hw: &mut dyn Hw, now_us: u64) -> Result<Temps, String> {
        let (Some(regs), Some(s)) = (self.regs.as_ref(), self.sensors) else {
            let e = String::from("the power controller's start did not pass in this boot: BitsOS sends no table transfer then, where Linux would read");
            self.read_failed = Some(e.clone());
            return Err(e);
        };
        self.reads += 1;
        match thermalhw::read_temps(hw, regs, s.driver_at) {
            Ok(t) => {
                (self.last, self.read_failed) = (Some((t, now_us)), None);
                Ok(t)
            }
            Err(e) => {
                self.read_failed = Some(e.clone());
                Err(e)
            }
        }
    }

    /// The power-off the last check decided, taken once.
    pub fn take_power_off(&mut self) -> Option<String> {
        self.power_off.take()
    }

    /// What the card's own table names as limits, in degrees: (edge, hotspot, memory, software shutdown).
    pub fn limits(&self) -> Option<(i32, i32, i32, u16)> {
        self.range.map(|r| (r.max / 1000, r.hotspot_crit_max / 1000, r.mem_crit_max / 1000, r.software_shutdown_temp))
    }
}

/// The wait of a reader that wakes at least every `idle_ns`: until the check is due when one is pending.
pub fn wait_ns(idle_ns: u64, due_in_us: Option<u64>) -> u64 {
    due_in_us.map_or(idle_ns, |us| idle_ns.min(us.saturating_mul(1000)))
}

/// What switches the machine off: the platform's orderly power-off in the kernel, a recorder in the tests.
pub trait PowerOff {
    /// Returns only when the machine did not switch off, with why.
    fn power_off(&mut self, why: &str) -> String;
}

/// How long the IH thread waits before it tries a power-off again that did not happen.
pub const OFF_RETRY_NS: u64 = 10_000_000_000;

/// The IH thread's part of the critical path, run with the session's lock dropped. A power-off that did not
/// happen has run the drivers' quiesce hooks already, the card's IH among them: no alert reaches the taker from
/// then on, so the thread tries the power-off again on its own clock, every `OFF_RETRY_NS`, until it goes.
#[derive(Debug, Default)]
pub struct OffWatch {
    /// Why the PC must go off, once a power-off did not happen.
    pub failed: Option<String>,
    pub tries: u64,
}

impl OffWatch {
    /// The power-off the last check decided (`take` takes it under the session's lock and drops the lock when it
    /// returns), or the one that did not happen, again. Its failure in plain words, for the taker's `state`;
    /// `None` when there was nothing to do.
    pub fn after(&mut self, take: impl FnOnce() -> Option<String>, off: &mut dyn PowerOff) -> Option<String> {
        let why = match self.failed.take() {
            Some(w) => {
                self.tries += 1;
                klog::event!(Error, "gx", "rdna4.thermal.poweroff_retry", tries = self.tries, why = w.as_str();
                    "the PC is still on after a power-off for the card's temperature: BitsOS tries again every 10 s until it goes off; hold the power button");
                w
            }
            None => take()?,
        };
        let failed = off.power_off(&why);
        klog::event!(Error, "gx", "rdna4.thermal.poweroff_failed", why = why.as_str(), error = failed.as_str();
            "the power-off for the card's temperature did not happen: hold the power button; the card's engines and its IH are stopped, its power controller still runs, and no thermal alert reaches BitsOS any more");
        let text = format!("power-off did not happen ({failed}; tried again {} times, every 10 s): hold the power button; the card's engines and its IH are stopped, its power controller still runs, and no thermal alert reaches BitsOS any more", self.tries);
        self.failed = Some(why);
        Some(text)
    }
}

/// What the stage `pmfw` hands the taker at the end of its call: the range its table gave (also when it
/// stopped after the table), whether the alert's four registers went, the card's registers, and the driver
/// table once the start passed.
pub fn arm(s: &mut super::session::Session, r: &Regs14, passed: bool) {
    let sensors = match (passed, s.pmfw.tables.as_ref()) {
        (true, Some((_, driver))) => Some(Sensors { driver_at: driver.start }),
        _ => None,
    };
    let (range, alert) = (s.pmfw.facts.thermal, s.pmfw.facts.alert_written == 4);
    if let Some(ih) = s.nb.ih.as_mut() {
        ih.reader.thermal.arm(range, alert, r.clone(), sensors);
    }
}
