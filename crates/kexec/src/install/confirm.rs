//! The kernel confirms its own boot (V2-PLAN §7 "Confirmation", RB-6): a kernel that bitsboot started as the one
//! try of a new generation (`GENERATION` record reason `NEWEST_TRY`) and that the firmware started (Reload
//! counter 0: a kernel brought by a Reload is not the generation's kernel) watches MCP; once MCP has answered
//! requests over `boot_confirm_s` (default 120 s: an answer at least that long after the first one), it writes
//! `OK` next to its generation and clears `TRY` and `NEW` (`super::confirm_on`), and prunes. Without that, the
//! next boot starts the newest confirmed generation (bitsboot's rule 2), so a kernel that hangs, panics or
//! comes up without its lifeline falls back by itself.
//!
//! "MCP answered" is read from a klog counter (no lock, no dependency on the MCP crate's internals): MC's
//! `mcp.answered` (authenticated requests answered; change request in the RB-6 notes) when the kernel has it,
//! else `mcp.requests` (every request MCP answered, refusals included: the server and the network work).
//!
//! UPDATE (Georg 15.11.: an update he starts himself must not depend on the remote desk): the desktop confirms
//! too (`DesktopWatch`). The confirmation's purpose is "this version works well enough to reach and use the
//! machine"; MCP proves the remote lifeline, the desktop proves the local one: a program has held the screen
//! without a break (`fbdev.takes`/`fbdev.releases`) for the whole proof time, and Georg's real input reached a
//! reader that takes physical input only (`input.physical_read`, the compositor's way of reading the devices;
//! MCP's injected keys never count) in at least `DESKTOP_INPUTS` samples spread over half of it. Whichever proof
//! is done first writes `OK`; a version that brings up neither falls back as before. Only a numbered version in
//! its own slot (what the update page and `bitsupdate` install) has the desktop proof (`desktop_proof_for`): a
//! development build the desk installs over MCP keeps MCP as its only proof, as before UPDATE.
//!
//! VB-1: Georg's own "Ja" (`confirm_ask`) is the third proof, taken at the next sample (`by=georg`), for the same
//! versions the desktop proof counts for.

use super::{Confirmed, FatStick, LOCK, confirm_on};
use alloc::format;
use alloc::string::String;
use bootinfo::gen_reason;

pub const DEFAULT_S: u64 = 120;
/// Bounds of CONFIG `boot_confirm_s=` (a shorter proof is no proof; longer than a day is a typo).
pub const MIN_S: u64 = 10;
pub const MAX_S: u64 = 86_400;
const SEC: u64 = 1_000_000_000;
const SAMPLE_NS: u64 = 2 * SEC;
/// After the proof: how often the stick is tried again when it is missing or read-only (bounded tries).
const RETRY_NS: u64 = 30 * SEC;
const TRIES: u32 = 20;

/// The counters that say "MCP answered", best first.
pub const COUNTERS: &[&str] = &["mcp.answered", "mcp.requests"];

/// UPDATE: the counters of the desktop proof (gopfb's screen holds, console's physical input handed to readers).
pub const SCREEN_TAKES: &str = "fbdev.takes";
pub const SCREEN_RELEASES: &str = "fbdev.releases";
pub const PHYSICAL_INPUT: &str = "input.physical_read";
/// Samples with new real input the desktop proof needs (one stray key at boot proves nothing).
pub const DESKTOP_INPUTS: u32 = 3;

/// The desktop proof rule (pure, see the module text): `sample` returns true once the screen was held by the same
/// hold (the takes count unchanged, more takes than releases) for `need_ns`, and real input rose in at least
/// `DESKTOP_INPUTS` samples of that hold whose first and last lie at least `need_ns / 2` apart. A release or a new
/// take starts it again; input counted before the hold began does not count.
#[derive(Clone, Copy, Debug)]
pub struct DesktopWatch {
    pub need_ns: u64,
    hold: Option<(u64, u64)>,
    last_input: Option<u64>,
    rises: u32,
    first_rise: Option<u64>,
    last_rise: Option<u64>,
    proven: bool,
    /// How often a lost or new hold started it again.
    pub restarts: u32,
}

impl DesktopWatch {
    pub fn new(need_ns: u64) -> Self {
        DesktopWatch { need_ns, hold: None, last_input: None, rises: 0, first_rise: None, last_rise: None, proven: false, restarts: 0 }
    }
    /// One sample; true once proven (and from then on). The proof is judged after the sample is taken in: a hold
    /// that ended since the last sample proves nothing.
    pub fn sample(&mut self, now_ns: u64, takes: u64, releases: u64, input: u64) -> bool {
        if self.proven {
            return true;
        }
        let held = takes > releases;
        let same = self.hold.is_some_and(|(t, _)| t == takes);
        if !held || !same {
            if self.hold.is_some() {
                self.restarts += 1;
            }
            self.hold = held.then_some((takes, now_ns));
            self.rises = 0;
            self.first_rise = None;
            self.last_rise = None;
        } else if self.last_input.is_some_and(|l| input > l) {
            self.rises += 1;
            self.first_rise.get_or_insert(now_ns);
            self.last_rise = Some(now_ns);
        }
        self.last_input = Some(input);
        self.proven = self.done(now_ns);
        self.proven
    }
    fn done(&self, now_ns: u64) -> bool {
        let (Some((_, since)), Some(f), Some(l)) = (self.hold, self.first_rise, self.last_rise) else { return false };
        now_ns.saturating_sub(since) >= self.need_ns && self.rises >= DESKTOP_INPUTS && l.saturating_sub(f) >= self.need_ns / 2
    }
    /// How long the screen has been held so far without a break.
    pub fn held_ns(&self, now_ns: u64) -> u64 {
        self.hold.map_or(0, |(_, s)| now_ns.saturating_sub(s))
    }
    pub fn rises(&self) -> u32 {
        self.rises
    }
}

/// Whether the desktop proof may confirm generation `generation` run by a kernel of version `version` (pure). A
/// numbered version in its own slot: an update Georg starts, which must not depend on the remote desk. Not a
/// development build (`version` 0): the desk installs those over MCP to try them, and their only lifeline is MCP;
/// one whose MCP or network is broken must fall back by itself even while Georg uses the desktop.
pub fn desktop_proof_for(version: u32, generation: u32) -> bool {
    version != 0 && version == generation
}

/// Both proofs of one boot (pure): `sample` returns which one is done first (`"mcp"` or `"desktop"`).
#[derive(Clone, Copy, Debug)]
pub struct Proofs {
    pub mcp: Watch,
    /// None: this kernel has no desktop proof (`desktop_proof_for`).
    pub desktop: Option<DesktopWatch>,
}

impl Proofs {
    pub fn new(need_ns: u64, version: u32, generation: u32) -> Self {
        Proofs { mcp: Watch::new(need_ns), desktop: desktop_proof_for(version, generation).then(|| DesktopWatch::new(need_ns)) }
    }
    pub fn sample(&mut self, now_ns: u64, answers: u64, takes: u64, releases: u64, input: u64) -> Option<&'static str> {
        if self.mcp.sample(now_ns, answers) {
            return Some("mcp");
        }
        let d = self.desktop.as_mut()?;
        d.sample(now_ns, takes, releases, input).then_some("desktop")
    }
}

/// The desktop proof's progress for the update page: (screen held s, samples with real input).
static DESKTOP: ksync::SpinLock<(u64, u32)> = ksync::SpinLock::named("kexec.confirm_desk", (0, 0));

pub fn desktop_progress() -> (u64, u32) {
    *DESKTOP.lock()
}

/// Longest silence the proof tolerates: a quarter of the proof time, at least three samples, at most 30 s. A
/// longer pause between two answers starts the proof again (an answer at boot and a stray one two minutes later
/// prove nothing about the time between).
const GAP_MIN_NS: u64 = 3 * SAMPLE_NS;
const GAP_MAX_NS: u64 = 30 * SEC;

pub fn gap_for(need_ns: u64) -> u64 {
    (need_ns / 4).clamp(GAP_MIN_NS, GAP_MAX_NS)
}

/// The proof rule (pure): MCP answered continuously over `need_ns`, sampled. `sample` returns true once an answer
/// was seen at least `need_ns` after the first one of an unbroken run, where unbroken means no two answers (and
/// no answer and the current sample) lie more than `gap_ns` apart.
#[derive(Clone, Copy, Debug)]
pub struct Watch {
    pub need_ns: u64,
    pub gap_ns: u64,
    last: u64,
    first: Option<u64>,
    last_rise: Option<u64>,
    /// How often a silence longer than `gap_ns` restarted the proof.
    pub restarts: u32,
}

impl Watch {
    /// Answers counted before the watch began count as one rise at the first sample.
    pub fn new(need_ns: u64) -> Self {
        Watch { need_ns, gap_ns: gap_for(need_ns), last: 0, first: None, last_rise: None, restarts: 0 }
    }
    pub fn sample(&mut self, now_ns: u64, count: u64) -> bool {
        if self.done() {
            return true;
        }
        if let Some(l) = self.last_rise {
            if now_ns.saturating_sub(l) > self.gap_ns {
                self.first = None;
                self.last_rise = None;
                self.restarts += 1;
            }
        }
        if count > self.last {
            self.first.get_or_insert(now_ns);
            self.last_rise = Some(now_ns);
        }
        self.last = self.last.max(count);
        self.done()
    }
    pub fn done(&self) -> bool {
        matches!((self.first, self.last_rise), (Some(f), Some(l)) if l.saturating_sub(f) >= self.need_ns)
    }
    /// How long answers have been seen so far without a break.
    pub fn covered_ns(&self) -> u64 {
        match (self.first, self.last_rise) {
            (Some(f), Some(l)) => l.saturating_sub(f),
            _ => 0,
        }
    }
}

/// What this boot's confirmation is doing (for `reload_info`, `reload`'s answer and `install_kernel`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not decided yet (before the init call).
    Unknown,
    /// Nothing to confirm, and why.
    NotNeeded(String),
    Pending { generation: u32, covered_s: u64, need_s: u64, counter: &'static str },
    Writing { generation: u32 },
    Confirmed { generation: u32 },
    /// The generation was replaced by an install in this boot.
    Replaced { generation: u32 },
    Failed { generation: u32, why: String },
}

static STATUS: ksync::SpinLock<Status> = ksync::SpinLock::named("kexec.confirm", Status::Unknown);

pub fn status() -> Status {
    STATUS.lock().clone()
}

fn set(s: Status) {
    *STATUS.lock() = s;
}

/// The generation this kernel still has to confirm, if any (`reload` warns: a Reload now leaves it unconfirmed).
pub fn pending() -> Option<(u32, u64, u64)> {
    match status() {
        Status::Pending { generation, covered_s, need_s, .. } => Some((generation, covered_s, need_s)),
        Status::Writing { generation } => Some((generation, 0, 0)),
        _ => None,
    }
}

pub fn describe(s: &Status) -> String {
    match s {
        Status::Unknown => String::from("not decided yet"),
        Status::NotNeeded(why) => format!("nothing to confirm: {why}"),
        Status::Pending { generation, covered_s, need_s, counter } => format!(
            "generation {generation} waits for its confirmation: MCP answers seen over {covered_s} s of {need_s} s without a pause over {} s (counter {counter}); a reboot before that falls back",
            gap_for(*need_s * SEC) / SEC
        ),
        Status::Writing { generation } => format!("generation {generation}: writing OK to the stick"),
        Status::Confirmed { generation } => format!("generation {generation} confirmed (OK) in this boot"),
        Status::Replaced { generation } => format!("generation {generation} was replaced by an install in this boot; nothing confirmed"),
        Status::Failed { generation, why } => format!("generation {generation} could not be confirmed: {why}"),
    }
}

/// Which boots confirm themselves (pure): only the firmware-started one try of a generation.
pub fn needs_confirmation(reload_counter: u32, record: Option<bootinfo::Generation>) -> Result<u32, String> {
    if reload_counter > 0 {
        return Err(format!(
            "this kernel came by Reload {reload_counter}; it is not the stick generation's kernel (that one is confirmed only by itself)"
        ));
    }
    let Some(g) = record else { return Err(String::from("bitsboot handed no GENERATION record")) };
    if g.reason != gen_reason::NEWEST_TRY {
        return Err(format!("generation {} started as {}", g.number, bitsboot_pure::gens::reason_name(g.reason)));
    }
    Ok(g.number)
}

fn counter() -> Option<(&'static str, u64)> {
    for want in COUNTERS {
        if let Some((n, v)) = klog::metrics::snapshot().find(|(n, _)| n == want) {
            return Some((n, v));
        }
    }
    None
}

/// One counter's value (0 when this kernel has none: the desktop proof then never completes).
fn value(name: &str) -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == name).map_or(0, |(_, v)| v)
}

fn need_ns() -> u64 {
    let s = crate::boot::config("boot_confirm_s").and_then(|v| v.parse::<u64>().ok()).unwrap_or(DEFAULT_S);
    s.clamp(MIN_S, MAX_S) * SEC
}

/// One try on the stick: Ok(true) = done (confirmed, already OK, or replaced). Holds Reload's busy flag for the
/// whole write: a Reload stopping the CPUs (and quiescing xHCI) under a USB write in flight is no cut point the
/// stick rules cover, and an install_kernel then waits for its turn instead of for `LOCK`.
fn write(n: u32) -> Result<bool, String> {
    let Some(_busy) = crate::tool::Busy::take() else {
        return Err(String::from("a Reload or an install_kernel is in progress"));
    };
    let Some(vol) = fs_fat::stick::volume() else {
        return Err(format!("no boot stick mounted ({})", fs_fat::stick::last_reason().unwrap_or("not seen yet")));
    };
    if !vol.writable() {
        return Err(format!("the boot stick is read-only ({})", fs_fat::stick::last_reason().unwrap_or("?")));
    }
    let _l = LOCK.lock_timeout(60 * SEC).ok_or("an install holds the stick for over a minute")?;
    let r = fs_fat::stick::with_sys(|s| confirm_on(&FatStick(s), n)).map_err(super::errno_name)?;
    match r? {
        Confirmed::Now { pruned } => {
            klog::event!(Info, "kexec", "boot.confirmed", generation = n, pruned = format!("{pruned:?}");
                "OK written, TRY and NEW cleared: this generation is the fallback from now on");
            set(Status::Confirmed { generation: n });
        }
        Confirmed::Already { pruned } => {
            klog::event!(Info, "kexec", "boot.confirmed", generation = n, already = true, pruned = format!("{pruned:?}"));
            set(Status::Confirmed { generation: n });
        }
        Confirmed::Missing => {
            klog::event!(Warn, "kexec", "boot.confirm_skipped", generation = n, reason = "replaced";
                "the generation this kernel came from was replaced by an install in this boot");
            set(Status::Replaced { generation: n });
        }
    }
    Ok(true)
}

fn run(n: u32, need: u64) {
    super::confirm_ask::watch_began(time::now_ns());
    let mut p = Proofs::new(need, bitsver::NUMBER, n);
    if p.desktop.is_none() {
        klog::event!(Info, "kexec", "boot.confirm_mcp_only", generation = n, version = bitsver::NUMBER;
            "a development build (or a version outside its own slot): only MCP confirms it");
    }
    // VB-1: a write that fails after Georg's "Ja" voids the answer and the watch waits again (the question comes
    // back); a write after a proof that fails ends the watch as before.
    while wait(&mut p, n, need) {
        match write_all(n) {
            Written::Done => return,
            Written::Failed(why) if super::confirm_ask::taken(n) => {
                super::confirm_ask::answer_lost(n);
                let name = counter().map_or("", |c| c.0);
                {
                    let mut s = STATUS.lock();
                    if !matches!(*s, Status::Writing { .. }) {
                        return;
                    }
                    *s = Status::Pending { generation: n, covered_s: p.mcp.covered_ns() / SEC, need_s: need / SEC, counter: name };
                }
                klog::event!(Warn, "kexec", "boot.confirm_answer_lost", generation = n, reason = why.as_str();
                    "OK could not be written after Georg's Ja: the question stands again and the watch goes on");
            }
            Written::Failed(why) => {
                klog::event!(Error, "kexec", "boot.confirm_failed", generation = n, reason = why.as_str();
                    "never confirmed: the next boot starts the newest confirmed generation");
                set(Status::Failed { generation: n, why });
                return;
            }
        }
    }
}

enum Written {
    /// Confirmed, already OK, or replaced meanwhile: the watch is over.
    Done,
    Failed(String),
}

/// The watch until a proof or Georg's answer: true when OK is to be written (the status is Writing then).
fn wait(p: &mut Proofs, n: u32, need: u64) -> bool {
    loop {
        sched::sleep_ns(SAMPLE_NS);
        let Some((name, count)) = counter() else {
            set(Status::Failed { generation: n, why: String::from("no MCP counter in this kernel") });
            klog::event!(Error, "kexec", "boot.confirm_failed", generation = n, reason = "no MCP counter");
            return false;
        };
        let now = time::now_ns();
        let restarts = p.mcp.restarts;
        let input = value(PHYSICAL_INPUT);
        let by = p.sample(now, count, value(SCREEN_TAKES), value(SCREEN_RELEASES), input);
        let w = &p.mcp;
        if by.is_none() && super::confirm_ask::taken(n) {
            klog::event!(Info, "kexec", "boot.confirm_proof", generation = n, by = "georg", covered_s = w.covered_ns() / SEC;
                "Georg said yes at the machine with a real key or click");
            super::confirm_ask::note_by("georg");
            break;
        }
        if w.restarts != restarts {
            klog::event!(Warn, "kexec", "boot.confirm_gap", generation = n, gap_s = w.gap_ns / SEC, answers = count;
                "MCP was silent longer than the proof allows: the proof starts again");
        }
        if by == Some("mcp") {
            klog::event!(Info, "kexec", "boot.confirm_proof", generation = n, by = "mcp", covered_s = w.covered_ns() / SEC, counter = name, answers = count;
                "MCP answered over the whole proof time");
            super::confirm_ask::note_by("mcp");
            break;
        }
        if let Some(d) = &p.desktop {
            *DESKTOP.lock() = (d.held_ns(now) / SEC, d.rises());
            if by == Some("desktop") {
                klog::event!(Info, "kexec", "boot.confirm_proof", generation = n, by = "desktop", held_s = d.held_ns(now) / SEC, input_samples = d.rises(), input = input;
                    "the desktop held the screen over the whole proof time and Georg's real input reached it");
                super::confirm_ask::note_by("desktop");
                break;
            }
        }
        // Only a Pending status is updated: an install that replaced the generation ends the watch.
        let mut s = STATUS.lock();
        if !matches!(*s, Status::Pending { .. }) {
            return false;
        }
        *s = Status::Pending { generation: n, covered_s: w.covered_ns() / SEC, need_s: need / SEC, counter: name };
    }
    let mut s = STATUS.lock();
    if !matches!(*s, Status::Pending { .. }) {
        return false;
    }
    *s = Status::Writing { generation: n };
    true
}

/// OK for `n`, with `TRIES` tries.
fn write_all(n: u32) -> Written {
    for t in 1..=TRIES {
        // An install in this boot may have replaced the generation while this waited (note_replaced).
        if !matches!(status(), Status::Writing { .. }) {
            return Written::Done;
        }
        match write(n) {
            Ok(_) => return Written::Done,
            Err(why) => {
                klog::event!(Warn, "kexec", "boot.confirm_retry", generation = n, attempt = t, reason = why.as_str());
                if t == TRIES {
                    return Written::Failed(why);
                }
                sched::sleep_ns(RETRY_NS);
            }
        }
    }
    Written::Done
}

/// An install in this boot removed generation `n` (the watch ends without writing anything).
pub fn note_replaced(n: u32) {
    let mut s = STATUS.lock();
    if let Some(next) = after_replace(&s, n) {
        *s = next;
    }
}

/// The status after an install removed generation `n` (pure): a watch or a write still waiting for `n` ends.
pub fn after_replace(s: &Status, n: u32) -> Option<Status> {
    matches!(*s, Status::Pending { generation, .. } | Status::Writing { generation } if generation == n)
        .then_some(Status::Replaced { generation: n })
}

/// The initramfs entry that makes a generation fail on purpose (RB-6's fallback round): a test bundle carries it,
/// the kernel panics before MCP starts, and with `panic=reboot` (the hardware default) the machine resets by
/// itself, so bitsboot's fallback can be proven without anyone at the machine. Only a bundle built for that test
/// has it; the kernel bytes are the ordinary ones.
pub const TEST_PANIC_ENTRY: &[u8] = b"etc/bitsos/test-panic-before-mcp";

/// Whether the cpio newc archives read through `read(offset, buf) -> ok` (`size` bytes; concatenated, zero padding between) have an entry named
/// `name` (headers and names only are read; a malformed archive is "no").
pub fn has_entry(read: &mut dyn FnMut(u64, &mut [u8]) -> bool, size: u64, name: &[u8]) -> bool {
    let hex = |b: &[u8]| core::str::from_utf8(b).ok().and_then(|s| u64::from_str_radix(s, 16).ok());
    let mut off = 0u64;
    let mut head = [0u8; 110];
    let mut nbuf = alloc::vec![0u8; name.len() + 1];
    let mut zeros = 0u64;
    while off + 110 <= size {
        if !read(off, &mut head) {
            return false;
        }
        if head[..4] == [0; 4] {
            // Zero padding between concatenated archives (a bound: real padding is under a block).
            zeros += 4;
            if zeros > 1 << 20 {
                return false;
            }
            off += 4;
            continue;
        }
        zeros = 0;
        if &head[..6] != b"070701" && &head[..6] != b"070702" {
            return false;
        }
        let (Some(fsize), Some(nlen)) = (hex(&head[54..62]), hex(&head[94..102])) else { return false };
        if nlen as usize == name.len() + 1 && off + 110 + nlen <= size {
            if read(off + 110, &mut nbuf) && &nbuf[..name.len()] == name {
                return true;
            }
        }
        let body = (off + 110 + nlen + 3) & !3;
        off = (body + fsize + 3) & !3;
    }
    false
}

/// RB-6 test hook (see `TEST_PANIC_ENTRY`): runs at the start of the Late init calls, before MCP.
fn test_panic_hook(bi: &bootinfo::BootInfo<'static>) {
    let Some(m) = bi.modules().find(|m| m.kind == bootinfo::module_kind::INITRAMFS) else { return };
    if !crate::boot::module_readable(&m) || !has_entry(&mut |off, b| hal::kexec_stub::read_phys(m.phys + off, b), m.size, TEST_PANIC_ENTRY) {
        return;
    }
    klog::event!(Error, "kexec", "boot.test_panic", generation = bi.generation().map(|g| g.number).unwrap_or(0);
        "this generation's initramfs asks for a panic before MCP (RB-6 fallback test)");
    // klogd prints serial and the screen console every 10 ms: give it the line before the panic cuts printing.
    sched::sleep_ns(200_000_000);
    klog::drain_deferred(usize::MAX);
    klog::flush_serial(usize::MAX);
    panic!("RB-6 fallback test: the initramfs holds /etc/bitsos/test-panic-before-mcp");
}

fn start() -> Result<(), &'static str> {
    if let Some((bi, _)) = crate::boot::boot_info() {
        test_panic_hook(&bi);
    }
    let Some((bi, _)) = crate::boot::boot_info() else {
        set(Status::NotNeeded(String::from("no BootInfo handed to kexec")));
        return Ok(());
    };
    let counter = bi.reload().map(|r| r.counter).unwrap_or(0);
    let n = match needs_confirmation(counter, bi.generation()) {
        Ok(n) => n,
        Err(why) => {
            klog::event!(Info, "kexec", "boot.confirm_not_needed", reason = why.as_str());
            set(Status::NotNeeded(why));
            return Ok(());
        }
    };
    let need = need_ns();
    set(Status::Pending { generation: n, covered_s: 0, need_s: need / SEC, counter: COUNTERS[COUNTERS.len() - 1] });
    klog::event!(Info, "kexec", "boot.confirm_pending", generation = n, need_s = need / SEC;
        "this generation's one try: OK is written once MCP has answered over the proof time");
    sched::spawn("boot-confirm", sched::Class::Background, move || run(n, need))
        .map(|_| ())
        .map_err(|_| "boot-confirm thread not started")
}

kreg::initcall!(kreg::Level::Late, "kexec.boot_confirm", start);
