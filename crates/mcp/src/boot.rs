//! What the kernel was started with (BootInfo), kept for `hw_info` and `reboot`: the kernel version, the system
//! generation and why bitsboot chose it, the Reload counter, the boot id. The kernel hands it over once with
//! `set_boot_info` before `start_kernel`; without it the tools say "unknown" instead of guessing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bitsboot_pure::gens::{self, Choice, GenDir, Pin};
use crate::bounded::{Bounded, Single};
use bootinfo::{Generation, gen_reason};
use ksync::Once;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootFacts {
    pub kernel_version: &'static str,
    pub generation: Option<Generation>,
    /// 0 = started by the firmware, n = the n-th Reload since.
    pub reload: Option<u32>,
    pub boot_id: Option<[u8; 16]>,
}

static FACTS: Once<BootFacts> = Once::new();
/// RB-11: the running kernel's id (FNV-1a 64 of its KERNEL file) as the Reload that started it handed it on; 0 =
/// not known (a firmware start, or a Reload by an older kernel). Set by kexec's crash init.
static RUNNING_KERNEL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Records the running kernel's id (kexec, RB-11 MC CR 2); hw_info and reboot name it after a Reload, because
/// the GENERATION record still names the generation bitsboot started then.
pub fn set_running_kernel_id(id: u64) {
    RUNNING_KERNEL.store(id, core::sync::atomic::Ordering::Relaxed);
}

/// `; kernel id <016x>` after a Reload when the id is known, else empty.
pub(crate) fn kernel_id_note(reload: Option<u32>) -> String {
    let id = RUNNING_KERNEL.load(core::sync::atomic::Ordering::Relaxed);
    if reload.unwrap_or(0) > 0 && id != 0 { format!("; kernel id {id:016x}") } else { String::new() }
}
/// CONFIG `profile=` and `profile_reason=` (BT-5), recorded when the kernel server starts.
static PROFILE: Once<(String, String)> = Once::new();

pub(crate) fn set_profile(config: &dyn Fn(&str) -> Option<String>) {
    PROFILE.get_or_init(|| {
        (config("profile").unwrap_or_else(|| String::from("normal")), config("profile_reason").unwrap_or_else(|| String::from("-")))
    });
}

/// The kernel calls this once with its BootInfo and its own version (`env!("CARGO_PKG_VERSION")` of the
/// kernel crate); later calls change nothing.
pub fn set_boot_info(bi: &bootinfo::BootInfo<'_>, kernel_version: &'static str) {
    set_facts(BootFacts { kernel_version, generation: bi.generation(), reload: bi.reload().map(|r| r.counter), boot_id: bi.boot_id() });
}

pub(crate) fn set_facts(f: BootFacts) {
    let got = *FACTS.get_or_init(|| f);
    if got != f {
        klog::event!(Warn, "mcp", "boot_info.again"; "boot facts were set already; kept");
    }
}

pub fn facts() -> Option<BootFacts> {
    FACTS.get().copied()
}

pub fn reason_name(r: u32) -> &'static str {
    match r {
        gen_reason::NEWEST_NO_RULE => "newest (loader without the marker rule)",
        gen_reason::NEWEST_OK => "newest, confirmed (OK)",
        gen_reason::NEWEST_TRY => "newest, on trial (TRY)",
        gen_reason::FALLBACK_AFTER_TRY => "fallback: the newest one was never confirmed",
        gen_reason::FALLBACK_BAD_FILES => "fallback: the newest one's files were broken",
        gen_reason::PINNED => "pinned (SYS\\PIN)",
        _ => "unknown reason",
    }
}

/// Which system generation bitsboot will start next (V2-PLAN §7 marker rule), judged from why this one was
/// started; the kernel cannot see the stick yet, so a generation installed meanwhile is named as a caveat.
/// The first word is the plan's shorthand: `KERNEL` (the newest) or `KERNEL.OLD` (the newest confirmed older
/// one, the fallback).
pub fn next_start(g: Option<Generation>) -> String {
    let Some(g) = g else {
        return String::from("unknown: the kernel did not hand over its BootInfo generation record (bitsboot's choice stays the same as at this start)");
    };
    let n = g.number;
    match g.reason {
        gen_reason::NEWEST_NO_RULE => format!("KERNEL: generation {n} again (this loader always starts the highest-numbered generation)"),
        gen_reason::NEWEST_OK => format!("KERNEL: generation {n} again (confirmed), unless a newer one was installed since"),
        gen_reason::NEWEST_TRY => format!(
            "KERNEL.OLD: the newest confirmed generation older than {n}, because generation {n} is still on trial (TRY) and nobody confirmed it yet; Reload brings {n} back"
        ),
        gen_reason::FALLBACK_AFTER_TRY | gen_reason::FALLBACK_BAD_FILES => format!(
            "KERNEL.OLD: generation {n} again (the fallback), while generation {} stays unconfirmed or broken",
            g.skipped
        ),
        gen_reason::PINNED => format!("generation {n} again (pinned), until the pin is removed"),
        r => format!("unknown: generation {n} was started for reason {r}, which this kernel does not know"),
    }
}

/// What bitsboot starts next on a stick with these generation folders and PIN (pure; host-tested): the file
/// checks are assumed to pass (bitsboot falls back at once when they do not). The first word follows bitsboot's
/// reason, not the number: `KERNEL` (the newest folder: NEWEST_*), `KERNEL.OLD` (the fallback: FALLBACK_*) or
/// `PINNED` (SYS\PIN), so a pin to an older generation is not mistaken for the fallback.
pub fn next_from_listing(gens: &[GenDir], pin: Option<u32>) -> String {
    match gens::choose(gens, pin.map_or(Pin::None, Pin::Number), &[], &[]) {
        Choice::Start { record, why, .. } => format!(
            "{}: generation {} ({}: {why}), as bitsboot's rule reads the boot stick now",
            label(record.reason),
            record.number,
            gens::reason_name(record.reason)
        ),
        Choice::Refuse(t) => format!("nothing: bitsboot would refuse the stick as it is now ({t})"),
    }
}
/// The running generation's reason as `reboot` and `hw_info` name it (RB-10 CR 1): bitsboot's record says TRY
/// for the whole boot, so a generation this boot confirmed (its folder carries OK on the stick now) is named as
/// confirmed; otherwise bitsboot's reason. The stick is listed only in that one case.
pub fn running_reason(g: &Generation) -> &'static str {
    let ok = g.reason == gen_reason::NEWEST_TRY
        && stick_listing().is_ok_and(|(gens, _)| gens.iter().any(|d| d.number == g.number && d.ok));
    running_reason_with(g, ok)
}

/// `running_reason` for a caller that listed the stick itself (RB-11, `system_list`): `ok_on_stick`, the
/// running generation's folder carries OK.
pub fn running_reason_with(g: &Generation, ok_on_stick: bool) -> &'static str {
    if g.reason == gen_reason::NEWEST_TRY && ok_on_stick {
        return "newest, confirmed (OK) in this boot";
    }
    reason_name(g.reason)
}

fn label(reason: u32) -> &'static str {
    match reason {
        gen_reason::FALLBACK_AFTER_TRY | gen_reason::FALLBACK_BAD_FILES => "KERNEL.OLD",
        gen_reason::PINNED => "PINNED",
        _ => "KERNEL",
    }
}

/// How long `hw_info` and `reboot` wait for the stick's listing (B4): a stick that hangs or is busy with an
/// install's writes must not hold an MCP call; the answer then names the guess from this start's record.
const LIST_DEADLINE_NS: u64 = 2_000_000_000;
/// One listing at a time: a stuck stick holds one helper thread, not one per `hw_info` poll.
static LISTING: Single = Single::new();

/// Why the stick could not be listed (the answer says it: absent and slow are told apart).
enum Unlisted {
    NoStick,
    Failed(i64),
    Slow,
    NoThread,
}

/// The boot stick's generations and PIN as bitsboot sees them, read in a helper thread with a deadline.
fn stick_listing() -> Result<(Vec<GenDir>, Option<u32>), Unlisted> {
    if fs_fat::stick::volume().is_none() {
        return Err(Unlisted::NoStick);
    }
    let r = LISTING.run("mcp-stick-list", LIST_DEADLINE_NS, || {
        fs_fat::stick::with_sys(|s| -> Result<_, i64> {
            let gens = s
                .generations()?
                .iter()
                .map(|g| {
                    let has = |n: &str| g.files.iter().any(|(f, _)| f == n);
                    GenDir { number: g.n, kernel: has("KERNEL"), initrd: has("INITRD"), new: g.new, tried: g.try_, ok: g.ok }
                })
                .collect::<Vec<_>>();
            Ok((gens, s.pin()?))
        })
    });
    let why = match r {
        Bounded::Done(Ok(Ok(l))) => return Ok(l),
        Bounded::Done(Ok(Err(e))) => Unlisted::Failed(e),
        Bounded::Done(Err(_)) => Unlisted::NoStick,
        Bounded::TimedOut | Bounded::Busy => Unlisted::Slow,
        Bounded::NoThread => Unlisted::NoThread,
    };
    match why {
        Unlisted::Failed(e) => klog::event!(Warn, "mcp", "next_start.stick_unlisted", errno = e; "named from this start's record instead"),
        Unlisted::Slow => klog::event!(Warn, "mcp", "next_start.stick_slow", deadline_ms = LIST_DEADLINE_NS / 1_000_000;
            "the stick did not list within the deadline (busy or hung): named from this start's record instead"),
        Unlisted::NoThread => klog::event!(Warn, "mcp", "next_start.no_thread"; "named from this start's record instead"),
        Unlisted::NoStick => {}
    }
    Err(why)
}

/// MC-4b: what the next start brings: read from the boot stick when it is mounted (bitsboot's own rule on the
/// generations as they are now, so an install since this start counts), else guessed from this start's record,
/// with the reason the stick was not read. During an install the listing shows the half-written generation as
/// it is (bitsboot would fall back from it if the machine restarted that moment).
pub fn next_start_now(g: Option<Generation>) -> String {
    match stick_listing() {
        Ok((gens, pin)) => next_from_listing(&gens, pin),
        Err(why) => {
            let note = match why {
                Unlisted::NoStick => String::from("no boot stick mounted"),
                Unlisted::Failed(e) => format!("the boot stick could not be listed (errno {e})"),
                Unlisted::Slow => format!("the boot stick did not answer within {} s (busy or hung)", LIST_DEADLINE_NS / 1_000_000_000),
                Unlisted::NoThread => String::from("no thread to read the boot stick"),
            };
            format!("{} [{note}: guessed from this start's record]", next_start(g))
        }
    }
}

/// MCP's own counters: `answered` counts only requests that passed the token and Origin checks (RB-6's boot
/// confirmation proof), `requests` every request, refusals included; a peer without the token moves only the
/// second one (checked by `echt.mjs` and `70-install.sh`).
fn counters() -> String {
    let get = |name: &str| klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |(_, v)| v);
    format!("mcp requests={} answered={}", get("mcp.requests"), get("mcp.answered"))
}

/// The `kernel` section of `hw_info`.
pub fn lines(uptime_ns: u64) -> alloc::vec::Vec<String> {
    let mut out = alloc::vec::Vec::new();
    match facts() {
        Some(f) => {
            out.push(format!("kernel version={} uptime_s={}", f.kernel_version, uptime_ns / 1_000_000_000));
            match f.generation {
                Some(g) => out.push(format!(
                    "generation {} reason={} ({}{}) skipped={} flags={:#x}",
                    g.number,
                    g.reason,
                    running_reason(&g),
                    kernel_id_note(f.reload),
                    g.skipped,
                    g.flags
                )),
                None => out.push(String::from("generation unknown (no generation record in BootInfo)")),
            }
            if let Some((p, why)) = PROFILE.get() {
                out.push(format!("profile={p} reason={why}"));
            }
            out.push(format!("reload_counter={}", f.reload.map(|r| format!("{r}")).unwrap_or_else(|| String::from("unknown"))));
            if let Some(id) = f.boot_id {
                let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                out.push(format!("boot_id={hex}"));
            }
            out.push(format!("next start: {}", next_start_now(f.generation)));
            out.push(counters());
        }
        None => {
            out.push(format!("kernel version=unknown (mcp::set_boot_info not called) mcp={} uptime_s={}", env!("CARGO_PKG_VERSION"), uptime_ns / 1_000_000_000));
            out.push(format!("next start: {}", next_start_now(None)));
            out.push(counters());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(number: u32, reason: u32, skipped: u32) -> Option<Generation> {
        Some(Generation { number, reason, skipped, flags: 0 })
    }

    #[test]
    fn next_start_follows_the_marker_rule() {
        assert!(next_start(g(4, gen_reason::NEWEST_OK, 0)).starts_with("KERNEL: generation 4 again"));
        assert!(next_start(g(4, gen_reason::NEWEST_NO_RULE, 0)).starts_with("KERNEL: generation 4"));
        let t = next_start(g(5, gen_reason::NEWEST_TRY, 0));
        assert!(t.starts_with("KERNEL.OLD") && t.contains("generation 5 is still on trial"), "{t}");
        let f = next_start(g(3, gen_reason::FALLBACK_AFTER_TRY, 5));
        assert!(f.starts_with("KERNEL.OLD: generation 3 again") && f.contains("generation 5"), "{f}");
        assert!(next_start(g(2, gen_reason::PINNED, 0)).contains("pinned"));
        assert!(next_start(g(2, 99, 0)).starts_with("unknown"));
        assert!(next_start(None).starts_with("unknown"));
    }
}
