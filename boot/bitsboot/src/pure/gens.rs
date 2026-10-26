//! System generations `\BITSOS2\SYS\<n>\` and the marker rule (V2-PLAN §7, B1). bitsboot calls [`choose`]
//! again after every generation whose files turn out bad, until it starts one or there is nothing left.
//!
//! The rule (N = the highest-numbered generation directory):
//! 1. `\BITSOS2\SYS\PIN` names a generation with `OK` and good files -> start it.
//! 2. N has `TRY` (its one start was never confirmed) -> the newest `OK` generation.
//! 3. N has `NEW` -> write `TRY` next to it, start N (its one try).
//! 4. N has `OK` -> start N.
//! 5. N's KERNEL/INITRD missing or failing the checks -> the newest `OK` generation at once.
//!
//! Markers combine as: `OK` beats `TRY` beats `NEW` (a confirmation interrupted between writing `OK` and
//! deleting `TRY` counts as confirmed; `NEW` stays next to `TRY`, see markers.rs). Decisions the plan leaves
//! open (BT-2 notes): a `TRY` generation with no `OK` generation to fall back to is started again (otherwise
//! the first generation on a fresh stick could not start a second time before RB-6 confirms it); a newest
//! generation without any marker gets one try like `NEW` when an older `OK` generation exists (a hand copy or
//! an interrupted install must not take the fallback away), and is started as it is only when none exists
//! (xtask test images); an unmarked generation never serves as a fallback; bad files with no `OK` generation
//! left stop with the error screen. `TRY` stays until the generation is confirmed or replaced, as §7 says
//! (no `BAD`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::{Generation, gen_reason};

pub const SYS: &str = "\\BITSOS2\\SYS";

/// Bits of the GENERATION record's `flags` (they live in `bootinfo::gen_flag`; `choose_read_only` sets
/// `SAFE_MODE`).
pub use bootinfo::gen_flag as flag;

/// What the loader found in one generation directory (from its listing; names compare case-insensitively).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GenDir {
    pub number: u32,
    pub kernel: bool,
    pub initrd: bool,
    pub new: bool,
    pub tried: bool,
    pub ok: bool,
}

impl GenDir {
    /// Records one entry of the generation's directory; directories never count as files or markers.
    pub fn note(&mut self, name: &str, is_dir: bool) {
        if is_dir {
            return;
        }
        let is = |s: &str| name.eq_ignore_ascii_case(s);
        if is("KERNEL") {
            self.kernel = true;
        } else if is("INITRD") {
            self.initrd = true;
        } else if is("NEW") {
            self.new = true;
        } else if is("TRY") {
            self.tried = true;
        } else if is("OK") {
            self.ok = true;
        }
    }

    /// `NEW+TRY`, `OK`, `no marker`: for the log.
    pub fn markers(&self) -> String {
        let on: Vec<&str> = [(self.new, "NEW"), (self.tried, "TRY"), (self.ok, "OK")]
            .iter()
            .filter(|(b, _)| *b)
            .map(|(_, n)| *n)
            .collect();
        if on.is_empty() { String::from("no marker") } else { on.join("+") }
    }
}

/// A generation directory name: decimal digits only (`u32::from_str` would also take "+7").
pub fn parse_name(name: &str) -> Option<u32> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

/// The content of `\BITSOS2\SYS\PIN`: one generation number, white space around it allowed.
pub fn parse_pin(text: &[u8]) -> Option<u32> {
    parse_name(core::str::from_utf8(text).ok()?.trim())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pin {
    None,
    Number(u32),
    /// PIN exists but holds no generation number (or could not be read).
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Load this generation; `mark_try` = write its `TRY` once its files passed the checks.
    Start { record: Generation, mark_try: bool, why: String },
    /// Nothing can be started.
    Refuse(String),
}

/// One step of the marker rule. `bad` = generations whose files failed the checks so far; `mark_failed` =
/// generations whose `TRY` could not be written. A Start never names a generation in `bad`, and the
/// loader adds the started one to `bad` or `mark_failed` before asking again, so its loop ends.
pub fn choose(gens: &[GenDir], pin: Pin, bad: &[u32], mark_failed: &[u32]) -> Choice {
    let Some(n) = gens.iter().max_by_key(|g| g.number).copied() else {
        return Choice::Refuse(format!("{SYS} holds no generation directory"));
    };
    let good = |g: &GenDir| g.kernel && g.initrd && !bad.contains(&g.number);
    let newest_ok = gens.iter().filter(|g| g.ok && good(g) && g.number != n.number).max_by_key(|g| g.number).copied();
    let others = |skipped: u32| if bad.iter().any(|&b| b != skipped) { flag::OTHERS_SKIPPED } else { 0 };
    let rec = |number, reason, skipped, flags| Generation { number, reason, skipped, flags };
    let start = |record: Generation, mark_try, why: String| Choice::Start { record, mark_try, why };

    let (flags, pin_note) = match pin {
        Pin::None => (0, String::new()),
        Pin::Invalid => (flag::PIN_IGNORED, String::from("; PIN ignored: it holds no generation number")),
        Pin::Number(p) => match gens.iter().find(|g| g.number == p) {
            Some(g) if g.ok && good(g) => {
                let skipped = if p == n.number { 0 } else { n.number };
                return start(rec(p, gen_reason::PINNED, skipped, others(skipped)), false, format!("pinned by {SYS}\\PIN"));
            }
            None => (flag::PIN_IGNORED, format!("; PIN {p} ignored: no such generation")),
            Some(g) if !g.ok => (flag::PIN_IGNORED, format!("; PIN {p} ignored: not confirmed (no OK)")),
            Some(_) => (flag::PIN_IGNORED, format!("; PIN {p} ignored: its files are bad")),
        },
    };

    let fallback = |reason: u32, what: &str| {
        newest_ok.map(|o| {
            let why = format!("newest confirmed generation; generation {} {what}{pin_note}", n.number);
            start(rec(o.number, reason, n.number, flags | others(n.number)), false, why)
        })
    };
    let again = |extra: u32, what: &str| {
        let f = flags | extra | flag::NO_CONFIRMED_FALLBACK | others(0);
        start(rec(n.number, gen_reason::NEWEST_TRY, 0, f), false, format!("{what}; no confirmed generation to fall back to{pin_note}"))
    };

    if !good(&n) {
        let what = if bad.contains(&n.number) {
            "failed the checks"
        } else if !n.kernel {
            "has no KERNEL"
        } else {
            "has no INITRD"
        };
        return fallback(gen_reason::FALLBACK_BAD_FILES, what).unwrap_or_else(|| {
            Choice::Refuse(format!("generation {} {what} and no older confirmed (OK) generation can start{pin_note}", n.number))
        });
    }
    if n.ok {
        return start(rec(n.number, gen_reason::NEWEST_OK, 0, flags | others(0)), false, format!("newest, confirmed{pin_note}"));
    }
    if n.tried {
        return fallback(gen_reason::FALLBACK_AFTER_TRY, "was tried and never confirmed")
            .unwrap_or_else(|| again(0, "newest, tried before and not confirmed"));
    }
    // NEW, or no marker at all while an older confirmed generation exists: one try, then the fallback.
    if n.new || newest_ok.is_some() || mark_failed.contains(&n.number) {
        let (um, what) = if n.new { (0, "new") } else { (flag::UNMARKED, "without markers") };
        if mark_failed.contains(&n.number) {
            return fallback(gen_reason::FALLBACK_AFTER_TRY, &format!("is {what} but its TRY marker could not be written"))
                .unwrap_or_else(|| again(flag::MARKER_WRITE_FAILED | um, &format!("newest, {what}, its TRY marker could not be written")));
        }
        let why = format!("newest, {what}: TRY is written, its one try{pin_note}");
        return start(rec(n.number, gen_reason::NEWEST_TRY, 0, flags | um | others(0)), true, why);
    }
    let f = flags | flag::UNMARKED | others(0);
    start(rec(n.number, gen_reason::NEWEST_NO_RULE, 0, f), false, format!("newest, without markers, no confirmed generation{pin_note}"))
}

/// The marker rule without writing (safe mode, BT-5): where [`choose`] would give the newest generation its
/// one try (write `TRY`), the newest confirmed generation starts instead and the new one keeps its try for a
/// normal boot; with no confirmed generation the new one starts without a `TRY`. Every Start carries
/// [`flag::SAFE_MODE`] and none asks for a marker.
pub fn choose_read_only(gens: &[GenDir], pin: Pin, bad: &[u32]) -> Choice {
    match choose(gens, pin, bad, &[]) {
        Choice::Start { record, mark_try: true, .. } => {
            let n = record.number;
            let ok = |g: &&GenDir| g.ok && g.kernel && g.initrd && !bad.contains(&g.number) && g.number != n;
            let pin_flag = record.flags & flag::PIN_IGNORED;
            match gens.iter().filter(ok).max_by_key(|g| g.number) {
                Some(o) => {
                    let others = if bad.iter().any(|&b| b != n) { flag::OTHERS_SKIPPED } else { 0 };
                    let flags = pin_flag | others | flag::SAFE_MODE;
                    let record = Generation { number: o.number, reason: gen_reason::FALLBACK_AFTER_TRY, skipped: n, flags };
                    let why = format!("safe mode writes no TRY: newest confirmed generation; generation {n} keeps its one try for a normal boot");
                    Choice::Start { record, mark_try: false, why }
                }
                None => {
                    let flags = record.flags | flag::NO_CONFIRMED_FALLBACK | flag::SAFE_MODE;
                    let record = Generation { number: n, reason: gen_reason::NEWEST_NO_RULE, flags, ..record };
                    let why = String::from("newest, unconfirmed; safe mode starts it without writing TRY (no confirmed generation)");
                    Choice::Start { record, mark_try: false, why }
                }
            }
        }
        // Already TRY and no OK to fall back to: started again, but never as NEWEST_TRY, the reason
        // kexec.boot_confirm acts on (it could neither hear MCP nor write OK on the read-only stick).
        Choice::Start { record, mark_try: false, why } if record.reason == gen_reason::NEWEST_TRY => {
            let flags = record.flags | flag::SAFE_MODE;
            let why = format!("{why}; safe mode: started as NEWEST_NO_RULE, no boot confirmation");
            Choice::Start { record: Generation { reason: gen_reason::NEWEST_NO_RULE, flags, ..record }, mark_try: false, why }
        }
        Choice::Start { record, mark_try: false, why } => {
            Choice::Start { record: Generation { flags: record.flags | flag::SAFE_MODE, ..record }, mark_try: false, why }
        }
        refuse => refuse,
    }
}

/// The one kind of path bitsboot ever writes: `\BITSOS2\SYS\<digits>\TRY` (R8b: markers only below SYS).
pub fn is_marker_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(SYS).and_then(|r| r.strip_prefix('\\')) else { return false };
    let mut parts = rest.split('\\');
    matches!((parts.next(), parts.next(), parts.next()), (Some(d), Some("TRY"), None) if parse_name(d).is_some())
}

pub fn path(number: u32, file: &str) -> String {
    format!("{SYS}\\{number}\\{file}")
}

/// INITRD header check (rule 5): an uncompressed cpio "newc" archive (`070701`, or `070702` with checksums).
/// A gzip or zstd initramfs is refused on purpose: the kernel unpacks only plain newc, so accepting it here
/// would trade a fallback at boot for a kernel that cannot find init.
pub fn check_initrd(head: &[u8]) -> Result<(), &'static str> {
    if head.is_empty() {
        return Err("empty file");
    }
    if head.len() < 110 {
        return Err("too short for a cpio header (under 110 bytes)");
    }
    if &head[..6] != b"070701" && &head[..6] != b"070702" {
        return Err("not a cpio newc archive (bad magic)");
    }
    Ok(())
}

pub fn reason_name(r: u32) -> &'static str {
    match r {
        gen_reason::NEWEST_NO_RULE => "NEWEST_NO_RULE",
        gen_reason::NEWEST_OK => "NEWEST_OK",
        gen_reason::NEWEST_TRY => "NEWEST_TRY",
        gen_reason::FALLBACK_AFTER_TRY => "FALLBACK_AFTER_TRY",
        gen_reason::FALLBACK_BAD_FILES => "FALLBACK_BAD_FILES",
        gen_reason::PINNED => "PINNED",
        _ => "UNKNOWN",
    }
}

pub fn flag_names(f: u32) -> String {
    let names = [
        (flag::PIN_IGNORED, "PIN_IGNORED"),
        (flag::NO_CONFIRMED_FALLBACK, "NO_CONFIRMED_FALLBACK"),
        (flag::MARKER_WRITE_FAILED, "MARKER_WRITE_FAILED"),
        (flag::UNMARKED, "UNMARKED"),
        (flag::OTHERS_SKIPPED, "OTHERS_SKIPPED"),
        (flag::SAFE_MODE, "SAFE_MODE"),
    ];
    let on: Vec<&str> = names.iter().filter(|(b, _)| f & b != 0).map(|(_, n)| *n).collect();
    if on.is_empty() { String::from("-") } else { on.join(",") }
}

#[cfg(test)]
mod tests;
