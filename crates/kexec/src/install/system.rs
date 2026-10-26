//! RB-9: system generations at run time (B1 "system back to ..."; V2-PLAN §7). The pure part (host-tested):
//! what `system_list` shows, what `system_pin` may write and what the next start (or a Reload `now`) then is,
//! by bitsboot's own rule (`bitsboot_pure::gens::choose`), and which generation ran at a given time (VS-3).
//! The stick side lives in `system_stick`, the tools and console commands in `system_tool`.

use super::plan::Gen;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bitsboot_pure::gens::{self, Choice, GenDir, Pin};
use bootinfo::Generation;

/// One generation with what the stick knows about it beyond its markers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SysGen {
    pub dir: Gen,
    pub kernel_bytes: Option<u64>,
    pub initrd_bytes: Option<u64>,
    /// UTC seconds of KERNEL's last write on the stick (written once: when it was installed).
    pub installed: Option<i64>,
    /// UTC seconds the `OK` marker was written (when the generation confirmed itself).
    pub confirmed: Option<i64>,
    /// FNV-1a 64 of the KERNEL file, the number `install_kernel`'s answer prints (None: not read yet).
    pub kernel_id: Option<u64>,
}

/// The stick's generations and what runs now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub gens: Vec<SysGen>,
    pub pin: Option<u32>,
    pub writable: bool,
    /// The running kernel's GENERATION record (bitsboot's, or the one a Reload handed on).
    pub running: Option<Generation>,
    pub reload: u32,
    /// RB-11: the running kernel's id as the Reload that started it handed it on (`crash::running_kernel_id`).
    pub running_kernel: Option<u64>,
}

/// Which kernel runs (RB-11). After a firmware start bitsboot's GENERATION record names it; after a Reload that
/// record may be the pinned one the boot started with (a URL Reload keeps it), so the kernel id the Reload handed
/// on is matched against the stick's generations instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Running {
    /// Started by bitsboot from this generation (no Reload since), or a Reload into its KERNEL file.
    Gen(u32),
    /// A Reload handed on kernel id `id`, which no listed generation has (all their ids are read).
    Other { id: u64 },
    /// A Reload handed on kernel id `id`, but some generations' ids are not read yet and the answer could still
    /// change (no generation is named then, so `system_pin now` does not skip a Reload on a guess).
    Pending { id: u64 },
    /// After a Reload by a kernel that did not hand the new kernel's id on (an older one).
    Unknown,
    /// No GENERATION record and no Reload: not started by bitsboot.
    NoRecord,
}

/// Which kernel runs, from the GENERATION record, the Reload counter, the handed-on kernel id and the
/// generations' kernel ids `(number, id)` (None: not read yet; only generations that have a KERNEL file).
pub fn identify(record: Option<Generation>, reload: u32, kernel: Option<u64>, ids: &[(u32, Option<u64>)]) -> Running {
    if reload == 0 {
        return record.map_or(Running::NoRecord, |r| Running::Gen(r.number));
    }
    let Some(id) = kernel else { return Running::Unknown };
    let hits = || ids.iter().filter(move |(_, k)| *k == Some(id)).map(|(n, _)| *n);
    // The same file may sit in several generations: the record's one if it is among them, else the newest. Only
    // the record's one is certain while ids are unread (an unread one may be the record's or a newer match).
    if let Some(n) = hits().find(|&n| record.is_some_and(|r| r.number == n)) {
        return Running::Gen(n);
    }
    if ids.iter().any(|(_, k)| k.is_none()) {
        return Running::Pending { id };
    }
    hits().max().map_or(Running::Other { id }, Running::Gen)
}

impl Listing {
    pub fn running_kernel(&self) -> Running {
        let ids: Vec<(u32, Option<u64>)> = self.gens.iter().filter(|g| g.kernel_bytes.is_some()).map(|g| (g.dir.n, g.kernel_id)).collect();
        identify(self.running, self.reload, self.running_kernel, &ids)
    }
}

/// What `system_pin` is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    Pin(u32),
    Off,
}

/// bitsboot's choice for a stick: `(record, mark_try, why)`.
pub type Start = (Generation, bool, String);

/// What bitsboot starts at the next boot with this PIN (files assumed good when present).
pub fn next_start(gens: &[Gen], pin: Option<u32>) -> Result<Start, String> {
    let dirs: Vec<GenDir> = gens.iter().map(Gen::dir).collect();
    match gens::choose(&dirs, pin.map_or(Pin::None, Pin::Number), &[], &[]) {
        Choice::Start { record, mark_try, why } => Ok((record, mark_try, why)),
        Choice::Refuse(t) => Err(t),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinPlan {
    /// The new content of `SYS\PIN` (None: remove it); `write == false`: the stick already says so.
    pub pin: Option<u32>,
    pub write: bool,
    pub was: Option<u32>,
    pub next: Start,
}

fn names(gens: &[Gen]) -> String {
    if gens.is_empty() {
        return String::from("none");
    }
    gens.iter().map(|g| format!("{} ({})", g.n, g.markers())).collect::<Vec<_>>().join(", ")
}

/// Checks a `system_pin` request against the stick (`pin`: what `SYS\PIN` holds now). A pin is refused unless
/// bitsboot would honour it (the generation exists, is confirmed and has both files): bitsboot ignores any
/// other PIN silently, so writing one would only look like "system back to n".
pub fn pin_plan(gens: &[Gen], pin: Option<u32>, want: Want) -> Result<PinPlan, String> {
    let new = match want {
        Want::Off => None,
        Want::Pin(n) => {
            let g = gens.iter().find(|g| g.n == n).ok_or_else(|| format!("no generation {n} on the stick (there are: {})", names(gens)))?;
            if !g.ok {
                return Err(format!(
                    "generation {n} is {}, not confirmed (OK): bitsboot starts a pinned generation only once it has confirmed itself",
                    g.markers()
                ));
            }
            if !(g.kernel && g.initrd) {
                return Err(format!("generation {n} lacks its {}: bitsboot could not start it", if g.kernel { "INITRD" } else { "KERNEL" }));
            }
            Some(n)
        }
    };
    // `Off` always removes the file: a PIN that holds no number reads as None and still makes bitsboot flag it.
    let write = new != pin || want == Want::Off;
    let next = next_start(gens, new)?;
    Ok(PinPlan { pin: new, write, was: pin, next })
}

/// What a Reload `now` goes into after `p`: Ok(None) when that generation runs already. Refused for a
/// generation that would start on trial (NEW): its one try and its confirmation need a firmware start.
/// `running`: the generation whose kernel runs (`identify`; None when not known: the Reload then goes ahead).
pub fn now_target(p: &PinPlan, running: Option<u32>) -> Result<Option<Generation>, String> {
    let (record, mark_try, _) = &p.next;
    if *mark_try || record.reason == bootinfo::gen_reason::NEWEST_TRY {
        return Err(format!(
            "the next start is generation {}'s one try (not confirmed yet): a Reload cannot give it that try (it confirms itself only after a firmware start); reboot instead",
            record.number
        ));
    }
    if running == Some(record.number) {
        return Ok(None);
    }
    Ok(Some(*record))
}

/// VS-3 ("the whole system as at 15:00"): the generation that ran at `utc` seconds: the newest confirmed
/// generation installed by then (an unconfirmed one may have been tried then, but cannot be pinned).
pub fn at_time(gens: &[SysGen], utc: i64) -> Option<u32> {
    gens.iter().filter(|g| g.dir.ok && g.dir.kernel && g.dir.initrd && g.installed.is_some_and(|t| t <= utc)).map(|g| g.dir.n).max()
}

/// `YYYY-MM-DD HH:MM` of UTC seconds shifted by `offset_min`.
pub fn date(utc: i64, offset_min: i32) -> String {
    let d = time::tz::DateTime::from_unix(utc + offset_min as i64 * 60);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year, d.month, d.day, d.hour, d.min)
}

fn mib(b: u64) -> String {
    format!("{}.{} MiB", b >> 20, ((b & ((1 << 20) - 1)) * 10) >> 20)
}

/// One generation's state word: OK, NEW, TRY (or several), "no marker"; files missing say so.
pub fn state(g: &Gen) -> String {
    let mut s = g.markers();
    if !(g.kernel && g.initrd) {
        s.push_str(if g.kernel { ", INITRD missing" } else { ", KERNEL missing" });
    }
    s
}

/// `system_list`'s answer. `offset` gives the local time zone's offset (minutes) at a UTC time.
pub fn text(l: &Listing, offset: &dyn Fn(i64) -> i32) -> String {
    let next = next_start(&l.gens.iter().map(|g| g.dir).collect::<Vec<_>>(), l.pin);
    let next_n = next.as_ref().ok().map(|s| s.0.number);
    let runs = l.running_kernel();
    let mut t = format!("system generations on the boot stick ({}):\n", if l.writable { "read-write" } else { "read-only" });
    if l.gens.is_empty() {
        t.push_str("  none\n");
    }
    for g in l.gens.iter().rev() {
        let when = |x: Option<i64>| x.map_or(String::from("?"), |u| date(u, offset(u)));
        let mut tags = Vec::new();
        if runs == Running::Gen(g.dir.n) {
            tags.push("RUNNING");
        }
        if next_n == Some(g.dir.n) {
            tags.push("NEXT START");
        }
        if l.pin == Some(g.dir.n) {
            tags.push("PINNED");
        }
        t.push_str(&format!("  {:>3}  {:<8} installed {}", g.dir.n, state(&g.dir), when(g.installed)));
        if g.dir.ok {
            t.push_str(&format!(", confirmed {}", when(g.confirmed)));
        }
        t.push_str(&format!(
            "; kernel id {}; KERNEL {}, INITRD {}",
            g.kernel_id.map_or(String::from("(not read yet)"), |k| format!("{k:016x}")),
            g.kernel_bytes.map_or(String::from("-"), mib),
            g.initrd_bytes.map_or(String::from("-"), mib)
        ));
        if !tags.is_empty() {
            t.push_str(&format!("  <- {}", tags.join(", ")));
        }
        t.push('\n');
    }
    t.push_str(&running_line(l, runs));
    match &next {
        Ok((r, _, why)) => t.push_str(&format!("next start: generation {} ({}: {why})\n", r.number, gens::reason_name(r.reason))),
        Err(e) => t.push_str(&format!("next start: nothing ({e})\n")),
    }
    match l.pin {
        Some(p) => t.push_str(&format!("pin: generation {p} (system_pin off removes it)")),
        None => t.push_str("pin: none (the newest confirmed generation starts; system_pin <n> goes back to an older one)"),
    }
    t
}

/// The `running:` line (RB-11): which generation's kernel runs, and bitsboot's reason in `hw_info`'s words.
fn running_line(l: &Listing, runs: Running) -> String {
    let record = |r: Generation| {
        let ok = l.gens.iter().any(|g| g.dir.n == r.number && g.dir.ok);
        format!(
            "{}; bitsboot: {}{}",
            mcp::running_reason_with(&r, ok),
            gens::reason_name(r.reason),
            if r.skipped != 0 { format!(", skipped {}", r.skipped) } else { String::new() }
        )
    };
    let id = |k: u64| format!("kernel id {k:016x}");
    let started = || match l.running {
        Some(r) => format!("the GENERATION record says generation {} ({})", r.number, record(r)),
        None => String::from("no GENERATION record"),
    };
    let after = format!("after Reload {}", l.reload);
    match (runs, l.running) {
        (Running::Gen(n), Some(r)) if r.number == n => {
            let mut s = format!("running: generation {n} ({}", record(r));
            if l.reload > 0 {
                s.push_str(&format!(", {after}"));
            }
            if let Some(k) = l.running_kernel {
                s.push_str(&format!(", {}", id(k)));
            }
            s.push_str(")\n");
            s
        }
        (Running::Gen(n), _) => format!(
            "running: generation {n}'s kernel (from a Reload: {} = generation {n}'s KERNEL, {after}); {}\n",
            l.running_kernel.map_or(String::from("kernel id ?"), id),
            started()
        ),
        (Running::Other { id: k }, _) => {
            format!("running: a kernel from a URL ({}, {after}): no generation on the stick has this KERNEL; {}\n", id(k), started())
        }
        (Running::Pending { id: k }, _) => format!(
            "running: {} ({after}): not all kernel ids are read yet, so which generation's KERNEL this is is not known yet (ask again); {}\n",
            id(k),
            started()
        ),
        (Running::Unknown, _) => format!("running: not known which kernel ({after} by a kernel that did not hand on its kernel id); {}\n", started()),
        (Running::NoRecord, _) => String::from("running: unknown (no GENERATION record: not started by bitsboot)\n"),
    }
}

/// The first lines of `system_pin`'s answer.
pub fn pin_text(p: &PinPlan, dry: bool) -> String {
    let what = match (p.pin, p.was) {
        (Some(n), Some(w)) if n == w => format!("generation {n} was pinned already"),
        (Some(n), _) => format!("PIN set to generation {n}"),
        (None, Some(w)) => format!("PIN removed (it named generation {w})"),
        (None, None) => String::from("no PIN was set"),
    };
    let what = if dry && p.write { format!("dry run, nothing written: would be {what}") } else { what };
    let (r, _, why) = &p.next;
    format!("{what}; the next start is generation {} ({}: {why})", r.number, gens::reason_name(r.reason))
}
