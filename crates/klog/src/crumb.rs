//! Breadcrumbs (RB-16): how far the last shutdown, Reload or reboot got, in places that outlive the machine's
//! RAM. The Mini-PC's reset and its sync flood clear RAM (rb-RB-13, rb-RB-14), so the crash page cannot say
//! which device's quiesce or which step a flood interrupted. Each step and each device's quiesce leaves a crumb
//! *before* it runs; the crumb goes to two stores:
//!
//! - **wire** (klogd's console mirror): one screen line `crumb <seq> <stage> <what>` through the route card's
//!   `poll_tx`, the card asked until it has sent it, so the listener has it before the step that may flood.
//!   After the mirror's end datagram the lines go under a fresh stream id (the listener shows "its start was
//!   missed" once). It dies with the route card's own quiesce, which `devmodel` therefore runs last but the
//!   watchdog's.
//! - **keep** (the FCH watchdog's count register, fch-wdt): survives the platform reset (desk 28.10.: the count
//!   a panic left read 29 at the next start); the next start decodes it and names the step or device
//!   (`watchdog` tool, boot log at warn after a flood).
//!
//! Rules: [`leave`] is lock-free and silent (no klog, no allocation, no waiting lock): it runs in Quiesce with
//! interrupts off and the other CPUs parked. A store that is busy or absent is skipped. Registration happens
//! once per store in thread context. CMOS and the FCH's BIOS RAM are not used: both belong to the firmware and
//! no byte is documented free (RB-16 notes).

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use ksync::OnceRef;

/// Where a crumb was left. The numbers are part of the keep store's code (fch-wdt `crumb`): never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Stage {
    /// The console mirror paused (klog::shutdown step 1).
    Paused = 1,
    /// The storage stop begins.
    Storage = 2,
    /// The thread-context device stops before the park (`devmodel::stop_all_before_park`).
    Prepare = 3,
    /// The other CPUs are being parked.
    Parked = 4,
    /// `quiesce_all` begins.
    Quiesce = 5,
    /// One device's Quiesce begins (`what` = its name).
    Device = 6,
    /// Every device's Quiesce returned.
    Quiesced = 7,
    /// The reset write comes next.
    Reset = 8,
    /// Reload: the other CPUs are stopped, the quiesce follows.
    ReloadStopped = 9,
    /// Reload: the bus-master sweep after the quiesce.
    ReloadSweep = 10,
    /// Reload: the jump into the new kernel comes next.
    ReloadJump = 11,
    /// A ktest's crumb (`rb.crumb.*`).
    Test = 12,
    /// RB-17: the reset writes, one crumb right before each (acpi's `reboot`): the FADT reset register.
    ResetFadt = 13,
    /// 0xCF9 with the hard reset code (6).
    ResetCf9Hard = 14,
    /// 0xCF9 with the full reset code (0xE).
    ResetCf9Full = 15,
    /// The 8042's reset pulse (the triple fault follows it without a crumb of its own). Past the keep store's
    /// device ids (16..47): its table appends, it never renumbers.
    ResetKbc = 48,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Paused => "paused",
            Stage::Storage => "storage",
            Stage::Prepare => "prepare",
            Stage::Parked => "parked",
            Stage::Quiesce => "quiesce",
            Stage::Device => "device",
            Stage::Quiesced => "quiesced",
            Stage::Reset => "reset",
            Stage::ReloadStopped => "reload_stopped",
            Stage::ReloadSweep => "reload_sweep",
            Stage::ReloadJump => "reload_jump",
            Stage::Test => "test",
            Stage::ResetFadt => "reset_fadt",
            Stage::ResetCf9Hard => "reset_cf9_hard",
            Stage::ResetCf9Full => "reset_cf9_full",
            Stage::ResetKbc => "reset_kbc",
        }
    }

    /// RB-17: a reset write (or the generic `Reset` mark before them) comes next: a shutdown whose last crumb is
    /// one of these reached its reset. Together with `ReloadJump` these end a shutdown.
    pub fn is_reset(self) -> bool {
        matches!(self, Stage::Reset | Stage::ResetFadt | Stage::ResetCf9Hard | Stage::ResetCf9Full | Stage::ResetKbc)
    }

    /// The shutdown reached its end: a reset write or the Reload's jump came next.
    pub fn ends_a_shutdown(self) -> bool {
        self.is_reset() || self == Stage::ReloadJump
    }

    pub fn from_u8(v: u8) -> Option<Stage> {
        Some(match v {
            1 => Stage::Paused,
            2 => Stage::Storage,
            3 => Stage::Prepare,
            4 => Stage::Parked,
            5 => Stage::Quiesce,
            6 => Stage::Device,
            7 => Stage::Quiesced,
            8 => Stage::Reset,
            9 => Stage::ReloadStopped,
            10 => Stage::ReloadSweep,
            11 => Stage::ReloadJump,
            12 => Stage::Test,
            13 => Stage::ResetFadt,
            14 => Stage::ResetCf9Hard,
            15 => Stage::ResetCf9Full,
            48 => Stage::ResetKbc,
            _ => return None,
        })
    }
}

/// One crumb as the stores get it.
#[derive(Clone, Copy, Debug)]
pub struct Crumb<'a> {
    /// Counts from 1 per boot.
    pub seq: u32,
    pub stage: Stage,
    /// The shutdown's name (`mcp-reboot`, `reload`, ..) or, for `Device`, the device's Quiesce name.
    pub what: &'a str,
}

/// A place crumbs go. `leave` is lock-free and silent (see the module text); it returns whether the crumb is
/// stored (or on the wire). `clear`: the shutdown did not end the kernel (a refused Reload, a failed power-off):
/// the store goes back to its normal use. Thread context.
pub trait Store: Send + Sync {
    fn leave(&self, c: &Crumb<'_>) -> bool;
    fn clear(&self) {}
}

static WIRE: OnceRef<dyn Store> = OnceRef::new();
static KEEP: OnceRef<dyn Store> = OnceRef::new();

/// klogd's mirror (once, when it first goes live).
pub fn set_wire(s: &'static dyn Store) -> bool {
    WIRE.set(s)
}

/// The store that survives the reset (fch-wdt, once, when the watchdog is armed).
pub fn set_keep(s: &'static dyn Store) -> bool {
    KEEP.set(s)
}

const NAME_MAX: usize = 32;
static SEQ: AtomicU32 = AtomicU32::new(0);
/// The last crumb in RAM (seq << 8 | stage; 0 = none), its name in `LAST_NAME` (bytes, `LAST_LEN` long):
/// for tests and the console after a failed power-off. One writer at a time (the shutdown's own thread).
static LAST: AtomicU64 = AtomicU64::new(0);
static LAST_NAME: [AtomicU8; NAME_MAX] = [const { AtomicU8::new(0) }; NAME_MAX];
static LAST_LEN: AtomicUsize = AtomicUsize::new(0);

counter!(LEFT, "crumb.left");
counter!(WIRE_REFUSED, "crumb.wire_refused");
counter!(KEEP_REFUSED, "crumb.keep_refused");

/// What `leave` did with one crumb.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Left {
    pub seq: u32,
    /// None: no such store registered.
    pub wire: Option<bool>,
    pub keep: Option<bool>,
}

/// Leaves a crumb: `stage` begins now (for `Device`: `what`'s Quiesce). Lock-free and silent.
pub fn leave(stage: Stage, what: &str) -> Left {
    let seq = SEQ.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
    remember(seq, stage, what);
    LEFT.inc();
    let c = Crumb { seq, stage, what };
    let wire = WIRE.get().map(|s| s.leave(&c));
    if wire == Some(false) {
        WIRE_REFUSED.inc();
    }
    let keep = KEEP.get().map(|s| s.leave(&c));
    if keep == Some(false) {
        KEEP_REFUSED.inc();
    }
    Left { seq, wire, keep }
}

/// The shutdown did not end this kernel: the stores back to normal (thread context).
pub fn clear() {
    for s in [WIRE.get(), KEEP.get()].into_iter().flatten() {
        s.clear();
    }
}

fn remember(seq: u32, stage: Stage, what: &str) {
    let b = what.as_bytes();
    let n = b.len().min(NAME_MAX);
    for (i, c) in b[..n].iter().enumerate() {
        LAST_NAME[i].store(*c, Ordering::Relaxed);
    }
    LAST_LEN.store(n, Ordering::Relaxed);
    LAST.store((seq as u64) << 8 | stage as u64, Ordering::Release);
}

/// The last crumb of this boot (seq, stage, the first `NAME_MAX` bytes of its name).
pub fn last() -> Option<(u32, Stage, alloc::string::String)> {
    let v = LAST.load(Ordering::Acquire);
    let stage = Stage::from_u8(v as u8)?;
    let n = LAST_LEN.load(Ordering::Relaxed).min(NAME_MAX);
    let bytes: alloc::vec::Vec<u8> = LAST_NAME[..n].iter().map(|b| b.load(Ordering::Relaxed)).collect();
    Some(((v >> 8) as u32, stage, alloc::string::String::from_utf8_lossy(&bytes).into_owned()))
}

/// The wire line of a crumb (`crumb 7 device xhci0`, `crumb 3 storage mcp-reboot`) into `out`, without
/// allocating; returns its length (cut at `out`'s end).
pub fn line(c: &Crumb<'_>, out: &mut [u8]) -> usize {
    struct W<'a>(&'a mut [u8], usize);
    impl core::fmt::Write for W<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &b in s.as_bytes() {
                if self.1 < self.0.len() {
                    self.0[self.1] = b;
                    self.1 += 1;
                }
            }
            Ok(())
        }
    }
    let mut w = W(out, 0);
    let _ = if c.what.is_empty() {
        core::fmt::write(&mut w, format_args!("crumb {} {}", c.seq, c.stage.as_str()))
    } else {
        core::fmt::write(&mut w, format_args!("crumb {} {} {}", c.seq, c.stage.as_str(), c.what))
    };
    w.1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_numbers_round_trip() {
        for v in (1..=15u8).chain([48]) {
            assert_eq!(Stage::from_u8(v).map(|s| s as u8), Some(v));
        }
        assert_eq!(Stage::from_u8(0), None);
        assert_eq!(Stage::from_u8(16), None, "16..47 are the keep store's device ids");
        assert_eq!(Stage::from_u8(47), None);
        assert_eq!(Stage::from_u8(49), None);
    }

    #[test]
    fn only_the_reset_writes_and_the_jump_end_a_shutdown() {
        let ends: alloc::vec::Vec<&str> =
            (0..=255u8).filter_map(Stage::from_u8).filter(|s| s.ends_a_shutdown()).map(Stage::as_str).collect();
        assert_eq!(ends, ["reset", "reload_jump", "reset_fadt", "reset_cf9_hard", "reset_cf9_full", "reset_kbc"]);
        assert!(!Stage::ReloadSweep.ends_a_shutdown() && !Stage::Quiesced.ends_a_shutdown());
        assert!(!Stage::ReloadJump.is_reset());
    }

    #[test]
    fn the_line_names_seq_stage_and_device() {
        let mut b = [0u8; 64];
        let n = line(&Crumb { seq: 7, stage: Stage::Device, what: "xhci0" }, &mut b);
        assert_eq!(&b[..n], b"crumb 7 device xhci0");
        let mut small = [0u8; 8];
        assert_eq!(line(&Crumb { seq: 7, stage: Stage::Device, what: "xhci0" }, &mut small), 8, "cut, never past the end");
    }

    #[test]
    fn leave_remembers_the_last_and_counts_up() {
        let _g = crate::tests::serial_limited();
        let a = leave(Stage::Test, "first");
        let b = leave(Stage::Test, "a-very-long-device-name-beyond-thirty-two-bytes");
        assert!(b.seq > a.seq);
        let (seq, stage, name) = last().unwrap();
        assert_eq!(seq, b.seq);
        assert_eq!(stage, Stage::Test);
        assert!(name.len() <= NAME_MAX);
    }
}
