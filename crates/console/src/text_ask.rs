//! A start question on the text console once the report page has ended (the measurement run, `gpu-messung`):
//! the asking command's thread draws the question's lines and waits; a key of a real keyboard answers it (J
//! starts, any other key ends it with nothing done, `gp_report::ask::start_key`). A key counts only
//! `report_page::ASK_ARM_NS` after the question appeared and again after every real key before that, as on the
//! report page. Keys MCP or a program injected, and those of a virtual device, never answer and are taken while
//! the question stands (they type nothing into the console either).

use alloc::string::String;
use core::sync::atomic::{AtomicU8, Ordering};
use gp_report::ask::{Start, start_key};
use ksync::SpinLock;

/// When a key starts to count; None: no question stands.
static ARMED_AT: SpinLock<Option<u64>> = SpinLock::named("console.text_ask", None);
/// 0: no answer yet, 1: Ja, 2: Nein.
static ANSWER: AtomicU8 = AtomicU8::new(0);

/// The answer to the question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Ja,
    Nein,
}

/// Puts the question up: its lines on the screen, no answer yet, keys counting in two seconds.
pub fn open(lines: &[String]) {
    for l in lines {
        crate::vt::write(l, crate::screen::FG_INDEX);
        crate::vt::write("\n", crate::screen::FG_INDEX);
    }
    ANSWER.store(0, Ordering::Release);
    *ARMED_AT.lock() = Some(time::now_ns() + crate::report_page::ASK_ARM_NS);
}

/// The answer, once a real key gave one.
pub fn poll() -> Option<Answer> {
    match ANSWER.load(Ordering::Acquire) {
        1 => Some(Answer::Ja),
        2 => Some(Answer::Nein),
        _ => None,
    }
}

/// A question stands (put up and not taken down yet).
pub fn standing() -> bool {
    ARMED_AT.lock().is_some()
}

/// Takes the question down (answered or not).
pub fn close() {
    *ARMED_AT.lock() = None;
}

/// Asks and waits on the caller's thread: the answer, or None when no real key came within `wait_ns`.
pub fn ask(lines: &[String], wait_ns: u64) -> Option<Answer> {
    open(lines);
    let end = time::now_ns().saturating_add(wait_ns);
    let got = loop {
        if let Some(a) = poll() {
            break Some(a);
        }
        if time::now_ns() >= end {
            break None;
        }
        sched::sleep_ns(20_000_000);
    };
    close();
    got
}

/// A key press on the VT's input thread: true when a question stands (the key is the question's then, whatever
/// it does). Must not wait.
pub(crate) fn answer(ev: &crate::evdev::InputEvent) -> bool {
    let now = time::now_ns();
    if ARMED_AT.lock().is_none() {
        return false;
    }
    // Outside the lock: the device list is cloned (an allocation), as `report_page::answer` does it.
    let real = crate::evdev::devices().iter().find(|d| d.number == ev.device).is_some_and(|d| crate::report_page::real_key(ev.injected, d.info.bus, &d.info.phys, &d.info.name));
    let mut g = ARMED_AT.lock();
    let Some(armed_at) = *g else { return false };
    if !real {
        drop(g);
        klog::event!(Info, "console", "ask.ignored", code = ev.code, injected = ev.injected; "not a real keyboard: the start question stays");
        return true;
    }
    if ANSWER.load(Ordering::Acquire) != 0 {
        return true;
    }
    match start_key(ev.code, now >= armed_at) {
        Start::Early => {
            *g = Some(now + crate::report_page::ASK_ARM_NS);
            drop(g);
            klog::event!(Info, "console", "ask.early", code = ev.code; "a real key before it counts: taken, nothing answered");
        }
        a => {
            ANSWER.store(if a == Start::Ja { 1 } else { 2 }, Ordering::Release);
            drop(g);
            klog::event!(Info, "console", "ask.answered", yes = a == Start::Ja);
        }
    }
    true
}
