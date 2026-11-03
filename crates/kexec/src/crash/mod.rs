//! The panic path (RB-2, spec/core.md §10.2-10.3): what the kernel's panic handler calls once it has made sure
//! only one CPU reports (`kernel/src/main.rs`, change request in plan/notes/rb-RB-2.md).
//!
//! `report(info)`, in this order, never allocating (the heap is in emergency mode) and never waiting for a lock:
//! 1. the panic line (`kernel panic`, as before);
//! 2. every other online CPU is stopped by NMI; each first writes its registers and a frame-pointer backtrace
//!    into its own slot (`hal::nmi`);
//! 3. this CPU's backtrace (frame pointers; a trap frame on the way, e.g. the #GP or #DF that ended in the panic,
//!    is crossed and named), with function names from the symbol table (`symbols`);
//! 4. each other CPU: registers and backtrace, or "silent";
//! 5. the crash record in the crash page (`record`): the same lines plus the newest log lines, so the next boot
//!    logs it (`crash.previous`) and the MCP `log` shows it;
//! 6. CONFIG `panic=halt|reboot` (`panic_delay=` seconds, default 30): by default a reboot after 30 s on hardware
//!    and a halt under a hypervisor, as before; a halt under a hypervisor also leaves QEMU with its panic exit
//!    code (ktest protocol), `panic=reboot` resets in QEMU too (warm reset: the crash-record scenario).
//!
//! Every line goes out through `klog::try_event!` (ring, serial, screen, the mirror's emergency path) and
//! `klog::flush_emergency()` after each block, so the Gaming-PC's listener gets it (the Mini-PC has no COM1).
//!
//! At boot (`init`, a Core initcall): the CONFIG keys, the crash page (registers it with hal, logs and marks the
//! previous record, reads and writes the boot marker that shows whether RAM survived the reset), the NMI slots,
//! and the symbol table.

mod clean;
pub mod record;
pub mod symbols;
pub mod text;
mod ktests;
mod ktests_nmi_user;
pub mod notes;
mod tool;

use crate::boot;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use hal::panic::Step;
use ksync::Once;
use record::{Header, Marker, MarkerFound, flags};
use symbols::Symbols;
use text::{Buf, OneLine};

pub use clean::{set_sound_hook, set_watchdog_hook, watchdog_secs, SoundHook, Watchdog, WatchdogHook, WATCHDOG_MARGIN_S};

// ---------------------------------------------------------------------------------------------------------
// CONFIG

/// `panic=` not set: reboot on hardware, halt under a hypervisor.
const MODE_DEFAULT: u8 = 0;
const MODE_HALT: u8 = 1;
const MODE_REBOOT: u8 = 2;

pub const DEFAULT_DELAY_S: u64 = 30;
/// Longest `panic_delay=` (an hour; a bound against a typo, the machine is down meanwhile).
pub const MAX_DELAY_S: u64 = 3600;

static MODE: AtomicU8 = AtomicU8::new(MODE_DEFAULT);
static DELAY_S: AtomicU64 = AtomicU64::new(DEFAULT_DELAY_S);

/// What happens after the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Stop here; under a hypervisor also leave QEMU with the panic exit code (the ktest protocol).
    Halt { qemu_exit: bool },
    /// Reset the machine after this many seconds.
    Reboot { secs: u64 },
}

/// The CONFIG values: (mode, delay in s, the first problem found). Bad values keep the defaults.
pub fn parse_config(panic: Option<&str>, delay: Option<&str>) -> (u8, u64, Option<&'static str>) {
    let mut problem = None;
    let mode = match panic.map(str::trim) {
        None | Some("") => MODE_DEFAULT,
        Some("halt") => MODE_HALT,
        Some("reboot") => MODE_REBOOT,
        Some(_) => {
            problem = Some("panic= wants halt or reboot");
            MODE_DEFAULT
        }
    };
    let secs = match delay.map(str::trim) {
        None | Some("") => DEFAULT_DELAY_S,
        Some(s) => match s.parse::<u64>() {
            Ok(v) if v <= MAX_DELAY_S => v,
            _ => {
                problem = problem.or(Some("panic_delay= wants whole seconds 0..3600"));
                DEFAULT_DELAY_S
            }
        },
    };
    (mode, secs, problem)
}

pub fn decide(mode: u8, secs: u64, hypervisor: bool) -> Action {
    match mode {
        // Under QEMU a halt also leaves QEMU with the panic exit code (the ktest protocol; xtask's images say
        // panic=halt); on hardware port 0xF4 is unused.
        MODE_HALT => Action::Halt { qemu_exit: hypervisor },
        MODE_REBOOT => Action::Reboot { secs },
        _ if hypervisor => Action::Halt { qemu_exit: true },
        _ => Action::Reboot { secs },
    }
}

/// The action a panic would take now.
pub fn action() -> Action {
    decide(MODE.load(Ordering::Acquire), DELAY_S.load(Ordering::Acquire), hal::cpu::detect().hypervisor)
}

fn mode_name(m: u8) -> &'static str {
    match m {
        MODE_HALT => "halt",
        MODE_REBOOT => "reboot",
        _ => "default",
    }
}

// ---------------------------------------------------------------------------------------------------------
// Boot side

static SYMBOLS: Once<(Symbols, &'static str)> = Once::new();
static PREVIOUS: Once<Previous> = Once::new();
static MARKER: Once<(MarkerFound, Option<Marker>)> = Once::new();
/// This boot's own marker and its offset in the crash page (a Reload rewrites it with the next kernel's id).
static MINE: Once<(Marker, u64)> = Once::new();

/// The crash record the boot before left (kept for `crash_info` and the ktests).
#[derive(Clone, Debug)]
pub struct Previous {
    pub header: Header,
    pub text: String,
    /// It had been logged by an earlier boot already (this boot did not log it again).
    pub reported_before: bool,
}

pub fn previous() -> Option<&'static Previous> {
    PREVIOUS.get()
}

/// The symbol table and where it came from (`initrd` or `fw`).
pub fn symbols() -> Option<(&'static Symbols, &'static str)> {
    SYMBOLS.get().map(|(s, src)| (s, *src))
}

/// What this boot found at the crash page's end (see `record::MarkerFound`).
pub fn marker() -> Option<(MarkerFound, Option<Marker>)> {
    MARKER.get().copied()
}

/// RB-11: the running kernel's id (FNV-1a 64 of its KERNEL file) when the Reload that started it handed it on
/// in the crash page's marker; None after a firmware start (bitsboot's GENERATION record names the kernel then)
/// or after a Reload by a kernel that did not hand it on.
pub fn running_kernel_id() -> Option<u64> {
    let (found, m) = marker()?;
    let my_reload = boot::boot_info().and_then(|(bi, _)| bi.reload()).map_or(0, |r| r.counter);
    record::handed_kernel(found, m.as_ref(), my_reload)
}

/// RB-11: right before a Reload's jump, this boot's marker names the kernel id the next kernel will be (`0` takes
/// it back after a jump that did not happen). False without a crash page: the next kernel then does not know it.
pub fn hand_on_kernel(id: u64) -> bool {
    let Some((m, off)) = MINE.get().copied() else { return false };
    hal::panic::crash_page_write(off, &Marker { next_kernel: id, ..m }.bytes())
}

/// The driver notes the kernel one Reload before handed on (read once at boot, `notes`).
static NOTES_IN: Once<Vec<(u32, Vec<u8>)>> = Once::new();
/// The try-lock of the note slots (two writers at once: the second gets `false`).
static NOTE_BUSY: AtomicBool = AtomicBool::new(false);

/// GX-7f CR-1 (INT-W25): hands a driver note of at most `notes::MAX_DATA` bytes on to the next kernel of this
/// boot (written now; a later call with the same tag replaces it). Safe in a Quiesce hook with interrupts off: no
/// allocation, no lock but a try, no log line. False without a crash page, for tag 0 or too many bytes, while
/// another writer holds the slots, or when every slot holds another tag of this kernel.
pub fn hand_on_note(tag: u32, bytes: &[u8]) -> bool {
    let Some((m, _)) = MINE.get().copied() else { return false };
    let Some((_, len)) = hal::panic::crash_page() else { return false };
    let len = len as usize;
    let Some(slot) = notes::encode(&m.boot_id, tag, m.reload, bytes) else { return false };
    if NOTE_BUSY.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    let n = notes::area(len) / notes::SLOT;
    let at = |i: usize| {
        let mut b = [0u8; notes::SLOT];
        let off = notes::slot_offset(len, record::MARKER, i) as u64;
        (hal::panic::crash_page_read(off, &mut b)).then_some(()).and_then(|_| notes::parse(&m.boot_id, &b).map(|x| (x.tag, x.reload)))
    };
    let done = notes::choose_slot(n, at, tag, m.reload)
        .is_some_and(|i| hal::panic::crash_page_write(notes::slot_offset(len, record::MARKER, i) as u64, &slot));
    NOTE_BUSY.store(false, Ordering::Release);
    done
}

/// GX-7f CR-1 (INT-W25): the note with `tag` the kernel before handed on, only when this kernel came from that
/// kernel's Reload (the rule of `running_kernel_id`); None after a firmware start or without such a note.
pub fn note_from_last(tag: u32) -> Option<Vec<u8>> {
    NOTES_IN.get()?.iter().find(|(t, _)| *t == tag).map(|(_, d)| d.clone())
}

/// The notes in a heap copy of the page that a kernel with Reload counter `mine` of boot `me` takes.
fn notes_handed(page: &[u8], me: &[u8; 16], mine: u32) -> Vec<(u32, Vec<u8>)> {
    let len = page.len();
    let mut out: Vec<(u32, Vec<u8>)> = Vec::new();
    for i in 0..notes::area(len) / notes::SLOT {
        let o = notes::slot_offset(len, record::MARKER, i);
        if let Some(n) = notes::parse(me, &page[o..o + notes::SLOT])
            && notes::from_last(n.reload, mine)
            && !out.iter().any(|(t, _)| *t == n.tag)
            && out.try_reserve(1).is_ok()
        {
            out.push((n.tag, n.data.to_vec()));
        }
    }
    out
}

/// RB-11: a Reload's `jump` with kernel id `id` handed on; `jump` returns only when the jump did not happen, and
/// the id is then taken back (a failed Reload leaves no id for a later one to inherit).
pub fn jump_handing_on<E>(id: u64, jump: impl FnOnce() -> E) -> E {
    let handed = hand_on_kernel(id);
    if !handed {
        // The new kernel then says "not known which kernel"; this record says why on the sending side.
        klog::try_event!(Warn, "crash", "kernel_id.not_handed", kernel_fnv = klog::Hex(id); "no crash-page marker of this boot (or it could not be written): the next kernel will not know its kernel id");
        klog::drain_deferred(usize::MAX);
    }
    let e = jump();
    if handed && !hand_on_kernel(0) {
        klog::try_event!(Warn, "crash", "kernel_id.take_back_failed"; "the marker still names the kernel of a Reload that did not happen");
    }
    e
}

fn hex16(id: &[u8; 16]) -> Buf<32> {
    let mut b = Buf::new();
    for x in id {
        let _ = write!(b, "{x:02x}");
    }
    b
}

fn my_boot_id() -> [u8; 16] {
    boot::boot_info().and_then(|(bi, _)| bi.boot_id()).unwrap_or([0; 16])
}

/// Registers BootInfo's crash page with hal, which checks it (KEEP memory, direct-mapped RAM) itself.
fn register_page() -> Option<(u64, u64)> {
    if let Some(p) = hal::panic::crash_page() {
        return Some(p);
    }
    let (bi, _) = boot::boot_info()?;
    match hal::panic::set_crash_page(&bi) {
        Ok(p) => Some(p),
        Err(hal::panic::CrashPageError::None) => None,
        Err(e) => {
            let r = bi.crash_page().unwrap_or(bootinfo::PhysRange { phys: 0, size: 0 });
            klog::event!(Warn, "crash", "page.refused", phys = klog::Hex(r.phys), bytes = r.size, error = alloc::format!("{e:?}"); "the CRASH_PAGE record is not a usable KEEP page: crashes are logged but not kept");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// The reset after a panic: the firmware's reset register first (FADT; the Mini-PC: I/O 0xB2 <- 0xBE), the one
// the MCP `reboot` uses, so the RAM check of the crash page measures the reset a panic really does; then hal's
// last resorts (0xCF9, 8042, triple fault).

/// The FADT reset register in I/O space: port << 8 | value, `NO_RESET` = none.
static RESET_IO: AtomicU64 = AtomicU64::new(NO_RESET);
const NO_RESET: u64 = u64::MAX;
/// The FADT reset register in memory space, mapped at boot (no mapping on the panic path), and its value.
static RESET_MEM: Once<(hal::mmio::IoMem, u8)> = Once::new();
/// How long one reset path gets before the next is tried.
const RESET_STEP_NS: u64 = 50_000_000;

/// What the FADT names, checked by the ACPI register guard at boot; returns a description for the log.
fn init_reset() -> &'static str {
    let Some(r) = acpi::live::tables().and_then(|a| a.fadt.as_ref()).and_then(|f| f.usable_reset()) else {
        return "0xCF9, 8042, triple fault (no FADT reset register)";
    };
    match r.reg.space {
        acpi::sdt::Space::Io if r.reg.address <= 0xFFFF && !(0xCF8..=0xCFF).contains(&r.reg.address) => {
            if acpi::live::check_io(r.reg.address, 1, true).is_err() {
                return "0xCF9, 8042, triple fault (the ACPI guard refused the FADT reset port)";
            }
            RESET_IO.store((r.reg.address << 8) | r.value as u64, Ordering::Release);
            "FADT I/O reset register, then 0xCF9, 8042, triple fault"
        }
        acpi::sdt::Space::Memory => {
            if acpi::live::check_mem(r.reg.address, 1).is_err() {
                return "0xCF9, 8042, triple fault (the ACPI guard refused the FADT reset address)";
            }
            match hal::mmio::map_device(r.reg.address, 1, hal::paging::CacheType::Uncached) {
                Ok(w) => {
                    RESET_MEM.get_or_init(|| (w, r.value));
                    "FADT memory reset register, then 0xCF9, 8042, triple fault"
                }
                Err(_) => "0xCF9, 8042, triple fault (the FADT reset register could not be mapped)",
            }
        }
        // 0xCF9 named by the FADT: hal's first last resort writes it anyway.
        _ => "0xCF9, 8042, triple fault (the FADT names 0xCF9 or another space)",
    }
}

/// Resets the machine in the MCP `reboot`'s order (`acpi::power::reboot_plan`): the FADT register, then hal's
/// last resorts (0xCF9 hard, 0xCF9 full, 8042, triple fault). Each path is logged before it is tried (RB-13: the
/// mirror's last line names the one that reset). Lock- and allocation-free.
fn reset_machine() -> ! {
    let io = RESET_IO.load(Ordering::Acquire);
    let mut n = 0;
    if io != NO_RESET {
        n += 1;
        clean::step(n, "FADT I/O register");
        let _ = hal::portio::outb((io >> 8) as u16, io as u8);
        hal::panic::spin_ns(RESET_STEP_NS, panic_hz());
    }
    if let Some((w, v)) = RESET_MEM.get() {
        n += 1;
        clean::step(n, "FADT memory register");
        let _ = w.write8(0, *v);
        hal::panic::spin_ns(RESET_STEP_NS, panic_hz());
    }
    clean::step(n + 1, LAST_RESORTS);
    hal::panic::reset_now()
}

/// What `hal::panic::reset_now` tries, in its order (`hal::panic::LAST_RESORT_CF9`, host-tested).
const LAST_RESORTS: &str = "0xCF9 hard (0x6), 0xCF9 full (0xe), 8042, triple fault";

/// The reset order this boot found, for the record (no heap).
fn describe_reset(w: &mut dyn Write) {
    let io = RESET_IO.load(Ordering::Acquire);
    if io != NO_RESET {
        let _ = write!(w, "FADT I/O {:#x} <- {:#x}, then ", io >> 8, io & 0xFF);
    }
    if RESET_MEM.get().is_some() {
        let _ = write!(w, "FADT memory register, then ");
    }
    let _ = write!(w, "{LAST_RESORTS}");
}

fn init_config() {
    PANIC_HZ.store(time::tsc_hz(), Ordering::Release);
    let (mode, secs, problem) = parse_config(boot::config("panic").as_deref(), boot::config("panic_delay").as_deref());
    MODE.store(mode, Ordering::Release);
    DELAY_S.store(secs, Ordering::Release);
    if let Some(p) = problem {
        klog::event!(Warn, "crash", "config.bad"; "{p}; keeping the default");
    }
    let a = decide(mode, secs, hal::cpu::detect().hypervisor);
    let after = match a {
        Action::Halt { .. } => "halt",
        Action::Reboot { .. } => "reboot",
    };
    let reset = init_reset();
    klog::event!(Info, "crash", "config", panic = mode_name(mode), delay_s = secs, after_panic = after, reset = reset);
}

/// Reads the page the boot before left: the marker (did RAM survive?), the crash record (logged once, then
/// marked), then writes this boot's marker.
fn init_page() {
    let Some((phys, len)) = register_page() else {
        klog::event!(Warn, "crash", "page.missing"; "no CRASH_PAGE in BootInfo: crashes are logged but not kept");
        return;
    };
    let len = len as usize;
    if len < record::MIN_PAGE {
        klog::event!(Warn, "crash", "page.small", phys = klog::Hex(phys), bytes = len);
        return;
    }
    let mut page = Vec::new();
    if page.try_reserve_exact(len).is_err() {
        klog::event!(Warn, "crash", "page.no_memory", bytes = len);
        return;
    }
    page.resize(len, 0u8);
    hal::panic::crash_page_read(0, &mut page);
    let me = my_boot_id();
    let m = Marker::parse(&page[len - record::MARKER..]);
    let found = record::judge(m.as_ref(), &me);
    MARKER.get_or_init(|| (found, m));
    let pm = m.unwrap_or_default();
    klog::event!(
        Info,
        "crash",
        "page",
        phys = klog::Hex(phys),
        bytes = len,
        marker = found.name(),
        prev_boot = hex16(&pm.boot_id).as_str(),
        prev_generation = pm.generation,
        prev_reload = pm.reload,
        prev_uptime_s = pm.time_ns / 1_000_000_000;
        "{}",
        match found {
            MarkerFound::Kept => "the page kept the boot before's marker: RAM survived the reset",
            MarkerFound::SameBoot => "marker of this same boot (a Reload)",
            MarkerFound::Absent => "no marker: a cold start (or RAM was cleared)",
        }
    );
    match record::decode(&page) {
        Ok((h, body)) => {
            let text = String::from_utf8_lossy(body).into_owned();
            let reported_before = h.flags & flags::REPORTED != 0;
            if !reported_before {
                log_previous(&h, &text, &me);
                let marked = Header { flags: h.flags | flags::REPORTED, ..h };
                let mut f = marked.sum_start();
                f.update(body);
                hal::panic::crash_page_write(0, &marked.bytes(f.finish()));
            }
            PREVIOUS.get_or_init(|| Previous { header: h, text, reported_before });
        }
        Err(record::RecordError::NoMagic) => {}
        Err(e) => klog::event!(Warn, "crash", "record.damaged", phys = klog::Hex(phys), error = alloc::format!("{e:?}")),
    }
    let mine = Marker {
        boot_id: me,
        generation: boot::boot_info().and_then(|(bi, _)| bi.generation()).map_or(0, |g| g.number),
        reload: boot::boot_info().and_then(|(bi, _)| bi.reload()).map_or(0, |r| r.counter),
        time_ns: time::now_ns(),
        next_kernel: 0,
    };
    // GX-7f CR-1: the boot id is in each note's checksum, so only a Reload of this same boot hands notes on.
    let handed_notes = notes_handed(&page, &me, mine.reload);
    if !handed_notes.is_empty() {
        let tags: Vec<String> = handed_notes.iter().map(|(t, d)| alloc::format!("{:08x}/{}", t, d.len())).collect();
        klog::event!(Info, "crash", "notes.handed", count = handed_notes.len(), reload = mine.reload, tags = tags.join(",").as_str(); "driver notes from the kernel before this Reload");
    }
    NOTES_IN.get_or_init(|| handed_notes);
    let off = (len - record::MARKER) as u64;
    if hal::panic::crash_page_write(off, &mine.bytes()) {
        MINE.get_or_init(|| (mine, off));
    }
    let handed = record::handed_kernel(found, m.as_ref(), mine.reload);
    if let Some(id) = handed {
        klog::event!(Info, "crash", "kernel_id.handed", kernel_id = klog::Hex(id), reload = mine.reload; "the Reload before named this kernel's KERNEL file");
    } else if mine.reload > 0 {
        klog::event!(Info, "crash", "kernel_id.unknown", reload = mine.reload; "the kernel before this Reload did not name this kernel's KERNEL file");
    }
}

fn log_previous(h: &Header, text: &str, me: &[u8; 16]) {
    let first = text.lines().next().unwrap_or("");
    klog::event!(
        Error,
        "crash",
        "previous",
        boot = hex16(&h.boot_id).as_str(),
        uptime_ms = h.time_ns / 1_000_000,
        cpu = h.cpu,
        cpus_asked = h.cpus_asked,
        cpus_answered = h.cpus_answered,
        reload = h.reload,
        generation = h.generation,
        exception = h.flags & flags::EXCEPTION != 0,
        test = h.flags & flags::TEST != 0,
        cut = h.flags & flags::CUT != 0,
        same_boot = &h.boot_id == me;
        "the boot before ended in a crash: {first}"
    );
    for (i, l) in text.lines().enumerate() {
        klog::event!(Info, "crash", "previous.line", n = i, text = l);
    }
}

fn init_symbols() {
    let Some((bi, _)) = boot::boot_info() else { return };
    let (s, e) = hal::panic::kernel_text();
    let keep = move |a: u64| a >= s && a < e;
    let anchor = symbols::rb_ksyms_anchor as fn() -> u64 as usize as u64;
    let mut tried = 0u32;
    let mut sources: Vec<(&'static str, u64, u64)> = Vec::new();
    for m in bi.modules() {
        if m.kind == bootinfo::module_kind::FIRMWARE && m.name.eq_ignore_ascii_case(b"KSYMS") {
            sources.push(("fw", m.phys, m.size));
        }
        // An initcall, before the kernel releases the initramfs; the check keeps a later caller off freed frames.
        if m.kind == bootinfo::module_kind::INITRAMFS && boot::module_readable(&m) {
            let mut read = |off: u64, buf: &mut [u8]| off.checked_add(buf.len() as u64).is_some_and(|e| e <= m.size) && hal::kexec_stub::read_phys(m.phys + off, buf);
            if let Some((off, len)) = symbols::cpio_find(&mut read, m.size, "boot/kernel.syms") {
                sources.insert(0, ("initrd", m.phys + off, len));
            }
        }
    }
    for (src, phys, len) in sources {
        tried += 1;
        let mut text = Vec::new();
        if len > usize::MAX as u64 || text.try_reserve_exact(len as usize).is_err() {
            klog::event!(Warn, "crash", "symbols.no_memory", source = src, bytes = len);
            continue;
        }
        text.resize(len as usize, 0u8);
        hal::kexec_stub::read_phys(phys, &mut text);
        match Symbols::parse_nm(&text, &keep) {
            Ok(t) if symbols::matches_build(&t, anchor) => {
                klog::event!(Info, "crash", "symbols", source = src, symbols = t.len(), bytes = t.bytes());
                SYMBOLS.get_or_init(|| (t, src));
                return;
            }
            Ok(t) => klog::event!(Warn, "crash", "symbols.other_build", source = src, symbols = t.len(); "the table does not name this kernel's anchor; ignored"),
            Err(e) => klog::event!(Warn, "crash", "symbols.unusable", source = src, error = alloc::format!("{e:?}")),
        }
    }
    klog::event!(Info, "crash", "symbols.none", tried = tried; "backtraces show addresses only (no boot/kernel.syms in the initramfs, no fw KSYMS)");
}

fn init() -> Result<(), &'static str> {
    init_config();
    hal::nmi::prepare();
    init_page();
    // RB-11 MC CR 2: hw_info and reboot name the running kernel after a Reload (the record names bitsboot's).
    if let Some(id) = running_kernel_id() {
        mcp::set_running_kernel_id(id);
    }
    init_symbols();
    Ok(())
}
kreg::initcall!(kreg::Level::Core, "crash.init", init);

// ---------------------------------------------------------------------------------------------------------
// The report

/// Steps of the reporting CPU's backtrace (a bound on one report).
const OWN_DEPTH: usize = 40;
/// Steps the walk may go on to after a full `OWN_DEPTH` (streamed, repeats folded: a sanity bound, not a buffer).
const OWN_WALK_MAX: usize = 1024;
/// Bytes of the record's body kept for the newest log lines from before the panic.
const LOG_RESERVE: usize = 1024;
/// Callers of each other CPU that go into the record's line for it (all of them go to the log and the mirror):
/// the page is small and the reporting CPU's own backtrace matters most.
const RECORD_OTHER_FRAMES: usize = 3;
/// Bytes of log lines taken per report (stack buffer).
const LOG_BUF: usize = 2048;

static REPORTING: AtomicBool = AtomicBool::new(false);
static STOP_DONE: AtomicBool = AtomicBool::new(false);
/// The stop's counts (asked, answered, send failures, ns), its error (0 none, 1 not prepared, 2 busy), CPUs not online.
static STOP_ROUND: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Stops every other online CPU by NMI (each saves its registers and backtrace first, `hal::nmi`). The kernel's
/// panic handler calls it first, before the heap goes into emergency mode: CPUs still running then would fail
/// their next allocation, panic too, and their slots would show that instead of their work. Idempotent; no
/// allocation, no lock, no log (`report` logs the result).
pub fn stop_others() {
    if STOP_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let wait = if hal::cpu::detect().hypervisor { 3_000_000_000 } else { 1_000_000_000 };
    match hal::nmi::stop_others(clock, wait) {
        Ok(r) => {
            for (i, v) in [r.asked as u64, r.answered as u64, r.send_failed as u64, r.ns, 0, r.not_online as u64].into_iter().enumerate() {
                STOP_ROUND[i].store(v, Ordering::Release);
            }
        }
        Err(e) => STOP_ROUND[4].store(if e == hal::nmi::NmiError::NotPrepared { 1 } else { 2 }, Ordering::Release),
    }
}

/// The TSC rate the panic path counts with, read at boot (`init_config`). RB-13 fixer: `time::tsc_hz` and
/// `time::now_ns` read the clock's sequence lock, which a CPU the panic stopped inside `Clock::set_rate` (the
/// TSC recheck) leaves odd for good; the panic path's own waits must not depend on it. A later recheck moves the
/// rate by ppm only, which does not matter for these bounds.
static PANIC_HZ: AtomicU64 = AtomicU64::new(0);

/// The TSC rate for panic waits (4 GHz assumed before `init_config`). Never blocks.
fn panic_hz() -> u64 {
    match PANIC_HZ.load(Ordering::Acquire) {
        0 => 4_000_000_000,
        hz => hz,
    }
}

/// Nanoseconds on the TSC, for waits in the panic path (`time::now_ns` is 0 before the clock exists). Never blocks.
fn clock() -> u64 {
    ((hal::cpu::rdtsc() as u128 * 1_000_000_000) / panic_hz() as u128) as u64
}

/// "name+0x1f" for an address (a return address is looked up one byte earlier: the call, not what follows).
pub fn symbolize(addr: u64, is_return: bool) -> Buf<240> {
    let mut b = Buf::new();
    let probe = if is_return { addr.wrapping_sub(1) } else { addr };
    match symbols().and_then(|(t, _)| t.lookup(probe)) {
        Some((name, off)) => {
            let off = if is_return { off + 1 } else { off };
            let _ = write!(b, "{name}+{off:#x}");
        }
        None => {
            let _ = b.write_str("?");
        }
    }
    b
}

/// Settles the `Guess` steps of a backtrace (`hal::panic::entry_return`: a return address read at the first
/// instructions of the function a trap interrupted) against the symbol table, in place; returns the new count.
/// With a table a guess is a call when the interrupted rip is a function's first or second instruction (its
/// `push rbp` or `mov rbp, rsp`) and is dropped otherwise (hand-written asm that pushes rbp mid-routine: the
/// word was a saved register); without a table it stays a guess, printed with `~`.
pub fn settle(steps: &mut [Step], n: usize) -> usize {
    match symbols() {
        Some((t, _)) => settle_with(steps, n, Some(&|a| t.lookup(a).map(|x| x.1))),
        None => settle_with(steps, n, None),
    }
}

/// `settle` with a lookup "offset of `a` in its function" (`None` for the lookup: no table).
pub fn settle_with(steps: &mut [Step], n: usize, offset_of: Option<&dyn Fn(u64) -> Option<u64>>) -> usize {
    let mut k = 0;
    let mut rip = None;
    for i in 0..n.min(steps.len()) {
        let s = steps[i];
        let keep = match (s, rip, offset_of) {
            (Step::Guess(ret), Some(r), Some(off)) => match off(r) {
                Some(o) if o <= 1 => Some(Step::Call(ret)),
                Some(_) => None,
                None => Some(s),
            },
            _ => Some(s),
        };
        rip = match s {
            Step::Trap { rip, user: false, .. } => Some(rip),
            _ => None,
        };
        if let Some(x) = keep {
            steps[k] = x;
            k += 1;
        }
    }
    k
}

/// Streams body lines into the crash page.
struct Page {
    off: u64,
    limit: u64,
    end: u64,
    on: bool,
    cut: bool,
}

impl Page {
    fn open() -> Page {
        match hal::panic::crash_page() {
            Some((_, len)) if len as usize >= record::MIN_PAGE => {
                let end = (record::HEADER + record::body_capacity(len as usize)) as u64;
                Page { off: record::HEADER as u64, limit: end.saturating_sub(LOG_RESERVE as u64), end, on: true, cut: false }
            }
            _ => Page { off: 0, limit: 0, end: 0, on: false, cut: false },
        }
    }
    fn line(&mut self, s: &str) {
        if !self.on {
            return;
        }
        let n = s.len() as u64 + 1;
        if self.off + n > self.limit {
            self.cut = true;
            return;
        }
        hal::panic::crash_page_write(self.off, s.as_bytes());
        hal::panic::crash_page_write(self.off + s.len() as u64, b"\n");
        self.off += n;
    }
    fn log_tail(&mut self, before_seq: u64) -> usize {
        if !self.on {
            return 0;
        }
        self.limit = self.end;
        self.line("log:");
        let room = (self.end - self.off) as usize;
        let mut buf = [0u8; LOG_BUF];
        let take = room.min(LOG_BUF);
        match klog::try_tail_text_before(&mut buf[..take], before_seq) {
            Some(n) => {
                hal::panic::crash_page_write(self.off, &buf[..n]);
                self.off += n as u64;
                n
            }
            None => {
                self.line("(log ring busy)");
                0
            }
        }
    }
    /// Header with the checksum over the body as written (read back in pieces).
    fn commit(&self, mut h: Header) -> u32 {
        if !self.on {
            return 0;
        }
        h.body_len = (self.off - record::HEADER as u64) as u32;
        if self.cut {
            h.flags |= flags::CUT;
        }
        let mut f = h.sum_start();
        let mut at = record::HEADER as u64;
        let mut chunk = [0u8; 256];
        while at < self.off {
            let k = ((self.off - at) as usize).min(chunk.len());
            hal::panic::crash_page_read(at, &mut chunk[..k]);
            f.update(&chunk[..k]);
            at += k as u64;
        }
        hal::panic::crash_page_write(0, &h.bytes(f.finish()));
        h.body_len
    }
}

/// What `Fold` makes of one raw step of the walk's rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Folded {
    /// Print step `n` (after a `Repeat` line for the steps folded before it, if any).
    Print { n: usize, step: Step, repeat: Option<(usize, usize)> },
    /// Nothing to print (already printed, settled away, or folded).
    Nothing,
}

/// The rest of the reporting CPU's walk after the first `OWN_DEPTH` steps (pure; host-tested): settled like them
/// (`settle_with`), and every run of one repeated two-step pattern (a trap and the handler frame it re-entered,
/// the 26.10. #PF recursion) folded into one `(first step, count)`, so the outermost steps (where the recursion
/// began) show.
pub(crate) struct Fold {
    /// Raw steps still to pass over (the ones already printed).
    skip: usize,
    /// Index of the next step.
    next: usize,
    prev_trap: Option<u64>,
    /// The last two steps, oldest first.
    last: [Step; 2],
    folded: usize,
    first_folded: usize,
}

impl Fold {
    pub(crate) fn new(raw: usize, printed: usize) -> Fold {
        Fold { skip: raw, next: printed, prev_trap: None, last: [Step::None; 2], folded: 0, first_folded: 0 }
    }

    /// `offset_of`: offset of an address in its function (the symbol table), as for `settle_with`.
    pub(crate) fn step(&mut self, s: Step, offset_of: Option<&dyn Fn(u64) -> Option<u64>>) -> Folded {
        let prev = self.prev_trap;
        self.prev_trap = match s {
            Step::Trap { rip, user: false, .. } => Some(rip),
            _ => None,
        };
        if self.skip > 0 {
            self.skip -= 1;
            self.last = [self.last[1], s];
            return Folded::Nothing;
        }
        let s = match (s, prev, offset_of) {
            (Step::Guess(ret), Some(r), Some(off)) => match off(r) {
                Some(o) if o <= 1 => Step::Call(ret),
                Some(_) => return Folded::Nothing,
                None => s,
            },
            _ => s,
        };
        let n = self.next;
        self.next += 1;
        let again = s != Step::None && s == self.last[0];
        self.last = [self.last[1], s];
        if again {
            if self.folded == 0 {
                self.first_folded = n;
            }
            self.folded += 1;
            return Folded::Nothing;
        }
        Folded::Print { n, step: s, repeat: self.take() }
    }

    /// The run folded last, if any (at the end of the walk).
    pub(crate) fn take(&mut self) -> Option<(usize, usize)> {
        let r = (self.folded > 0).then_some((self.first_folded, self.folded));
        self.folded = 0;
        r
    }

    pub(crate) fn steps(&self) -> usize {
        self.next
    }
}

fn repeat_line(page: &mut Page, cpu: usize, (from, count): (usize, usize)) {
    klog::try_event!(Error, "crash", "repeat", cpu = cpu, n = from, steps = count; "these steps repeat the two before them");
    let mut l = Buf::<96>::new();
    let _ = write!(l, " #{from}..#{} repeat the two steps above ({count} steps)", from + count - 1);
    page.line(l.as_str());
}

/// Prints the walk's rest through `Fold` (section 3 of the report).
fn print_rest(page: &mut Page, cpu: usize, rbp: u64, raw: usize, printed: usize) {
    let mut f = Fold::new(raw, printed);
    let table = symbols().map(|(t, _)| t);
    let off = |a: u64| table.and_then(|t| t.lookup(a).map(|x| x.1));
    let off: Option<&dyn Fn(u64) -> Option<u64>> = if table.is_some() { Some(&off) } else { None };
    hal::panic::walk_visit(rbp, OWN_WALK_MAX, &mut |s| {
        if let Folded::Print { n, step, repeat } = f.step(s, off) {
            if let Some(r) = repeat {
                repeat_line(page, cpu, r);
            }
            step_line(page, cpu, n, step);
        }
    });
    if let Some(r) = f.take() {
        repeat_line(page, cpu, r);
    }
    let mut l = Buf::<64>::new();
    let _ = write!(l, "cpu {cpu} backtrace end ({} steps)", f.steps());
    page.line(l.as_str());
}

fn step_line(page: &mut Page, cpu: usize, i: usize, s: Step) {
    step_line_to(Some(page), cpu, i, s);
}

/// One backtrace step as an event, and as a record line when `page` is given.
fn step_line_to(page: Option<&mut Page>, cpu: usize, i: usize, s: Step) {
    let mut l = Buf::<320>::new();
    match s {
        Step::None => return,
        Step::Call(ret) => {
            let sym = symbolize(ret, true);
            klog::try_event!(Error, "crash", "frame", cpu = cpu, n = i, pc = klog::Hex(ret), at = sym.as_str());
            let _ = write!(l, " #{i} {ret:016x} {}", sym.as_str());
        }
        Step::Guess(ret) => {
            let sym = symbolize(ret, true);
            klog::try_event!(Error, "crash", "frame", cpu = cpu, n = i, pc = klog::Hex(ret), at = sym.as_str(), guess = true);
            let _ = write!(l, " #{i} ~{ret:016x} {}", sym.as_str());
        }
        Step::Trap { vector, error, rip, user } => {
            let name = hal::idt::exception_name(vector);
            if user {
                klog::try_event!(Error, "crash", "trap", cpu = cpu, n = i, vector = vector, error = klog::Hex(error), rip = klog::Hex(rip), user = true, what = name);
                let _ = write!(l, " -- {name} from user mode at {rip:#x}");
            } else {
                let sym = symbolize(rip, false);
                klog::try_event!(Error, "crash", "trap", cpu = cpu, n = i, vector = vector, error = klog::Hex(error), rip = klog::Hex(rip), user = false, what = name, at = sym.as_str());
                let _ = write!(l, " -- {name} at {rip:016x} {}", sym.as_str());
            }
        }
    }
    if let Some(p) = page {
        p.line(l.as_str());
    }
}

/// Distinct CPU states already in the record (signature, CPU): a CPU in the same place as one of these gets a
/// short "like cpu N" line, so 16 mostly idle CPUs fit the page. A bound on one report's lookback, not a count.
const SEEN: usize = 8;

/// The function names (no offsets) of the interrupted instruction and its first callers, hashed.
fn signature(r: &hal::nmi::CpuRegs) -> u32 {
    let mut f = record::Fnv32::new();
    if r.user {
        f.update(b"user");
    }
    for s in r.steps[..r.nsteps].iter().take(RECORD_OTHER_FRAMES + 1) {
        let (a, ret) = match *s {
            Step::Call(a) | Step::Guess(a) => (a, true),
            Step::Trap { rip, user: false, .. } => (rip, false),
            _ => continue,
        };
        let b = symbolize(a, ret);
        let name = b.as_str().rsplit_once('+').map_or(b.as_str(), |x| x.0);
        if name == "?" {
            f.update(&a.to_le_bytes());
        } else {
            f.update(name.as_bytes());
        }
    }
    f.finish()
}

fn other_cpu(page: &mut Page, c: usize, seen: &mut [(u32, usize); SEEN], nseen: &mut usize) {
    let mut l = Buf::<320>::new();
    if !hal::smp::is_online(c) {
        klog::try_event!(Warn, "crash", "cpu.not_online", cpu = c; "not online (starting up): no NMI, not stopped");
        let _ = write!(l, "cpu {c}: not online, not stopped");
        page.line(l.as_str());
        return;
    }
    let Some(mut r) = hal::nmi::regs(c).filter(|_| hal::nmi::answer(c) == hal::nmi::Answer::Stopped) else {
        klog::try_event!(Warn, "crash", "cpu.silent", cpu = c; "no answer to the NMI (NMIs blocked, parked, or dead)");
        let _ = write!(l, "cpu {c}: no answer to the NMI");
        page.line(l.as_str());
        return;
    };
    r.nsteps = settle(&mut r.steps, r.nsteps);
    let f = &r.frame;
    let sym = if r.user { Buf::<240>::new() } else { symbolize(f.rip, false) };
    klog::try_event!(
        Error,
        "crash",
        "cpu",
        cpu = c,
        apic = r.apic_id,
        rip = klog::Hex(f.rip),
        rsp = klog::Hex(f.rsp),
        rbp = klog::Hex(f.rbp),
        rflags = klog::Hex(f.rflags),
        cr2 = klog::Hex(r.cr2),
        cr3 = klog::Hex(r.cr3),
        user = r.user,
        at = if r.user { "user" } else { sym.as_str() }
    );
    klog::try_event!(
        Error,
        "crash",
        "regs",
        cpu = c,
        rax = klog::Hex(f.rax),
        rbx = klog::Hex(f.rbx),
        rcx = klog::Hex(f.rcx),
        rdx = klog::Hex(f.rdx),
        rsi = klog::Hex(f.rsi),
        rdi = klog::Hex(f.rdi),
        r8 = klog::Hex(f.r8),
        r9 = klog::Hex(f.r9),
        r10 = klog::Hex(f.r10),
        r11 = klog::Hex(f.r11),
        r12 = klog::Hex(f.r12),
        r13 = klog::Hex(f.r13),
        r14 = klog::Hex(f.r14),
        r15 = klog::Hex(f.r15)
    );
    // The log and the mirror get every frame; the step 0 is the interrupted instruction (on the line above).
    for (i, s) in r.steps[..r.nsteps].iter().enumerate().skip(1) {
        step_line_to(None, c, i, *s);
    }
    // The record: one line per CPU, the place and the first callers, or "like cpu N" for a place seen already.
    let sig = signature(&r);
    if let Some(&(_, m)) = seen[..*nseen].iter().find(|x| x.0 == sig) {
        let _ = write!(l, "cpu {c}: like cpu {m}, rip {:016x} rsp {:016x}", f.rip, f.rsp);
        page.line(l.as_str());
        return;
    }
    if *nseen < SEEN {
        seen[*nseen] = (sig, c);
        *nseen += 1;
    }
    let _ = write!(l, "cpu {c}: rip {:016x} rsp {:016x} rflags {:x} cr2 {:x}{} {}", f.rip, f.rsp, f.rflags, r.cr2, if r.user { " user mode" } else { "" }, sym.as_str());
    for s in r.steps[..r.nsteps].iter().skip(1).take(RECORD_OTHER_FRAMES) {
        match *s {
            Step::Call(a) => {
                let _ = write!(l, " < {}", symbolize(a, true).as_str());
            }
            Step::Guess(a) => {
                let _ = write!(l, " < ~{}", symbolize(a, true).as_str());
            }
            Step::Trap { vector, rip, .. } => {
                let _ = write!(l, " < [{}] {}", hal::idt::exception_name(vector), symbolize(rip, false).as_str());
            }
            Step::None => {}
        }
    }
    page.line(l.as_str());
}

/// The panic report; never returns. Called by the kernel's panic handler on the one CPU that reports, with
/// interrupts off and the heap in emergency mode (see the module doc for the order).
/// Stored lines serial had not printed yet that the panic path prints before its report (at most).
const PRE_PANIC_LINES: usize = 256;

pub fn report(info: &core::panic::PanicInfo<'_>) -> ! {
    let rbp = hal::panic::frame_pointer();
    if REPORTING.swap(true, Ordering::AcqRel) {
        // The kernel lets one CPU report; a second call means a panic inside the report: straight to the end.
        finish(action());
    }
    let me = hal::percpu::this_cpu();
    // CR-5: a CPU stopped inside `Clock::set_rate` holds the clock's sequence lock for good; never spin on it.
    let up = time::now_ns_unlocked();
    // RB-klog: serial is printed from the ring by one CPU at a time; what it had not printed yet comes first,
    // so the serial log reads in order up to the panic.
    klog::flush_serial_emergency(PRE_PANIC_LINES);
    if time::clock_stuck() {
        klog::event!(Error, "crash", "clock.stuck"; "the clock's sequence lock stays held (a stopped CPU in set_rate): time stamps read its fields unchecked");
    }
    // The record's log part ends here: what happened before the panic, not the report itself.
    let seq0 = klog::next_seq();
    klog::try_event!(Error, "kernel", "panic"; "{}", info);
    let mut msg = Buf::<480>::new();
    let _ = write!(OneLine(&mut msg), "{}", info);
    // The trap path's panics read "kernel <exception> at rip ..." (`hal::idt`).
    let exception = msg.as_str().contains(": kernel ") && msg.as_str().contains(" at rip ");
    let mut page = Page::open();
    let mut l = Buf::<640>::new();
    let _ = write!(l, "panic on cpu {me} at {}.{:06} s: {}", up / 1_000_000_000, (up / 1000) % 1_000_000, msg.as_str());
    page.line(l.as_str());

    // 2. The other CPUs (normally stopped already by the kernel's panic handler, see `stop_others`).
    stop_others();
    let rd = |i: usize| STOP_ROUND[i].load(Ordering::Acquire);
    let round = hal::nmi::Round { asked: rd(0) as usize, answered: rd(1) as usize, send_failed: rd(2) as usize, not_online: rd(5) as usize, ns: rd(3) };
    match rd(4) {
        0 => {
            klog::try_event!(Error, "crash", "cpus.stopped", asked = round.asked, answered = round.answered, send_failed = round.send_failed, not_online = round.not_online, us = round.ns / 1000);
        }
        e => {
            let why = if e == 1 { "the NMI slots are not set up yet (panic before the init calls)" } else { "a stop runs already" };
            klog::try_event!(Warn, "crash", "cpus.not_stopped", cpus = hal::percpu::cpu_count(); "{why}");
        }
    }
    klog::flush_emergency();
    // RB-13: the watchdog waits for the panic's own reset (`clean`).
    let reboot = match action() {
        Action::Reboot { secs } => Some(secs),
        Action::Halt { .. } => None,
    };
    if let Some(secs) = reboot {
        clean::watchdog(secs, Some(&mut page));
    }

    // 3. This CPU (from the caller of `report`: the panic handler, then core's panic machinery, then the code).
    let mut steps = [Step::None; OWN_DEPTH];
    let raw = hal::panic::walk(rbp, &mut steps);
    let n = settle(&mut steps, raw);
    l.clear();
    let _ = write!(l, "cpu {me} backtrace ({n} frames{}):", if symbols().is_some() { "" } else { ", no symbol table" });
    page.line(l.as_str());
    for (i, s) in steps[..n].iter().enumerate() {
        step_line(&mut page, me, i, *s);
    }
    if raw == OWN_DEPTH {
        // RB-12: a full walk goes on (an exception recursion filled the 40 steps before the first fault showed).
        print_rest(&mut page, me, rbp, raw, n);
    }
    klog::flush_emergency();
    // RB-13: the xHCI controllers halted and reset before the reset, their lines early in the record (`clean`).
    if reboot.is_some() {
        clean::devices(Some(&mut page));
    }

    // 4. The others.
    let mut seen = [(0u32, 0usize); SEEN];
    let mut nseen = 0;
    for c in (0..hal::percpu::cpu_count()).filter(|&c| c != me) {
        other_cpu(&mut page, c, &mut seen, &mut nseen);
        klog::flush_emergency();
    }

    // 5. The record.
    page.log_tail(seq0);
    let (bi, _) = boot::boot_info().unzip();
    let h = Header {
        body_len: 0,
        flags: if exception { flags::EXCEPTION } else { 0 },
        boot_id: my_boot_id(),
        time_ns: up,
        reload: bi.and_then(|b| b.reload()).map_or(0, |r| r.counter),
        generation: bi.and_then(|b| b.generation()).map_or(0, |g| g.number),
        cpu: me as u32,
        cpus_asked: round.asked.min(u16::MAX as usize) as u16,
        cpus_answered: round.answered.min(u16::MAX as usize) as u16,
    };
    match hal::panic::crash_page() {
        Some((phys, _)) if page.on => {
            let bytes = page.commit(h);
            klog::try_event!(Error, "crash", "record.written", phys = klog::Hex(phys), bytes = bytes, cut = page.cut; "kept for the next boot if RAM survives the reset");
        }
        _ => {
            klog::try_event!(Warn, "crash", "record.none"; "no crash page: nothing kept for the next boot");
        }
    }
    // The other CPUs' backtraces push the panic line off a 1440p console; repeat it last so a photo of the screen
    // shows it (the Mini-PC clears RAM on reset, so the record above is often lost).
    klog::try_event!(Error, "crash", "panic.again", cpu = me; "{}", msg.as_str());
    klog::flush_emergency();
    finish(action())
}

/// The end of a panic: halt (QEMU: its panic exit code) or reset after the delay.
pub fn finish(a: Action) -> ! {
    match a {
        Action::Halt { qemu_exit } => {
            klog::try_event!(Error, "crash", "halt", qemu_exit = qemu_exit; "this CPU stops here (panic=halt, or the default under a hypervisor)");
            klog::flush_emergency();
            if qemu_exit {
                hal::panic::qemu_exit(hal::panic::EXIT_PANIC);
            }
            hal::panic::halt_forever()
        }
        Action::Reboot { secs } => {
            // Done in `report` already, unless the report itself panicked (each step runs once).
            clean::watchdog(secs, None);
            clean::devices(None);
            klog::try_event!(Error, "crash", "reboot", in_s = secs; "restarting the machine in {secs} s");
            klog::flush_emergency();
            hal::panic::spin_ns(secs.saturating_mul(1_000_000_000), panic_hz());
            reset_machine()
        }
    }
}

#[cfg(test)]
mod tests;
