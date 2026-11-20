//! The records this boot found (CRASHDISK): the RAM page's and the disk slots', newest first, a RAM record and
//! its disk twin (the same panic) as one. `crash_info` lists and shows them, the newest unreported one becomes a
//! journal event and the desktop's line (`/proc/bitsos/crash`).

use super::format::{self, Head};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use ksync::SpinLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Ram,
    Disk { slot: usize },
    /// A disk record whose panic the RAM page kept too.
    Both { slot: usize },
}

impl Source {
    pub fn name(self) -> String {
        match self {
            Source::Ram => String::from("RAM"),
            Source::Disk { slot } => format!("disk slot {slot}"),
            Source::Both { slot } => format!("disk slot {slot} (and RAM)"),
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Source::Ram => "ram",
            Source::Disk { .. } => "disk",
            Source::Both { .. } => "disk+ram",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rec {
    pub source: Source,
    /// Disk records: the header as found (flags without REPORTED's news); RAM: the fields it has (seq 0).
    pub head: Head,
    pub text: String,
    /// Logged by an earlier boot already.
    pub reported_before: bool,
}

impl Rec {
    pub fn first_line(&self) -> &str {
        self.text.lines().next().unwrap_or("")
    }
    pub fn same_panic(&self, o: &Rec) -> bool {
        self.head.boot_id == o.head.boot_id && self.head.time_ns == o.head.time_ns && self.head.cpu == o.head.cpu
    }
}

/// How long after a crash a cold boot still counts its record as new when /boot is read-only (see `new_unmarked`).
pub const UNMARKED_NEW_S: i64 = 15 * 60;

/// "New" for a disk record whose REPORTED flag cannot be set (/boot read-only at this boot: nvme_boot= taken back,
/// or fs-fat's doubts): without a write, every later boot would call it new again. So it is new only when it is
/// the boot right before's: the RAM marker names its boot (a warm reset), or its wall time lies at most
/// `UNMARKED_NEW_S` before this boot began (a cold reset soon after the crash; 2 min slack for the clocks).
pub fn new_unmarked(rec_boot: &[u8; 16], rec_wall: i64, prev_boot: Option<&[u8; 16]>, boot_wall: Option<i64>) -> bool {
    if prev_boot == Some(rec_boot) {
        return true;
    }
    match boot_wall {
        Some(b) if rec_wall > 0 => rec_wall <= b + 120 && b - rec_wall <= UNMARKED_NEW_S,
        _ => false,
    }
}

/// Disk records as `boot` read them, newest first.
static DISK: SpinLock<Vec<Rec>> = SpinLock::named("crash.disk.records", Vec::new());

pub(super) fn set_disk(mut v: Vec<Rec>) {
    v.sort_by(|a, b| b.head.seq.cmp(&a.head.seq));
    *DISK.lock() = v;
}

fn ram() -> Option<Rec> {
    let p = super::super::previous()?;
    let h = &p.header;
    let head = Head {
        kind: format::KIND_RECORD,
        flags: h.flags & !super::super::record::flags::REPORTED,
        boot_id: h.boot_id,
        time_ns: h.time_ns,
        generation: h.generation,
        reload: h.reload,
        cpu: h.cpu,
        cpus_asked: h.cpus_asked,
        cpus_answered: h.cpus_answered,
        body_len: p.text.len() as u32,
        ..Head::default()
    };
    Some(Rec { source: Source::Ram, head, text: p.text.clone(), reported_before: p.reported_before })
}

/// Every record, newest first: the RAM page's merges with its disk twin; an unreported RAM record (from the boot
/// right before) comes first, a RAM record logged before comes after the disk's.
pub fn records() -> Vec<Rec> {
    let mut v = DISK.lock().clone();
    let Some(r) = ram() else { return v };
    if let Some(d) = v.iter_mut().find(|d| d.same_panic(&r)) {
        if let Source::Disk { slot } = d.source {
            d.source = Source::Both { slot };
        }
        d.reported_before &= r.reported_before;
        return v;
    }
    if r.reported_before {
        v.push(r);
    } else {
        v.insert(0, r);
    }
    v
}

pub fn newest() -> Option<Rec> {
    records().into_iter().next()
}

/// "17:49" or "12.11. um 17:49" in the machine's zone (CONFIG `tz=`), relative to `now_s`.
pub fn local_when(wall_s: i64, now_s: i64, tz: time::tz::Tz) -> String {
    let local = |t: i64| t + tz.offset_at(t) as i64 * 60;
    let (l, n) = (local(wall_s), local(now_s));
    let (day, nday) = (l.div_euclid(86_400), n.div_euclid(86_400));
    let secs = l.rem_euclid(86_400);
    let hm = format!("{:02}:{:02}", secs / 3600, (secs / 60) % 60);
    if day == nday {
        hm
    } else {
        let (_, m, d) = time::tz::civil_from_days(day);
        format!("{d}.{m}. um {hm}")
    }
}

/// The desktop's line (plain German, no code words).
pub fn notice_text(r: &Rec, now_s: i64, tz: time::tz::Tz) -> String {
    let what = if r.head.flags & format::flags::HANG != 0 { "hängen geblieben und wurde neu gestartet" } else { "abgestürzt" };
    let saved = match r.source {
        Source::Ram => "Der Bericht ist gesichert (nur im Speicher).",
        _ => "Der Bericht ist gesichert.",
    };
    let when = if r.head.wall_s > 0 {
        let w = local_when(r.head.wall_s, now_s, tz);
        if w.contains('.') { format!("am {w}") } else { format!("um {w}") }
    } else {
        String::from("beim letzten Mal")
    };
    format!("BitsOS ist {when} {what}. {saved} Zum Lesen „absturz“ in die Adresszeile tippen.")
}

fn now_s() -> i64 {
    time::wall::realtime_ns().map_or(0, |n| (n / 1_000_000_000) as i64)
}

/// `/proc/bitsos/crash`: one line for machines, the desktop's line, then the newest record's text. `decided=0`
/// while the boot thread still waits for /boot (the disk's records are not read yet: the desktop asks again).
fn render() -> String {
    let decided = (super::state() != super::State::Waiting) as u8;
    let Some(r) = newest() else { return format!("new=0 source=none decided={decided}\n") };
    let h = &r.head;
    let mut t = format!(
        "new={} source={} slot={} seq={} wall={} boot={} generation={} cut={} hang={} decided={decided}\nnotice={}\n\n",
        !r.reported_before as u8,
        r.source.word(),
        match r.source {
            Source::Disk { slot } | Source::Both { slot } => slot as i64,
            Source::Ram => -1,
        },
        h.seq,
        h.wall_s,
        hex(&h.boot_id),
        h.generation,
        (h.flags & format::flags::CUT != 0) as u8,
        (h.flags & format::flags::HANG != 0) as u8,
        notice_text(&r, now_s(), time::wall::tz())
    );
    t.push_str(&r.text);
    t
}
vfs::procfile!("/proc/bitsos/crash", render);

pub fn hex(id: &[u8; 16]) -> String {
    let mut s = String::new();
    for b in id {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// One record's summary line for crash_info.
pub fn summary(i: usize, r: &Rec) -> String {
    let h = &r.head;
    let when = if h.wall_s > 0 { local_when(h.wall_s, now_s(), time::wall::tz()) } else { String::from("time unknown") };
    format!(
        "[{i}] {} seq {} at {when}, boot {} generation {}, after {}.{:03} s on CPU {}{}{}{}: {}",
        r.source.name(),
        h.seq,
        hex(&h.boot_id),
        h.generation,
        h.time_ns / 1_000_000_000,
        (h.time_ns / 1_000_000) % 1000,
        h.cpu,
        if h.flags & format::flags::HANG != 0 { ", a hang the watchdog ended" } else { "" },
        if h.flags & format::flags::CUT != 0 { ", cut" } else { "" },
        if r.reported_before { ", logged by an earlier boot" } else { "" },
        r.first_line()
    )
}

/// The journal event of the newest record this boot found unreported (the RAM path logs its own `previous`).
pub(super) fn report_new() {
    let all = records();
    let Some(r) = all.first().filter(|r| !r.reported_before) else { return };
    let (Source::Disk { slot } | Source::Both { slot }) = r.source else { return };
    let h = &r.head;
    klog::event!(Error, "crash", "disk.previous", slot = slot, seq = h.seq, wall_s = h.wall_s, boot = hex(&h.boot_id), generation = h.generation,
        cut = h.flags & format::flags::CUT != 0, hang = h.flags & format::flags::HANG != 0, also_ram = matches!(r.source, Source::Both { .. }),
        bytes = r.text.len(); "the boot before ended in a crash (disk copy): {}", r.first_line());
    if matches!(r.source, Source::Disk { .. }) {
        // Not in RAM: its first lines (message and backtrace) go to the log too; the whole record is in crash_info.
        for (i, l) in r.text.lines().take(60).enumerate() {
            klog::event!(Info, "crash", "disk.previous.line", n = i, text = l);
        }
    }
}
