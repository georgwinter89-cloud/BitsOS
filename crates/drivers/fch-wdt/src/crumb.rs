//! RB-16: the watchdog's count register as the breadcrumbs' keep store (`klog::crumb::set_keep`).
//!
//! Why this register: the Mini-PC's reset and its sync flood clear RAM; CMOS and the FCH's BIOS RAM belong to
//! the firmware (no byte is documented free; coreboot keeps its own S3 and UMA values in BIOS RAM). The count
//! register is the watchdog's, which BitsOS owns, and it survives the platform reset: after a panic armed the
//! 120 s grace and the reset came 90 s later, the next start read `count=29` (desk 28.10., gen of RB-13). The
//! counter counts seconds down while it runs and keeps its value across the reset.
//!
//! The code is the watchdog's timeout at the same time (a kick reloads the counter from it), so it stays in a
//! range that still resets a hung shutdown within minutes. The range starts at the fixed [`BASE`], above every
//! count the normal use leaves (the timeout and the grace, both in seconds); it does not depend on the
//! settings, so a start decodes what an earlier start with other watchdog settings left. A start whose timeout
//! or grace reaches into the range uses no keep store (`base` = 0) and decodes nothing.
//!
//! Each id owns a slot of its own width, and its code is the top of the slot: the counter runs on between the
//! crumb and the next one (or the reset), so the width is the longest the step or the device's Quiesce may take
//! plus one tick of the 1 Hz counter and a margin (NVMe's shutdown and RDY waits: 15 s; the storage stop and the
//! park: 5 s; everything else under 2 s). A step that ran longer than its slot would read as the slot below.
//!
//! The layout is the stored format: never reorder, change widths or renumber (append only, at the end).

use alloc::format;
use alloc::string::String;
use klog::crumb::Stage;

/// The first count of the crumb range: above the largest normal count (the default grace 240 s) plus `MARGIN`.
pub const BASE: u32 = 256;
/// Seconds between the largest normal count and `BASE`.
const MARGIN: u32 = 16;
/// The slot of a step or device that takes at most about 5 s.
const SHORT: u32 = 8;
/// The storage stop (`block::shutdown::STOP_NS`, 5 s) and the park.
const MID: u32 = 16;
/// NVMe's Quiesce: shutdown (5 s) and RDY (at most 10 s).
const LONG: u32 = 24;

/// Device kinds: Quiesce name prefix, first id, instances (`xhci0`..`xhci5`), slot width. A kind with more than
/// one instance has one more id after them for every later instance (`xhci6` and up: the kind is still named;
/// the listener's wire crumb has the exact name). A name not in the table is `other`. The counter is 16 bits
/// and its value is also the timeout, so the table cannot grow with the machine (R1 exception, RB-16 notes).
const KINDS: [(&str, u16, u16, u32); 11] = [
    ("mirror", 16, 1, SHORT),
    ("igc", 17, 2, SHORT),
    ("wlan", 20, 2, SHORT),
    ("hda", 23, 4, SHORT),
    ("xhci", 28, 6, SHORT),
    ("nvme", 35, 4, LONG),
    ("virtio-net", 40, 2, SHORT),
    ("amdgpu.flip", 43, 1, SHORT),
    ("amdgpu.psp", 44, 1, SHORT),
    ("snd/mix", 45, 1, SHORT),
    ("fch-wdt", 46, 1, SHORT),
];
const OTHER: u16 = 47;
/// RB-17: stages appended after the devices (the ids 1..15 below the devices were all taken): their id is the
/// stage's number. A start of an older generation reads their counts as above its range: no crumb, never
/// "stopped there".
const LATE_STAGES: [Stage; 1] = [Stage::ResetKbc];
/// One past the last id.
pub const IDS: u16 = OTHER + 1 + LATE_STAGES.len() as u16;
/// The first device id (the ids below are the stages; 13..15 are the reset writes since RB-17).
const FIRST_DEVICE: u16 = 16;

/// Whether an id is a stage's (below the devices, or appended after them).
fn is_stage_id(id: u16) -> bool {
    id < FIRST_DEVICE || id > OTHER
}

/// The stage an id stands for.
fn stage_of(id: u16) -> Option<Stage> {
    if !is_stage_id(id) {
        return None;
    }
    u8::try_from(id).ok().and_then(Stage::from_u8)
}

/// The slot width of an id.
fn width(id: u16) -> u32 {
    if is_stage_id(id) {
        return match stage_of(id) {
            Some(Stage::Storage) | Some(Stage::Parked) => MID,
            _ => SHORT,
        };
    }
    for (_, first, n, w) in KINDS {
        let last = if n > 1 { first + n } else { first };
        if (first..=last).contains(&id) {
            return w;
        }
    }
    SHORT
}

/// The first count of an id's slot above `BASE` (ids 1.. in order, each its width).
fn start(id: u16) -> u32 {
    (1..id).map(width).sum()
}

/// One past the last count of the range (`BASE` + every slot).
pub fn end(base: u32) -> u32 {
    base + start(IDS)
}

/// The crumb range's first count for this configuration: `BASE`, or 0 (no keep store) when the timeout or the
/// grace would reach into it.
pub fn base(timeout_s: u32, grace_s: u32) -> u32 {
    if timeout_s.max(grace_s) + MARGIN > BASE || end(BASE) > crate::regs::MAX_SECONDS { 0 } else { BASE }
}

/// The id of a crumb (a stage's id is its number: 1..15 and the appended `LATE_STAGES`).
pub fn id(stage: Stage, what: &str) -> u16 {
    if stage != Stage::Device {
        return stage as u16;
    }
    for (prefix, first, n, _) in KINDS {
        let Some(rest) = what.strip_prefix(prefix) else { continue };
        let inst = if rest.is_empty() {
            Some(0)
        } else if n > 1 && rest.bytes().all(|b| b.is_ascii_digit()) && rest.len() <= 5 {
            rest.parse::<u16>().ok()
        } else {
            None
        };
        match inst {
            Some(i) if i < n => return first + i,
            Some(_) => return first + n,
            None => continue,
        }
    }
    OTHER
}

/// The count that stands for `id` (the top of its slot).
pub fn code(id: u16, base: u32) -> Option<u32> {
    (base != 0 && id != 0 && id < IDS).then(|| base + start(id) + width(id) - 1)
}

/// The id a count read at boot stands for (None: below the range, a slot without an id, or above it).
pub fn decode(count: u32, base: u32) -> Option<u16> {
    if base == 0 || count < base || count >= end(base) {
        return None;
    }
    let off = count - base;
    let id = (1..IDS).find(|&id| off < start(id) + width(id))?;
    (!is_stage_id(id) || stage_of(id).is_some()).then_some(id)
}

/// What an id names, for the report: `step storage`, `device xhci2`, `device xhci (instance 6 or later)`.
pub fn describe(id: u16) -> String {
    if is_stage_id(id) {
        return match stage_of(id) {
            Some(Stage::ResetKbc) => String::from("step reset_kbc (the 8042 pulse; the triple fault follows it)"),
            Some(s) => format!("step {}", s.as_str()),
            None => format!("id {id}"),
        };
    }
    for (prefix, first, n, _) in KINDS {
        if (first..first + n).contains(&id) {
            return if n == 1 { format!("device {prefix}") } else { format!("device {prefix}{}", id - first) };
        }
        if n > 1 && id == first + n {
            return format!("device {prefix} (instance {n} or later: see the listener's crumb lines)");
        }
    }
    String::from("device other (not in the table: see the listener's crumb lines)")
}

/// Whether the decoded crumb says the previous shutdown finished: a reset write (`reset` or one of RB-17's
/// per-write crumbs) or the Reload's jump came next. Anything else stopped earlier (a flood, a hang, a power cut
/// inside the shutdown).
pub fn reached_the_end(id: u16) -> bool {
    stage_of(id).is_some_and(Stage::ends_a_shutdown)
}

/// The next start's view of the count the previous one left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Previous {
    pub count: u32,
    pub base: u32,
    /// The decoded crumb, None: the count is outside the range.
    pub id: Option<u16>,
    /// The counter still ran at this start (a Reload with `watchdog_reload=grace`): it went on counting through
    /// the start, so the slot is only approximate.
    pub running: bool,
}

impl Previous {
    pub fn read(count: u32, base: u32) -> Previous {
        Previous::read_with(count, base, false)
    }

    pub fn read_with(count: u32, base: u32, running: bool) -> Previous {
        Previous { count, base, id: decode(count, base), running }
    }

    /// The crumb says the previous shutdown stopped before its end (and the slot is exact).
    pub fn incomplete(&self) -> bool {
        !self.running && self.id.is_some_and(|id| !reached_the_end(id))
    }

    /// One line for the boot log and the `watchdog` tool.
    pub fn text(&self) -> String {
        match self.id {
            Some(id) => format!(
                "last crumb {}{} (count={} slot {}..{}: up to {} s after the crumb){}",
                if self.running { "about " } else { "" },
                describe(id),
                self.count,
                self.base + start(id),
                self.base + start(id) + width(id) - 1,
                width(id) - 1,
                if self.running {
                    ": the counter still ran at this start (counted on through it)"
                } else if reached_the_end(id) {
                    ": the previous shutdown reached its end"
                } else {
                    ": the previous shutdown stopped there"
                }
            ),
            None if self.base == 0 => format!("no crumb store (count={}: timeout or grace too large for the range)", self.count),
            None => format!(
                "no crumb (count={} outside {}..{}: normal use, or the store was not reached)",
                self.count,
                self.base,
                end(self.base) - 1
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_lies_above_the_normal_counts_and_does_not_follow_the_settings() {
        assert_eq!(base(30, 240), BASE);
        assert_eq!(base(30, 30), BASE, "the same range for every setting below it");
        assert_eq!(base(200, 60), BASE);
        assert_eq!(base(30, 241), 0, "a grace that reaches into the range: no store");
        assert_eq!(base(0xFF00, 30), 0);
        // RB-17: 728 before the appended `reset_kbc` slot (8 s more).
        assert!(end(BASE) <= 12 * 60 + 16, "the longest timeout a crumb sets: about 12 minutes, got {}", end(BASE));
        assert_eq!(code(5, 0), None);
    }

    #[test]
    fn slots_are_contiguous_and_wide_enough_for_their_step() {
        let mut next = BASE;
        for id in 1..IDS {
            assert_eq!(BASE + start(id), next, "id {id}");
            next += width(id);
        }
        assert_eq!(next, end(BASE));
        assert!(width(id(Stage::Device, "nvme0")) > 15 + 1, "NVMe: 5 s shutdown + 10 s RDY + a tick");
        assert!(width(Stage::Storage as u16) > 5 + 1, "the storage stop's 5 s");
        assert!(width(id(Stage::Device, "nvme9")) == LONG, "the overflow id keeps the kind's width");
    }

    #[test]
    fn ids_by_stage_and_device_name() {
        assert_eq!(id(Stage::Storage, "mcp-reboot"), 2);
        assert_eq!(id(Stage::Reset, ""), 8);
        assert_eq!(describe(id(Stage::Device, "xhci2")), "device xhci2");
        assert_eq!(describe(id(Stage::Device, "xhci5")), "device xhci5");
        assert_eq!(describe(id(Stage::Device, "wlan0")), "device wlan0");
        assert_eq!(describe(id(Stage::Device, "amdgpu.psp")), "device amdgpu.psp");
        assert_eq!(describe(id(Stage::Device, "snd/mix")), "device snd/mix");
        assert_eq!(describe(id(Stage::Device, "mirror")), "device mirror");
        assert_eq!(describe(id(Stage::Device, "virtio-net0")), "device virtio-net0");
        assert_eq!(describe(id(Stage::Device, "nvme3")), "device nvme3");
        assert!(describe(id(Stage::Device, "xhci9")).starts_with("device xhci (instance 6 or later"), "beyond the instances: the kind stays");
        assert_eq!(id(Stage::Device, "xhci9"), id(Stage::Device, "xhci60000"));
        assert_eq!(id(Stage::Device, "rb-quiesce-test"), OTHER);
        assert_eq!(id(Stage::Device, "igcx"), OTHER);
        assert_eq!(id(Stage::Device, "mirror2"), OTHER, "a single-instance kind takes its exact name only");
        let mut seen = alloc::vec::Vec::new();
        for (prefix, first, n, _) in KINDS {
            for i in 0..n + (n > 1) as u16 {
                let name = if n == 1 { String::from(prefix) } else { format!("{prefix}{i}") };
                let v = id(Stage::Device, &name);
                assert_eq!(v, first + i, "{name}");
                assert!(!seen.contains(&v), "{name}: id {v} twice");
                seen.push(v);
            }
        }
        assert!(seen.iter().all(|&v| v >= FIRST_DEVICE && v < OTHER));
    }

    #[test]
    fn every_id_round_trips_with_drift_up_to_its_width() {
        let b = base(30, 240);
        for id in (1..=15).chain(FIRST_DEVICE..IDS) {
            let c = code(id, b).unwrap();
            for drift in 0..width(id) {
                assert_eq!(decode(c - drift, b), Some(id), "id {id} drift {drift}");
            }
        }
        let nvme = id(Stage::Device, "nvme1");
        assert_eq!(decode(code(nvme, b).unwrap() - 16, b), Some(nvme), "an NVMe shutdown of 16 s still names NVMe");
        assert_eq!(decode(239, b), None, "the grace left by a clean reboot");
        assert_eq!(decode(29, b), None, "a panic's grace");
        assert_eq!(decode(0, b), None);
        assert_eq!(decode(u32::MAX, b), None);
        assert_eq!(decode(b + start(14) + 3, b), Some(Stage::ResetCf9Hard as u16), "13..15: the reset writes (RB-17)");
        assert_eq!(decode(end(b), b), None, "above the range");
        assert_eq!(decode(0xFFFF, b), None, "a counter that was never written");
    }

    #[test]
    fn the_report_says_whether_the_shutdown_reached_its_end() {
        let b = base(30, 240);
        let p = Previous::read(code(Stage::Reset as u16, b).unwrap() - 1, b);
        assert!(p.text().contains("step reset") && p.text().contains("reached its end"), "{}", p.text());
        let p = Previous::read(code(id(Stage::Device, "wlan0"), b).unwrap(), b);
        assert!(p.text().contains("device wlan0") && p.text().contains("stopped there") && p.text().contains("up to 7 s"), "{}", p.text());
        assert!(Previous::read(200, b).text().starts_with("no crumb"));
        let p = Previous::read_with(code(id(Stage::Device, "wlan0"), b).unwrap(), b, true);
        assert!(!p.incomplete() && p.text().contains("about device wlan0"), "{}", p.text());
        assert!(Previous::read(code(id(Stage::Device, "wlan0"), b).unwrap(), b).incomplete());
        assert!(!Previous::read(code(Stage::Reset as u16, b).unwrap(), b).incomplete());
        assert!(Previous::read(300, 0).text().starts_with("no crumb store"));
        // RB-17: every reset write's crumb is an end; the 8042's names the triple fault after it.
        for st in [Stage::ResetFadt, Stage::ResetCf9Hard, Stage::ResetCf9Full, Stage::ResetKbc] {
            let p = Previous::read(code(st as u16, b).unwrap() - 2, b);
            assert!(!p.incomplete() && p.text().contains(st.as_str()) && p.text().contains("reached its end"), "{}", p.text());
        }
        assert!(Previous::read(code(Stage::ResetKbc as u16, b).unwrap(), b).text().contains("triple fault"));
        assert!(Previous::read(code(Stage::ReloadSweep as u16, b).unwrap(), b).incomplete());
    }

    /// RB-17: the stored format. The counts RB-16 wrote keep their meaning (the Mini-PC read 719 for the watchdog's
    /// own Quiesce), the new ids take the free slots 13..15 and one slot appended at the end, and a start of the
    /// RB-16 generation reads every new count as "no crumb" (never "stopped there").
    #[test]
    fn the_stored_format_only_grows_and_old_starts_read_the_new_crumbs_as_none() {
        let b = BASE;
        let codes = |ids: &[u16]| ids.iter().map(|&i| code(i, b).unwrap()).collect::<alloc::vec::Vec<_>>();
        assert_eq!(codes(&[1, 2, 8, 9, 10, 11, 12]), [263, 279, 335, 343, 351, 359, 367], "RB-16 stages");
        assert_eq!(codes(&[16, 35, 46, 47]), [399, 567, 719, 727], "RB-16 devices (fch-wdt = 719, other = 727)");
        let new = [Stage::ResetFadt, Stage::ResetCf9Hard, Stage::ResetCf9Full, Stage::ResetKbc];
        assert_eq!(codes(&new.map(|s| s as u16)), [375, 383, 391, 735]);
        // The RB-16 decoder: the same slots, ids 1..12 and 16..47, the range ending at 728.
        let rb16 = |count: u32| -> Option<u16> {
            if count < b || count >= b + start(OTHER + 1) {
                return None;
            }
            let id = (1..OTHER + 1).find(|&id| count - b < start(id) + width(id))?;
            (id >= FIRST_DEVICE || id <= 12).then_some(id)
        };
        assert_eq!(b + start(OTHER + 1), 728);
        for st in new {
            for drift in 0..SHORT {
                assert_eq!(rb16(code(st as u16, b).unwrap() - drift), None, "{} drift {drift}", st.as_str());
            }
        }
        for id in (1..=12).chain(FIRST_DEVICE..=OTHER) {
            assert_eq!(rb16(code(id, b).unwrap()), decode(code(id, b).unwrap(), b), "id {id} reads the same in both");
        }
    }
}
