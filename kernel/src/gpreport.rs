//! GP-ROUND (plan/spec/gaming-pc.md §8): the Gaming-PC's report page. Under the profile gaming-pc only, a thread
//! waits until the drivers bound and the network's staged self-check had its time (at most [`SETTLE_S`] seconds),
//! collects the rows from the drivers' getters (`collect`), shows them as pages on the firmware screen with no
//! keyboard (`console::report_page`) and keeps the text for MCP (`hw_info {"section":"report"}`). Only then, with
//! `gp_stick=on`, a thread of its own writes it once per boot below `\BITSOS2\REPORT\` on the boot stick
//! (`gp_report::file`; the newest three kept): USB storage can take minutes in exactly the failures Georg must
//! photograph, and the page never waits for it. The page is redrawn whole (never appended to: a photo must hold
//! it whole) on three events only, each at most once: the file thread's answer (the file's row alone), a second
//! collection [`REDRAW_S`] seconds after the first page when it says something new, and a card's address arriving
//! later than that (polled every [`ADDR_POLL_S`] seconds for at most [`ADDR_WAIT_S`] seconds).
//! VB-1 adds a fourth: the question "Version N läuft. Bestätigen?" at the foot of every page (`ask`), redrawn when
//! its state changes (asked, answered, confirmed by a proof first, the restart for "Zurück").
//! On every other machine `start` returns at its first line and nothing here runs.

mod ask;
mod collect;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use gp_report::rows::{FileState, McpDown};
use ksync::{Once, SpinLock};

/// The page waits at most this long for the network's self-check (from the thread's start, a few seconds after
/// power-on's kernel entry): the page comes within about a minute even when the network never does.
const SETTLE_S: u64 = 45;
/// At least this long for USB to enumerate the keyboard, the stick and the headset.
const MIN_S: u64 = 8;
/// The one later collection.
const REDRAW_S: u64 = 60;
/// After the page said "noch keine Adresse": how often to look again, and for how long after the page.
const ADDR_POLL_S: u64 = 5;
const ADDR_WAIT_S: u64 = 300;

/// What `main` hands over (BootInfo is gone by the time the thread runs).
pub struct Facts {
    /// The framebuffer (address, bytes, width, height).
    pub fb: Option<(u64, u64, u32, u32)>,
    /// Generation numbers the loader skipped as older than the disk guard (`bitsboot generation.unguarded`).
    pub unguarded: Vec<u32>,
    /// The generation running and the loader's word for why.
    pub generation: Option<(u32, &'static str)>,
}

static FACTS: Once<Facts> = Once::new();
/// MCP's start: Ok(port) or why not.
static MCP: Once<Result<u16, McpDown>> = Once::new();
/// The report the file thread writes next (the newest wins); `FILE_BUSY` is true from a post until the thread
/// found the slot empty again, both changed under this lock.
static FILE_JOB: SpinLock<Option<gp_report::Report>> = SpinLock::named("gp.report.file_job", None);
static FILE_BUSY: AtomicBool = AtomicBool::new(false);
/// The file thread's latest answer, taken by the report thread.
static FILE_DONE: SpinLock<Option<FileState>> = SpinLock::named("gp.report.file_done", None);
/// The report thread ends: the file thread ends too once its slot is empty.
static FILE_CLOSED: AtomicBool = AtomicBool::new(false);
/// The report's text lines for MCP (None until the first page).
static TEXT: SpinLock<Option<Vec<String>>> = SpinLock::named("gp.report.text", None);

/// Whether this boot runs the Gaming-PC's profile (the loader's or the kernel's own decision; under QEMU the
/// board's DMI names give it too, which is the round's rehearsal `host/tests/pc/80-gp-round.sh`).
pub fn on() -> bool {
    gp_platform::tiers::is_gaming_pc(crate::config::get("profile").as_deref())
}

/// `main` notes MCP's start before calling `start`.
pub fn note_mcp(r: Result<u16, McpDown>) {
    MCP.get_or_init(|| r);
}

/// MCP's start error in the page's terms.
pub fn mcp_down(e: mcp::StartError) -> McpDown {
    match e {
        mcp::StartError::NoToken => McpDown::NoToken,
        mcp::StartError::BadToken => McpDown::BadToken,
        mcp::StartError::BadPort => McpDown::BadPort,
        mcp::StartError::NoStack | mcp::StartError::Listen(_) | mcp::StartError::Spawn => McpDown::NetworkNotReady,
    }
}

/// Starts the report thread under the profile gaming-pc.
pub fn start(facts: Facts) {
    if !on() {
        return;
    }
    FACTS.get_or_init(|| facts);
    mcp::set_report_source(text);
    if let Err(e) = sched::spawn("gp.report", sched::Class::Normal, run) {
        klog::event!(Error, "kernel", "gp.report_not_started", error = alloc::format!("{e:?}"); "no report page this boot");
    }
}

/// The generations the loader's log names as older than the disk guard.
pub fn unguarded_in(log: &[u8]) -> Vec<u32> {
    let text = String::from_utf8_lossy(log);
    text.lines()
        .filter_map(|l| l.split("generation.unguarded number=").nth(1))
        .filter_map(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok())
        .collect()
}

/// Why the loader started this generation, in German.
pub fn reason_de(r: u32) -> &'static str {
    use bootinfo::gen_reason::*;
    match r {
        NEWEST_OK => "die neueste, bestätigt",
        NEWEST_TRY => "die neueste, zur Probe",
        FALLBACK_AFTER_TRY => "Rückfall: die neueste wurde nie bestätigt",
        FALLBACK_BAD_FILES => "Rückfall: die Dateien der neuesten sind kaputt oder zu alt",
        PINNED => "festgelegt",
        NEWEST_NO_RULE => "die neueste",
        _ => "Grund unbekannt",
    }
}

fn text() -> Option<Vec<String>> {
    TEXT.lock().clone()
}

fn now_s() -> u64 {
    time::now_ns() / 1_000_000_000
}

fn run() {
    // Under the profile the screen is quiet until the page: one plain line says that it comes and when.
    console::Console::write_str(console::console(), "Der Bericht für diesen PC wird gesammelt und erscheint in spätestens einer Minute.\n", console::FG);
    let t0 = now_s();
    let mut waited_for_net = false;
    loop {
        let el = now_s().saturating_sub(t0);
        if el >= MIN_S && collect::network_settled(el) {
            break;
        }
        if el >= SETTLE_S {
            waited_for_net = true;
            break;
        }
        sched::sleep_ms(500);
    }
    let Some(facts) = FACTS.get() else { return };
    let mcp = MCP.get().copied().unwrap_or(Err(McpDown::Unknown));
    let mut file = collect::file_first();
    let mut cur = collect::report(facts, &mcp, file.clone(), true);
    ask_turn(&mut cur);
    *TEXT.lock() = Some(cur.lines());
    let shown = show(&cur);
    klog::event!(Info, "kernel", "gp.report", rows = cur.rows.len(), waited_s = now_s().saturating_sub(t0), net_timed_out = waited_for_net,
        file = gp_report::rows::file(&file).as_str());
    if file == FileState::Waiting {
        post_file(&cur);
        if let Err(e) = sched::spawn("gp.report.file", sched::Class::Normal, file_thread) {
            klog::event!(Error, "kernel", "gp.report.file_failed", why = "spawn", error = alloc::format!("{e:?}"));
            FILE_BUSY.store(false, Ordering::Release);
            file = FileState::Failed(String::from("das Schreiben konnte nicht starten"));
            cur.set_file(&file);
            publish(&cur);
        }
    }
    let t_page = now_s();
    let (mut turned, redraw_at) = (t_page, t_page + REDRAW_S);
    let mut redrawn = false;
    let mut addr_wait = waits_for_address(&cur);
    let mut addr_polled = t_page;
    let mut was_up = shown;
    // Turns the pages until Georg's key ends the page; the three later events; then the thread ends.
    loop {
        let busy = FILE_BUSY.load(Ordering::Acquire);
        let done = FILE_DONE.lock().take();
        if let Some(st) = done
            && st != file
        {
            klog::event!(Info, "kernel", "gp.report.file_row", file = gp_report::rows::file(&st).as_str());
            file = st;
            cur.set_file(&file);
            publish(&cur);
        }
        if !redrawn && now_s() >= redraw_at {
            redrawn = true;
            refresh(facts, &mcp, &file, &mut cur, "later");
            addr_wait &= waits_for_address(&cur);
        }
        if addr_wait && now_s().saturating_sub(addr_polled) >= ADDR_POLL_S {
            addr_polled = now_s();
            if collect::has_address() {
                addr_wait = false;
                refresh(facts, &mcp, &file, &mut cur, "address");
            } else if now_s().saturating_sub(t_page) >= ADDR_WAIT_S {
                addr_wait = false;
            }
        }
        if ask_turn(&mut cur) {
            publish(&cur);
        }
        let up = shown && console::report_page::active();
        if was_up && !up && !cur.ask.is_empty() {
            ask::after_page();
        }
        was_up = up;
        if up && now_s().saturating_sub(turned) >= gp_report::page::TURN_S {
            turned = now_s();
            if let Some((i, n)) = console::report_page::turn()
                && n > 1
            {
                klog::event!(Debug, "console", "report.page", page = i, pages = n, by = "timer");
            }
        }
        if !up && redrawn && !addr_wait && !busy {
            break;
        }
        sched::sleep_ms(250);
    }
    FILE_CLOSED.store(true, Ordering::Release);
}

/// VB-1: the question's rows as the kernel's confirmation stands now; true when they changed (the console gets
/// the new state, the caller redraws).
fn ask_turn(cur: &mut gp_report::Report) -> bool {
    let now = ask::state();
    let lines = now.as_ref().map_or(Vec::new(), |(v, s)| gp_report::ask::lines(*v, s));
    if lines == cur.ask {
        return false;
    }
    cur.ask = lines;
    console::report_page::set_ask(now.map(|(_, s)| s), ask::answer);
    true
}

/// The page says MCP runs but no card has an address yet (the address poll's start and end).
fn waits_for_address(r: &gp_report::Report) -> bool {
    let no_addr = gp_report::rows::remote(&gp_report::rows::Remote::NoAddress);
    r.group(gp_report::Group::Network).any(|t| t == no_addr)
}

/// A new collection: when it says something new, the page is redrawn whole, MCP's text renewed and the file
/// written again (the file thread takes the newest; one still on its way gets the new rows too).
fn refresh(facts: &Facts, mcp: &Result<u16, McpDown>, file: &FileState, cur: &mut gp_report::Report, why: &'static str) {
    let mut again = collect::report(facts, mcp, file.clone(), false);
    again.ask = cur.ask.clone();
    if again.same_rows(cur) {
        return;
    }
    *cur = again;
    let up = publish(cur);
    klog::event!(Info, "kernel", "gp.report.redrawn", rows = cur.rows.len(), on_screen = up, why = why);
    if matches!(file, FileState::Written(_) | FileState::Waiting) {
        post_file(cur);
    }
}

/// MCP's text and, while the page is up, the page itself.
fn publish(r: &gp_report::Report) -> bool {
    *TEXT.lock() = Some(r.lines());
    console::report_page::active() && show(r)
}

fn post_file(r: &gp_report::Report) {
    let mut slot = FILE_JOB.lock();
    *slot = Some(r.clone());
    FILE_BUSY.store(true, Ordering::Release);
}

/// The file thread: writes each posted report (the first decides the name, later ones renew it); after a
/// failure it stops, since a stick that failed once is not tried again this boot.
fn file_thread() {
    let mut name: Option<String> = None;
    loop {
        let job = {
            let mut slot = FILE_JOB.lock();
            let j = slot.take();
            if j.is_none() {
                FILE_BUSY.store(false, Ordering::Release);
            }
            j
        };
        match job {
            Some(r) => {
                let st = write_file(&r, name.clone());
                let failed = matches!(st, FileState::Failed(_));
                if let FileState::Written(n) = &st {
                    name = Some(n.clone());
                }
                *FILE_DONE.lock() = Some(st);
                if failed {
                    let _ = FILE_JOB.lock().take();
                    FILE_BUSY.store(false, Ordering::Release);
                    return;
                }
            }
            None if FILE_CLOSED.load(Ordering::Acquire) => return,
            None => sched::sleep_ms(250),
        }
    }
}

/// Shows `r` on the console's grid; false without a display or after Georg ended the page.
fn show(r: &gp_report::Report) -> bool {
    let (cols, rows) = console::vt::size();
    if cols == 0 || rows == 0 {
        return false;
    }
    let pages = gp_report::page::layout(r, cols, rows);
    let n = pages.len();
    let up = console::report_page::show(pages);
    if up {
        klog::event!(Info, "console", "report.shown", pages = n, cols = cols, rows = rows);
    }
    up
}

/// Writes the report to the stick (`gp_stick=on` only): a new file, or `again` the same one renewed. The state for
/// the page's own row.
fn write_file(r: &gp_report::Report, again: Option<String>) -> FileState {
    if crate::config::get(gp_platform::tiers::STICK_KEY).as_deref().map(str::trim) != Some("on") {
        return FileState::Off;
    }
    let Some(vol) = fs_fat::stick::wait_mounted(30_000) else {
        klog::event!(Warn, "kernel", "gp.report.file_failed", why = "not_mounted");
        return FileState::Failed(String::from("der Stick wurde nicht eingebunden"));
    };
    if !vol.writable() {
        klog::event!(Warn, "kernel", "gp.report.file_failed", why = "read_only", reason = fs_fat::stick::last_reason().unwrap_or(""));
        return FileState::Failed(String::from("der Stick wird nur gelesen"));
    }
    let ns = vfs::root_ns();
    let dir = gp_report::file::DIR;
    let name = match again {
        Some(n) => n,
        None => {
            let names: Vec<String> = ns.list_dir(dir).map(|l| l.into_iter().map(|(n, _)| String::from_utf8_lossy(&n).into_owned()).collect()).unwrap_or_default();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let (name, old) = gp_report::file::plan(&refs);
            let c = vfs::Ctx::kernel(&ns);
            for o in old {
                if let Err(e) = c.unlink(None, alloc::format!("{dir}/{o}").as_bytes()) {
                    klog::event!(Warn, "kernel", "gp.report.old_kept", file = o.as_str(), error = alloc::format!("{e}"));
                }
            }
            name
        }
    };
    // The page's own row names the file before it is written, so the file holds the line that names it.
    let mut text = r.clone();
    text.set_file(&FileState::Written(name.clone()));
    let path = alloc::format!("{dir}/{name}");
    // Durable at once: the round may end with the power button held at any moment after the page appeared.
    let written = ns.create_file_bytes(path.as_bytes(), 0o644, text.file_text().as_bytes()).and_then(|d| d.inode.sync(vfs::pagecache::SyncKind::Fsync));
    let written = written.and_then(|_| vol.sync(true));
    match written {
        Ok(_) => {
            klog::event!(Info, "kernel", "gp.report.file", path = path.as_str(), bytes = text.file_text().len());
            FileState::Written(name)
        }
        Err(e) => {
            klog::event!(Warn, "kernel", "gp.report.file_failed", path = path.as_str(), error = alloc::format!("{e}"));
            FileState::Failed(alloc::format!("Fehler beim Schreiben: {e}"))
        }
    }
}
