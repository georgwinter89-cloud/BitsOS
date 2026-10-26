//! RB-10: what the new kernel learns from the boot stick as it is at the jump, not as it was at boot. A Reload
//! reads `\BITSOS2\CONFIG.TXT` again (settingsd's keys written since the boot reach the new kernel) and the
//! running generation's markers (a generation started on trial and confirmed in this boot is handed on as
//! confirmed, so `reboot` and `hw_info` stop calling it "on trial"). Runs after the write-back and before the
//! other CPUs stop (`tool::go_on_boot_cpu`); the stick is read in a helper thread with a deadline (B4).
//!
//! Which text wins (pure, [`choose_config`]): the file as it is, except
//! - the keys bitsboot or only Georg's own edit decide (R8a's guarded keys, the boot `profile`): a change of
//!   them since the boot takes effect at the next firmware start, so the boot copy's lines for them are put
//!   back (the file's lines for them go, the boot copy's go to the end: last line wins);
//! - a safe-mode boot (bitsboot wrapped the file in its profile lines and nothing writes the stick then), a
//!   missing, unreadable or too large file, a stick that does not answer in time: the boot copy, logged.

use crate::handover::{self, BuildError};
use crate::stage::Staged;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use bootinfo::{Generation, gen_reason};
use core::sync::atomic::{AtomicBool, Ordering};

/// Keys a Reload never changes: R8a's guarded keys and bitsboot's profile choice.
pub const KEPT_KEYS: &[&str] = &["nvme_write", "stick_write", "usb_msd_write", "profile", "profile_reason"];

/// How long the jump waits for the stick (a hung stick must not hold the Reload: B4), and, at the jump, at most
/// as long again for an earlier read still running.
pub const STICK_DEADLINE_NS: u64 = 2_000_000_000;

/// The stick's CONFIG.TXT as the helper read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileNow {
    Text(Vec<u8>),
    Missing,
    /// errno (EFBIG: larger than the BootInfo's KEEP block can hold; EAGAIN: no helper thread).
    Failed(i64),
    /// This read's helper did not answer within the deadline (it is still running).
    Slow,
    /// An earlier read's helper still runs (a stick that hangs): not read again.
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The file is byte for byte the boot copy.
    Unchanged,
    /// The file as it is.
    Fresh,
    /// The file, with these keys as at boot.
    KeptKeys(Vec<&'static str>),
    /// Boot copy: bitsboot started this machine in safe mode.
    SafeMode,
    Missing,
    Failed(i64),
    Slow,
    Busy,
}

impl Outcome {
    pub fn name(&self) -> &'static str {
        match self {
            Outcome::Unchanged => "unchanged",
            Outcome::Fresh => "stick",
            Outcome::KeptKeys(_) => "stick_kept_keys",
            Outcome::SafeMode => "boot_copy_safe_mode",
            Outcome::Missing => "boot_copy_missing",
            Outcome::Failed(_) => "boot_copy_unreadable",
            Outcome::Slow => "boot_copy_slow",
            Outcome::Busy => "boot_copy_busy",
        }
    }
    /// The boot copy is handed on although the stick may hold something newer.
    pub fn fell_back(&self) -> bool {
        matches!(self, Outcome::Missing | Outcome::Failed(_) | Outcome::Slow | Outcome::Busy)
    }
}

/// A line's key and value read as loosely as the loosest reader (fs-fat's guard): `#` comment, a BOM, any case
/// of the key, a non-UTF-8 byte (as the kernel's lossy parser). One reader for the comparison and the merge, so
/// no spelling is counted by one and missed by the other.
fn key_value(raw: &[u8]) -> Option<(String, String)> {
    let l = String::from_utf8_lossy(raw);
    let l = l.split('#').next().unwrap_or("").trim().trim_start_matches('\u{feff}');
    let (k, v) = l.split_once('=')?;
    Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
}

/// The value `key` has (read as [`key_value`]; last line wins).
pub fn effective(text: &[u8], key: &str) -> Option<String> {
    let key = key.to_ascii_lowercase();
    text.split(|&b| b == b'\n').filter_map(key_value).filter(|(k, _)| *k == key).last().map(|(_, v)| v)
}

/// The kept key a line sets, if any.
fn kept_key(line: &[u8]) -> Option<&'static str> {
    let (k, _) = key_value(line)?;
    KEPT_KEYS.iter().copied().find(|x| *x == k)
}

fn boot_is_safe(boot: &[u8]) -> bool {
    effective(boot, "profile").is_some_and(|p| p.eq_ignore_ascii_case("gaming-pc"))
}

/// `file` without its kept-key lines, then the boot copy's kept-key lines in their order.
fn merge(boot: &[u8], file: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(file.len() + 128);
    for line in file.split(|&b| b == b'\n') {
        if kept_key(line).is_none() {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    // split yields an empty last piece for a text ending in '\n' (or an empty text): no extra blank line.
    if out.ends_with(b"\n\n") || (file.is_empty() && out == b"\n") {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(b"# reload: these keys stay as at boot until the next firmware start\n");
    for line in boot.split(|&b| b == b'\n') {
        if kept_key(line).is_some() {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    out
}

/// The CONFIG text the new kernel gets (`None`: the boot copy unchanged) and why (pure; host-tested).
pub fn choose_config(boot: &[u8], file: FileNow) -> (Option<Vec<u8>>, Outcome) {
    let text = match file {
        FileNow::Text(t) => t,
        FileNow::Missing => return (None, Outcome::Missing),
        FileNow::Failed(e) => return (None, Outcome::Failed(e)),
        FileNow::Slow => return (None, Outcome::Slow),
        FileNow::Busy => return (None, Outcome::Busy),
    };
    if text == boot {
        return (None, Outcome::Unchanged);
    }
    if boot_is_safe(boot) {
        return (None, Outcome::SafeMode);
    }
    let changed: Vec<&'static str> = KEPT_KEYS.iter().copied().filter(|k| effective(&text, k) != effective(boot, k)).collect();
    let guard = fs_fat::protect::config_guard(boot, &text);
    if changed.is_empty() && guard.is_ok() {
        return (Some(text), Outcome::Fresh);
    }
    let mut keys = changed;
    if let Err(k) = guard
        && let Some(k) = KEPT_KEYS.iter().copied().find(|x| *x == k)
        && !keys.contains(&k)
    {
        keys.push(k);
    }
    let merged = merge(boot, &text);
    if fs_fat::protect::config_guard(boot, &merged).is_err() || KEPT_KEYS.iter().any(|k| effective(&merged, k) != effective(boot, k)) {
        // Cannot happen (the merge puts the boot lines back verbatim); the boot copy is the safe answer.
        return (None, Outcome::KeptKeys(Vec::new()));
    }
    (Some(merged), Outcome::KeptKeys(keys))
}

/// The GENERATION record for the new kernel (`None`: unchanged): a generation started on trial (TRY) whose
/// folder now carries `OK` (this boot confirmed it) is handed on as confirmed (pure).
pub fn refreshed_generation(g: Option<Generation>, ok_on_stick: Option<bool>) -> Option<Generation> {
    let g = g?;
    (g.reason == gen_reason::NEWEST_TRY && ok_on_stick == Some(true)).then_some(Generation { reason: gen_reason::NEWEST_OK, ..g })
}

/// What the stick says now.
pub struct StickNow {
    pub file: FileNow,
    /// The running generation's `OK` marker (`None`: no such folder, no generation record, or not listed).
    pub ok: Option<bool>,
}

/// The plan for the jump: the new CONFIG (if any), why, and the new GENERATION record (if any).
pub struct Plan {
    pub config: Option<Vec<u8>>,
    pub outcome: Outcome,
    pub generation: Option<Generation>,
    /// RB-9: `generation` is another stick generation's record (a Reload into it), not the running one confirmed.
    pub switched: bool,
    pub boot_bytes: usize,
}

/// One stick read at a time: a stick that hangs holds one helper, not one per Reload or dry run.
static READING: AtomicBool = AtomicBool::new(false);
/// Woken when a helper ends (its result is in, `READING` is free).
static READ_WQ: ksync::WaitQueue = ksync::WaitQueue::new();

struct Slot {
    v: ksync::SpinLock<Option<StickNow>>,
    done: AtomicBool,
}

fn take_reader() -> bool {
    READING.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// Reads CONFIG.TXT (at most `max` bytes) and the generation `running`'s markers in a helper thread, waiting
/// at most [`STICK_DEADLINE_NS`]. `wait_for_reader` (the jump): an earlier helper still running is waited for
/// up to the deadline too; without it (a dry run) the answer is [`FileNow::Busy`] at once.
fn read_stick(running: Option<u32>, max: usize, wait_for_reader: bool) -> StickNow {
    let only = |file| StickNow { file, ok: None };
    if !take_reader() && !(wait_for_reader && READ_WQ.wait_until_timeout(take_reader, STICK_DEADLINE_NS)) {
        return only(FileNow::Busy);
    }
    let slot = Arc::new(Slot { v: ksync::SpinLock::named("kexec.refresh", None), done: AtomicBool::new(false) });
    let s = slot.clone();
    let started = sched::spawn("reload-config", sched::Class::Normal, move || {
        let now = match fs_fat::stick::with_sys(|sys| {
            let path = alloc::format!("{}/BITSOS2/CONFIG.TXT", sys.mount);
            let file = match sys.ns.read_file(&path, max) {
                Ok(t) => FileNow::Text(t),
                Err(uapi::errno::ENOENT) => FileNow::Missing,
                Err(e) => FileNow::Failed(e),
            };
            let ok = running.and_then(|n| sys.generations().ok()?.iter().find(|g| g.n == n).map(|g| g.ok));
            StickNow { file, ok }
        }) {
            Ok(n) => n,
            Err(e) => StickNow { file: FileNow::Failed(e), ok: None },
        };
        *s.v.lock() = Some(now);
        s.done.store(true, Ordering::Release);
        READING.store(false, Ordering::Release);
        READ_WQ.wake_all();
    });
    if started.is_err() {
        READING.store(false, Ordering::Release);
        READ_WQ.wake_all();
        return only(FileNow::Failed(uapi::errno::EAGAIN));
    }
    if !READ_WQ.wait_until_timeout(|| slot.done.load(Ordering::Acquire), STICK_DEADLINE_NS) {
        return only(FileNow::Slow);
    }
    slot.v.lock().take().unwrap_or_else(|| only(FileNow::Slow))
}

/// Reads the stick and decides (allocates, sleeps up to the deadline: before the CPUs stop). `target`: the
/// record of the stick generation the Reload goes into (RB-9 `system_pin ... now`), handed on as it is.
pub fn plan(capacity: usize, wait_for_reader: bool, target: Option<Generation>) -> Plan {
    let (boot_copy, running) = match crate::boot::boot_info() {
        Some((bi, _)) => (bi.config().map(<[u8]>::to_vec).unwrap_or_default(), bi.generation()),
        None => (Vec::new(), None),
    };
    let now = read_stick(if target.is_some() { None } else { running.map(|g| g.number) }, capacity, wait_for_reader);
    let (config, outcome) = choose_config(&boot_copy, now.file);
    let generation = if target.is_some() { target } else { refreshed_generation(running, now.ok) };
    Plan { config, outcome, generation, switched: target.is_some(), boot_bytes: boot_copy.len() }
}

/// The refreshed BootInfo for a plan (pure; host-tested).
#[derive(Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// `None`: keep the staged bytes.
    pub bytes: Option<Vec<u8>>,
    /// The plan's CONFIG is in.
    pub config: bool,
    /// The plan's GENERATION is in.
    pub generation: bool,
    /// Why the full plan did not fit (the retry without the CONFIG may still carry the generation).
    pub error: Option<BuildError>,
}

/// The staged BootInfo with the plan's CONFIG and GENERATION; a CONFIG too large for the KEEP block (or any
/// other build error) is dropped and the generation tried alone, so the confirmed generation is not lost with it.
pub fn rebuild(staged: &[u8], capacity: usize, p: &Plan) -> Rebuilt {
    let none = |error| Rebuilt { bytes: None, config: false, generation: false, error };
    if p.config.is_none() && p.generation.is_none() {
        return none(None);
    }
    match handover::refresh(staged, capacity, p.config.as_deref(), p.generation) {
        Ok(b) => Rebuilt { bytes: Some(b), config: p.config.is_some(), generation: p.generation.is_some(), error: None },
        Err(e) if p.config.is_some() && p.generation.is_some() => match handover::refresh(staged, capacity, None, p.generation) {
            Ok(b) => Rebuilt { bytes: Some(b), config: false, generation: true, error: Some(e) },
            Err(_) => none(Some(e)),
        },
        Err(e) => none(Some(e)),
    }
}

/// The plan's one-line description (reload's dry run; values never, only sizes and key names).
pub fn describe(p: &Plan, r: &Rebuilt, capacity: usize) -> String {
    let cfg = match (&p.config, &p.outcome) {
        (Some(c), _) if !r.config => alloc::format!(
            "CONFIG: the boot copy ({} bytes): the stick's CONFIG.TXT ({} bytes) does not fit the KEEP block ({} bytes)",
            p.boot_bytes,
            c.len(),
            capacity
        ),
        (Some(c), Outcome::KeptKeys(k)) => alloc::format!(
            "CONFIG: the stick's CONFIG.TXT ({} bytes, boot copy {}), with {} as at boot",
            c.len(),
            p.boot_bytes,
            k.join(",")
        ),
        (Some(c), _) => alloc::format!("CONFIG: the stick's CONFIG.TXT ({} bytes, boot copy {})", c.len(), p.boot_bytes),
        (None, Outcome::Unchanged) => alloc::format!("CONFIG: unchanged since boot ({} bytes)", p.boot_bytes),
        (None, Outcome::Failed(e)) => {
            alloc::format!("CONFIG: the boot copy ({} bytes): the stick's file could not be read ({})", p.boot_bytes, crate::install::errno_name(*e))
        }
        (None, Outcome::Busy) => alloc::format!(
            "CONFIG: not read now, an earlier read of the stick is still running (the jump waits for it and reads then; boot copy {} bytes)",
            p.boot_bytes
        ),
        (None, o) => alloc::format!("CONFIG: the boot copy ({} bytes, {})", p.boot_bytes, o.name()),
    };
    match (p.generation, r.generation) {
        (Some(g), true) if p.switched => alloc::format!(
            "{cfg}; the new kernel is told it is generation {} ({})",
            g.number,
            bitsboot_pure::gens::reason_name(g.reason)
        ),
        (Some(g), false) if p.switched => alloc::format!("{cfg}; generation {}: the BootInfo cannot carry its record", g.number),
        (Some(g), true) => alloc::format!("{cfg}; generation {} handed on as confirmed (OK on the stick)", g.number),
        (Some(g), false) => alloc::format!("{cfg}; generation {} is OK on the stick, but the BootInfo cannot carry the change", g.number),
        (None, _) => cfg,
    }
}

/// The dry run's line: what the jump would hand on from the stick now (no wait for an earlier read).
pub fn dry_run(st: &Staged) -> String {
    let cap = st.summary.bootinfo_capacity;
    let p = plan(cap, false, st.generation);
    describe(&p, &rebuild(st.bootinfo_bytes(), cap, &p), cap)
}

/// What [`apply_plan`] did to the staged BootInfo.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// Nothing to change.
    Nothing,
    Swapped { config: bool, generation: bool },
    /// The staged bytes stay (does not fit, or the jump's checks failed on the rebuilt one).
    Kept,
}

/// Swaps the plan into the staged BootInfo; `check` is the jump's check (a ktest injects a failing one). Any
/// failure keeps the staged bytes, logged.
pub fn apply_plan(st: &mut Staged, p: &Plan, check: &dyn Fn(&Staged) -> Result<(), String>) -> Applied {
    let counter = st.summary.counter;
    let cap = st.summary.bootinfo_capacity;
    let r = rebuild(st.bootinfo_bytes(), cap, p);
    if let Some(e) = r.error {
        klog::event!(Warn, "kexec", "reload.config_kept", reload = counter, capacity = cap, bytes = p.config.as_ref().map_or(0, Vec::len),
            error = alloc::format!("{e:?}"), generation_handed_on = r.generation;
            "the refreshed BootInfo does not fit the KEEP block: the new kernel gets the boot copy's CONFIG");
    }
    let Some(new) = r.bytes else {
        return if r.error.is_some() { Applied::Kept } else { Applied::Nothing };
    };
    let old = st.replace_bootinfo(new);
    if let Err(e) = check(st) {
        st.replace_bootinfo(old);
        klog::event!(Warn, "kexec", "reload.config_kept", reload = counter, error = e.as_str();
            "the refreshed BootInfo failed the jump's checks: the new kernel gets the staged one");
        return Applied::Kept;
    }
    if r.generation
        && let Some(g) = p.generation
        && p.switched
    {
        klog::event!(Info, "kexec", "reload.generation_switched", reload = counter, generation = g.number,
            reason = bitsboot_pure::gens::reason_name(g.reason), skipped = g.skipped; "the new kernel is told it is this stick generation");
    } else if r.generation
        && let Some(g) = p.generation
    {
        klog::event!(Info, "kexec", "reload.generation_confirmed", reload = counter, generation = g.number;
            "started on trial and confirmed in this boot: handed on as NEWEST_OK");
    }
    Applied::Swapped { config: r.config, generation: r.generation }
}

/// RB-10, at the jump (after the write-back, before the CPUs stop): the staged BootInfo gets the stick's CONFIG
/// and the confirmed generation.
pub fn apply(st: &mut Staged) -> Applied {
    let counter = st.summary.counter;
    let p = plan(st.summary.bootinfo_capacity, true, st.generation);
    let (errno, keys) = match &p.outcome {
        Outcome::Failed(e) => (*e, String::new()),
        Outcome::KeptKeys(k) => (0, k.join(",")),
        _ => (0, String::new()),
    };
    if p.outcome.fell_back() {
        klog::event!(Warn, "kexec", "reload.config", reload = counter, source = p.outcome.name(), boot_bytes = p.boot_bytes, errno = errno;
            "the stick's CONFIG.TXT could not be read: the new kernel gets the boot copy");
    } else if matches!(p.outcome, Outcome::KeptKeys(_)) {
        klog::event!(Warn, "kexec", "reload.config", reload = counter, source = p.outcome.name(), bytes = p.config.as_ref().map_or(0, Vec::len),
            boot_bytes = p.boot_bytes, kept = keys.as_str(); "keys only a firmware start changes were changed on the stick: kept as at boot");
    } else {
        klog::event!(Info, "kexec", "reload.config", reload = counter, source = p.outcome.name(), bytes = p.config.as_ref().map_or(p.boot_bytes, Vec::len),
            boot_bytes = p.boot_bytes);
    }
    if matches!(p.outcome, Outcome::Slow | Outcome::Busy) {
        // Like any thread reading the stick when a Reload comes: the xHCI quiesce is lock-free and the bus-master
        // sweep turns off DMA of every function that had it, so the outstanding read cannot hold the jump.
        klog::event!(Warn, "kexec", "reload.config_reader_outstanding", reload = counter, deadline_ms = STICK_DEADLINE_NS / 1_000_000,
            reader = if p.outcome == Outcome::Slow { "this_read" } else { "earlier_read" };
            "a stick read is still running at the jump: the Reload goes on, the quiesce stops the stick's controller");
    }
    apply_plan(st, &p, &|s| s.check().map_err(|e| alloc::format!("{e:?}")))
}
