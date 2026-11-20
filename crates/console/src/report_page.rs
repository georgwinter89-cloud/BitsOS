//! GP-ROUND (plan/spec/gaming-pc.md §8): the Gaming-PC's report page on the text console. The kernel hands over the
//! pages (`gp_report::page::layout`), this module shows one of them in place of the text model, as the boot screen
//! does: log lines and the shell's output go on into the model meanwhile and appear when the page ends. Pages turn by
//! the kernel's timer (`turn`) and on any key; a key on the last page ends the page and shows the console with the
//! shell's prompt (the key itself types nothing). Only the profile gaming-pc ever calls `show`: elsewhere this
//! module stays inactive and costs one flag read per key and per redraw.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ksync::SpinLock;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The page shown (0-based).
static AT: AtomicUsize = AtomicUsize::new(0);
/// A leaf: held only to copy the pages in or out.
static PAGES: SpinLock<Vec<Vec<String>>> = SpinLock::named("console.report_page", Vec::new());
/// The page ended once (a key, or a program took the screen): a later `show` does not take the screen back.
static ENDED: AtomicBool = AtomicBool::new(false);

/// VB-1: the question on the page ("Version N läuft. Bestätigen?"): its state, when a key starts to count, and
/// the kernel's answer (called on the VT's input thread: it must not wait).
struct Ask {
    state: gp_report::ask::State,
    armed_at: u64,
    answer: fn(bool),
}
static ASK: SpinLock<Option<Ask>> = SpinLock::named("console.report_ask", None);
/// A real J or Z came before the key counted (the question's rows say so until it is answered).
static EARLY: AtomicBool = AtomicBool::new(false);
/// A key counts this long after the question appeared, and again after every real key before that
/// (bitsinstall's console rule).
pub const ASK_ARM_NS: u64 = 2_000_000_000;

/// The page is on the screen (the text model is not drawn).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

/// Shows `pages` (or replaces the pages on screen, keeping the page number when it still exists). False when
/// the page already ended once: the screen is Georg's shell then, and the new text goes only to file and MCP.
pub fn show(pages: Vec<Vec<String>>) -> bool {
    if ENDED.load(Ordering::Acquire) || pages.is_empty() {
        return false;
    }
    let n = pages.len();
    *PAGES.lock() = pages;
    if AT.load(Ordering::Acquire) >= n {
        AT.store(0, Ordering::Release);
    }
    ACTIVE.store(true, Ordering::Release);
    draw();
    true
}

/// The next page (after the last: the first again). (page, pages) of the one shown, None when the page is not up.
pub fn turn() -> Option<(usize, usize)> {
    if !active() || crate::vt::graphics() {
        return None;
    }
    let n = PAGES.lock().len().max(1);
    let at = (AT.load(Ordering::Acquire) + 1) % n;
    AT.store(at, Ordering::Release);
    draw();
    Some((at + 1, n))
}

/// (page, pages) shown now.
pub fn shown() -> Option<(usize, usize)> {
    active().then(|| (AT.load(Ordering::Acquire) + 1, PAGES.lock().len()))
}

/// The lines of the page shown now (MCP `screen` text mode reads them while the page is up).
pub fn lines() -> Option<Vec<String>> {
    if !active() {
        return None;
    }
    PAGES.lock().get(AT.load(Ordering::Acquire)).cloned()
}

/// A key while the page is up: the next page, or after the last one the console. True when the key was taken.
pub(crate) fn key() -> bool {
    if !active() {
        return false;
    }
    let n = PAGES.lock().len();
    let at = AT.load(Ordering::Acquire);
    if at + 1 < n {
        AT.store(at + 1, Ordering::Release);
        draw();
        klog::event!(Info, "console", "report.page", page = at + 2, pages = n, by = "key");
    } else {
        end("key");
        crate::vt::show_text();
    }
    true
}

/// VB-1: the question's state (None: no question); `answer(true)` is "Ja", `answer(false)` "Zurück". The arming
/// starts when the question is asked anew.
pub fn set_ask(state: Option<gp_report::ask::State>, answer: fn(bool)) {
    let now = time::now_ns();
    if !matches!(state, Some(gp_report::ask::State::Asking { .. })) {
        EARLY.store(false, Ordering::Release);
    }
    let mut a = ASK.lock();
    *a = match (state, a.take()) {
        (None, _) => None,
        (Some(st), Some(old)) if matches!(old.state, gp_report::ask::State::Asking { .. }) && matches!(st, gp_report::ask::State::Asking { .. }) => Some(Ask { state: st, ..old }),
        (Some(st), _) => Some(Ask { state: st, armed_at: now + ASK_ARM_NS, answer }),
    };
}

/// Whether a real J or Z came too early since the question was asked.
pub fn early() -> bool {
    EARLY.load(Ordering::Acquire)
}

/// Whether a key's device is a real keyboard (pure): no event MCP or a program injected, no MCP or virtual device
/// (bitsinstall's `injected_why`, the rule of the desktop proof).
pub fn real_key(injected: bool, bus: u16, phys: &str, name: &str) -> bool {
    !injected && bus != crate::evdev::BUS_VIRTUAL && !phys.starts_with("mcp/") && !name.starts_with("BitsOS MCP")
}

/// A key press while the page and its question are up: true when it answered, or was a J or Z that came too
/// early (the key does nothing else then: the page neither turns nor ends).
pub(crate) fn answer(ev: &crate::evdev::InputEvent) -> bool {
    if !active() || ASK.lock().is_none() {
        return false;
    }
    let now = time::now_ns();
    let real = crate::evdev::devices().iter().find(|d| d.number == ev.device).is_some_and(|d| real_key(ev.injected, d.info.bus, &d.info.phys, &d.info.name));
    let mut g = ASK.lock();
    let Some(a) = g.as_mut() else { return false };
    if !real {
        klog::event!(Info, "console", "report.ask_ignored", code = ev.code, injected = ev.injected; "not a real keyboard: the question stays");
        return false;
    }
    let armed = now >= a.armed_at;
    if !armed {
        a.armed_at = now + ASK_ARM_NS;
    }
    let yes = match gp_report::ask::key(&a.state, ev.code, armed) {
        gp_report::ask::Key::Page => return false,
        gp_report::ask::Key::Early => {
            EARLY.store(true, Ordering::Release);
            drop(g);
            klog::event!(Info, "console", "report.ask_early", code = ev.code; "a real key before it counts: taken, nothing answered");
            return true;
        }
        gp_report::ask::Key::Ja => true,
        gp_report::ask::Key::Zurueck => false,
    };
    a.state = gp_report::ask::State::Confirming;
    let f = a.answer;
    drop(g);
    klog::event!(Info, "console", "report.ask_answered", yes = yes);
    f(yes);
    true
}

/// Panic/NMI path: the page off without a lock, an allocation or a log line; true when it was up (the caller
/// paints the text model over it before the panic lines, as after the boot screen).
pub fn end_for_panic() -> bool {
    ENDED.store(true, Ordering::Release);
    ACTIVE.swap(false, Ordering::AcqRel)
}

/// For a ktest only: an earlier test's program took the screen and so ended the page for this boot (`end`
/// "handover"); with the console back in text mode the test may show its own page again. Never called otherwise.
pub(crate) fn reopen_for_ktest() -> bool {
    if crate::vt::graphics() {
        return false;
    }
    ENDED.store(false, Ordering::Release);
    AT.store(0, Ordering::Release);
    true
}

/// Ends the page (`reason` for the log); the caller shows the text console.
pub fn end(reason: &'static str) -> bool {
    ENDED.store(true, Ordering::Release);
    if !ACTIVE.swap(false, Ordering::AcqRel) {
        return false;
    }
    klog::event!(Info, "console", "report.ended", reason = reason);
    true
}

fn draw() {
    // A program has the screen: its picture is not drawn over (the page ended at the handover anyway).
    if crate::vt::graphics() {
        return;
    }
    let Some(lines) = lines() else { return };
    crate::vt::draw_page(&lines);
}

/// Colour of each page row: rows Georg must see whole in the error colour, titles bright, the rest normal. A
/// row `gp_report::page::wrap` continued (four spaces deeper than the row it belongs to) has its row's colour,
/// and the mark is looked for in the whole row, so a mark wrapped onto the continuation colours the first row too.
pub(crate) fn colors(lines: &[String]) -> Vec<u32> {
    let lead = |l: &str| l.len() - l.trim_start_matches(' ').len();
    let mut out = alloc::vec![crate::FG; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let own = lead(&lines[i]);
        let mut j = i + 1;
        while j < lines.len() && !lines[i].trim().is_empty() && !lines[j].trim().is_empty() && lead(&lines[j]) == own + 4 {
            j += 1;
        }
        let whole: Vec<&str> = lines[i..j].iter().map(|l| l.trim()).collect();
        let c = color_of(&lines[i], &whole.join(" "), i == 0);
        out[i..j].fill(c);
        i = j;
    }
    out
}

fn color_of(row: &str, whole: &str, first: bool) -> u32 {
    if gp_report::page::must_keep(whole) {
        crate::FG_ERR
    } else if first || (!row.starts_with(' ') && !row.is_empty()) {
        crate::FG_PROMPT
    } else {
        crate::FG
    }
}
