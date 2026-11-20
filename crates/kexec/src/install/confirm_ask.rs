//! VB-1 (Georg 17.11., "Ja, beides"): Georg confirms a new version himself. After the start of an unconfirmed
//! version BitsOS asks "Version N läuft. Bestätigen?" (the desktop's notice, the Gaming-PC's report page, the
//! console's `bitsupdate frage`); "Ja" writes OK for that version through the boot confirmation's own write
//! (`confirm::run` takes the answer as its third proof, `boot.confirm_proof by=georg`), "Zurück zur vorherigen"
//! pins the newest confirmed version below (the node's `back`) and restarts.
//!
//! The question stands only where the desktop proof would also count (`confirm::desktop_proof_for`: a numbered
//! version in its own slot, an update Georg started): a development build the desk installs over MCP keeps MCP
//! as its only lifeline, so nobody at the machine can make a build with a broken MCP the fallback. It needs a
//! writable boot medium (OK cannot be written otherwise) and ends the moment a proof or an answer is written.
//!
//! Only real input counts. The screens check it themselves (a physical-only reader, no MCP or virtual device,
//! armed after a short time); the node checks it once more: an answer through it counts only when a physical-only
//! reader read a real key or button press (`console::evdevfs::last_physical_press_ns`; pointer moves do not
//! count) after the watch began and at most `REAL_WITHIN_NS` before the answer. The node itself is closed to
//! processes MCP started, and bitspkgd's `update_confirm` refuses them too (`mcp_run=` of /proc/bitsos/power).
//! What stays open, and is accepted: a root process MCP did not start (directly or through bitspkgd) that
//! answers within those seconds after any real press on the machine; the press is not tied to the question's own
//! screen. The
//! kernel's own report page (keys of the VT, never a physical-only reader) answers through `answer` with its
//! own check.

use super::confirm::{self, Status};
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const SEC: u64 = 1_000_000_000;
/// How recent the real press behind an answer through the node must be: the screens answer on the press (the
/// console) or its release (the desktop's card), well inside this.
pub const REAL_WITHIN_NS: u64 = 3 * SEC;

/// The generation Georg said "Ja" to (0: none); `confirm::run` takes it at its next sample.
static ANSWERED: AtomicU32 = AtomicU32::new(0);
/// Which proof or answer wrote OK in this boot ("mcp", "desktop", "georg"; empty: none yet).
static BY: ksync::SpinLock<&'static str> = ksync::SpinLock::named("kexec.confirm_by", "");
/// When the watch began (`time::now_ns`, 0: not yet): a press before it is no answer to the question.
static WATCH_NS: AtomicU64 = AtomicU64::new(0);

/// The status as one word for user space.
pub fn state_word(s: &Status) -> &'static str {
    match s {
        Status::Unknown => "unknown",
        Status::NotNeeded(_) => "none",
        Status::Pending { .. } => "pending",
        Status::Writing { .. } => "writing",
        Status::Confirmed { .. } => "confirmed",
        Status::Replaced { .. } => "replaced",
        Status::Failed { .. } => "failed",
    }
}

/// The generation the question is about, when it stands (pure): a watch still waiting, a version in its own
/// slot, a medium that can take OK, and no "Ja" given yet.
pub fn stands(s: &Status, version: u32, writable: bool, answered: u32) -> Option<u32> {
    match *s {
        Status::Pending { generation, .. } if confirm::desktop_proof_for(version, generation) && writable && answered != generation => Some(generation),
        _ => None,
    }
}

/// Whether a real press backs an answer through the node (pure): `press_ns` is when a physical-only reader last
/// read one (0: never), `watch_ns` when the watch began (0: not yet).
pub fn real_recent(press_ns: u64, watch_ns: u64, now_ns: u64) -> bool {
    watch_ns > 0 && press_ns >= watch_ns && press_ns <= now_ns && now_ns - press_ns <= REAL_WITHIN_NS
}

/// Why an answer for `want` is refused, or Ok(()) (pure).
pub fn judge(s: &Status, version: u32, writable: bool, answered: u32, want: u32, real: bool) -> Result<(), String> {
    if !real {
        return Err(format!("no real key or click reached a physical-only reader in the last {} s", REAL_WITHIN_NS / SEC));
    }
    match stands(s, version, writable, answered) {
        Some(g) if g == want => Ok(()),
        Some(g) => Err(format!("the question is about generation {g}, not {want}")),
        None if answered == want => Err(format!("generation {want} was answered already")),
        None => Err(format!("no question stands ({})", confirm::describe(s))),
    }
}

fn writable() -> bool {
    fs_fat::stick::volume().is_some_and(|v| v.writable())
}

/// The question now: (version, generation), or None.
pub fn question() -> Option<(u32, u32)> {
    stands(&confirm::status(), bitsver::NUMBER, writable(), ANSWERED.load(Ordering::Acquire)).map(|g| (bitsver::NUMBER, g))
}

pub fn answered() -> u32 {
    ANSWERED.load(Ordering::Acquire)
}

pub fn by() -> &'static str {
    *BY.lock()
}

/// Georg's "Ja" for generation `n`. `checked`: the caller saw the real key itself (the report page); else a
/// physical-only reader must have read a real press within `REAL_WITHIN_NS` (`real_recent`).
pub fn answer(n: u32, checked: bool) -> Result<(), String> {
    let real = checked || real_recent(console::evdevfs::last_physical_press_ns(), WATCH_NS.load(Ordering::Acquire), time::now_ns());
    let r = judge(&confirm::status(), bitsver::NUMBER, writable(), ANSWERED.load(Ordering::Acquire), n, real);
    match &r {
        Ok(()) => {
            ANSWERED.store(n, Ordering::Release);
            klog::event!(Info, "kexec", "boot.confirm_answer", generation = n, via = if checked { "report_page" } else { "node" };
                "Georg said yes at the machine: OK is written at the watch's next sample");
        }
        Err(why) => klog::event!(Warn, "kexec", "boot.confirm_answer_refused", generation = n, reason = why.as_str(); "nothing changed"),
    }
    r
}

/// The watch begins (`confirm::run`): only presses from now on can back an answer.
pub(super) fn watch_began(now_ns: u64) {
    WATCH_NS.store(now_ns.max(1), Ordering::Release);
}

/// The write after Georg's "Ja" to `n` failed: the answer is void and the question stands again (pure state
/// change; `confirm::run` sets the status back to waiting).
pub(super) fn answer_lost(n: u32) {
    let _ = ANSWERED.compare_exchange(n, 0, Ordering::AcqRel, Ordering::Acquire);
}

#[cfg(test)]
pub(super) fn set_answered(n: u32) {
    ANSWERED.store(n, Ordering::Release);
}

/// Whether the watch must stop for Georg's "Ja" to `n`.
pub(super) fn taken(n: u32) -> bool {
    ANSWERED.load(Ordering::Acquire) == n
}

pub(super) fn note_by(by: &'static str) {
    *BY.lock() = by;
}

/// Where "Zurück zur vorherigen" goes: Ok(Some) the newest confirmed generation below the running one (the
/// node's `back`, `plan::back_target`), Ok(None) there is none, Err the medium could not be read within
/// `lock_ns` (an install or the confirmation's write holds it): ask again later.
pub fn back_target(lock_ns: u64) -> Result<Option<u32>, String> {
    super::system_stick::with_gens(lock_ns, |s, gens, _| {
        let running = super::system_stick::running_number(s).or_else(|| crate::boot::boot_info().and_then(|(bi, _)| bi.generation()).map(|g| g.number));
        Ok(super::plan::back_target(gens, running).ok())
    })
}
