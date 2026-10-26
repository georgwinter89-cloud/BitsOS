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
    let mut w = Watch::new(need);
    loop {
        sched::sleep_ns(SAMPLE_NS);
        let Some((name, count)) = counter() else {
            set(Status::Failed { generation: n, why: String::from("no MCP counter in this kernel") });
            klog::event!(Error, "kexec", "boot.confirm_failed", generation = n, reason = "no MCP counter");
            return;
        };
        let restarts = w.restarts;
        let proven = w.sample(time::now_ns(), count);
        if w.restarts != restarts {
            klog::event!(Warn, "kexec", "boot.confirm_gap", generation = n, gap_s = w.gap_ns / SEC, answers = count;
                "MCP was silent longer than the proof allows: the proof starts again");
        }
        if proven {
            klog::event!(Info, "kexec", "boot.confirm_proof", generation = n, covered_s = w.covered_ns() / SEC, counter = name, answers = count;
                "MCP answered over the whole proof time");
            break;
        }
        // Only a Pending status is updated: an install that replaced the generation ends the watch.
        let mut s = STATUS.lock();
        if !matches!(*s, Status::Pending { .. }) {
            return;
        }
        *s = Status::Pending { generation: n, covered_s: w.covered_ns() / SEC, need_s: need / SEC, counter: name };
    }
    {
        let mut s = STATUS.lock();
        if !matches!(*s, Status::Pending { .. }) {
            return;
        }
        *s = Status::Writing { generation: n };
    }
    for t in 1..=TRIES {
        // An install in this boot may have replaced the generation while this waited (note_replaced).
        if !matches!(status(), Status::Writing { .. }) {
            return;
        }
        match write(n) {
            Ok(_) => return,
            Err(why) => {
                klog::event!(Warn, "kexec", "boot.confirm_retry", generation = n, attempt = t, reason = why.as_str());
                if t == TRIES {
                    klog::event!(Error, "kexec", "boot.confirm_failed", generation = n, reason = why.as_str();
                        "never confirmed: the next boot starts the newest confirmed generation");
                    set(Status::Failed { generation: n, why });
                    return;
                }
                sched::sleep_ns(RETRY_NS);
            }
        }
    }
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
