//! VB-1: the question "Version N läuft. Bestätigen?" on the Gaming-PC's report page (safe mode: no desktop asks).
//! The report thread looks at the kernel's confirmation every turn and puts the question's rows at the foot of
//! every page (`gp_report::ask`); the console takes J or Z of a real keyboard (`console::report_page::answer`)
//! and calls `answer` here. "Ja" goes to the boot confirmation's write (`kexec::install::confirm_ask`);
//! "Zurück" pins the newest confirmed version below (the update node's `back`) and restarts by the console's
//! own reboot path. A proof that confirms first ends the question.

use alloc::string::String;
use gp_report::ask::{Back, State};
use kexec::install::confirm_ask;
use ksync::SpinLock;

/// The question's facts once it was seen, the back target as far as known, and what an answer on this page
/// brought (None: no answer yet).
struct Seen {
    asked: Option<(u32, u32)>,
    back: Look,
    after: Option<State>,
    /// A "Ja" on this page could not be written: the question stands again.
    again: bool,
}

/// The back target: read once it could be (an install may hold the medium), until then looked for again.
#[derive(Clone, Copy)]
enum Look {
    Known(Option<u32>),
    /// Not read yet: the next try at `at_ns`, then after `wait_ns` more (doubling up to `LOOK_MAX_NS`).
    Later { at_ns: u64, wait_ns: u64 },
}

static SEEN: SpinLock<Seen> = SpinLock::named("gp.report.ask", Seen { asked: None, back: Look::Later { at_ns: 0, wait_ns: LOOK_FIRST_NS }, after: None, again: false });

/// Seconds the page shows "Zurück zu Version M" before the restart.
const RESTART_S: u64 = 5;
const SEC: u64 = 1_000_000_000;
/// The report thread waits this long for the medium per look (it turns the pages too).
const LOOK_LOCK_NS: u64 = SEC / 2;
const LOOK_FIRST_NS: u64 = 5 * SEC;
const LOOK_MAX_NS: u64 = 60 * SEC;

/// The back target as shown: a look when one is due.
fn back_now() -> Back {
    let look = SEEN.lock().back;
    let now = time::now_ns();
    let look = match look {
        Look::Later { at_ns, wait_ns } if now >= at_ns => {
            let l = match confirm_ask::back_target(LOOK_LOCK_NS) {
                Ok(b) => {
                    klog::event!(Info, "kernel", "gp.report.ask_back", back = b.unwrap_or(0); "the page knows where Zurück goes");
                    Look::Known(b)
                }
                Err(why) => {
                    klog::event!(Info, "kernel", "gp.report.ask_back_later", reason = why.as_str(), again_s = wait_ns / SEC; "Zurück is offered once the medium could be read");
                    Look::Later { at_ns: now + wait_ns, wait_ns: (wait_ns * 2).min(LOOK_MAX_NS) }
                }
            };
            SEEN.lock().back = l;
            l
        }
        l => l,
    };
    match look {
        Look::Known(Some(b)) => Back::To(b),
        Look::Known(None) => Back::Nothing,
        Look::Later { .. } => Back::Unread,
    }
}

/// The question's state now (None: no question this boot).
pub(super) fn state() -> Option<(u32, State)> {
    let q = confirm_ask::question();
    let (asked, after) = {
        let s = SEEN.lock();
        (s.asked, s.after.clone())
    };
    let (v, g) = match (asked, q) {
        (Some(a), _) => a,
        (None, Some((v, g))) => {
            klog::event!(Info, "kernel", "gp.report.ask", version = v, generation = g; "the page asks Georg to confirm");
            SEEN.lock().asked = Some((v, g));
            (v, g)
        }
        (None, None) => return None,
    };
    if let Some(a) = after {
        if a == State::Confirming && confirmed() {
            return Some((v, State::Confirmed { by_georg: confirm_ask::by() == "georg" && confirm_ask::answered() == g }));
        }
        if a == State::Confirming && failed() {
            return Some((v, State::Failed(String::from("Die Bestätigung konnte nicht geschrieben werden."))));
        }
        if a == State::Confirming && q.is_some() {
            // The write after this "Ja" failed and the kernel asks again (`confirm_ask::answer_lost`).
            let mut s = SEEN.lock();
            s.after = None;
            s.again = true;
        } else {
            return Some((v, a));
        }
    }
    if q.is_some() {
        let again = SEEN.lock().again;
        return Some((v, State::Asking { back: back_now(), again, early: console::report_page::early() }));
    }
    if confirmed() {
        return Some((v, State::Confirmed { by_georg: false }));
    }
    // Replaced or failed without an answer here: the question is simply gone.
    None
}

fn confirmed() -> bool {
    matches!(kexec::install::confirm::status(), kexec::install::confirm::Status::Confirmed { .. })
}

fn failed() -> bool {
    matches!(kexec::install::confirm::status(), kexec::install::confirm::Status::Failed { .. } | kexec::install::confirm::Status::Replaced { .. })
}

fn after(s: Option<State>) {
    SEEN.lock().after = s;
}

/// The console's call for a real J (`yes`) or Z: on its input thread, so the slow part runs on a thread. Both
/// answers count only while the question still stands for the generation the page asked about; a proof that
/// confirmed meanwhile leaves the page to show that.
pub(super) fn answer(yes: bool) {
    let (asked, look) = {
        let s = SEEN.lock();
        (s.asked, s.back)
    };
    let Some((_, g)) = asked else { return };
    if confirm_ask::question().map(|q| q.1) != Some(g) {
        klog::event!(Info, "kernel", "gp.report.answer_late", generation = g, yes = yes; "the question was answered already: nothing changed");
        return;
    }
    if yes {
        SEEN.lock().again = false;
        match confirm_ask::answer(g, true) {
            Ok(()) => after(Some(State::Confirming)),
            Err(_) => after(Some(State::Failed(String::from("Die Antwort wurde nicht angenommen (die Frage war schon beantwortet).")))),
        }
        return;
    }
    let Look::Known(Some(to)) = look else { return };
    after(Some(State::Back { to }));
    if sched::spawn("gp.report.back", sched::Class::Normal, move || back_and_restart(g, to)).is_err() {
        after(Some(State::Failed(String::from("Zurück konnte nicht starten."))));
    }
}

fn back_and_restart(g: u32, to: u32) {
    if confirm_ask::question().map(|q| q.1) != Some(g) {
        klog::event!(Info, "kernel", "gp.report.back_refused", reason = "the question was answered already"; "nothing changed");
        after(None);
        return;
    }
    match kexec::install::update_node::pin_step(true, Some(to)) {
        Ok(s) => {
            let next = s.plan.next.0.number;
            klog::event!(Warn, "kernel", "gp.report.back", pin = next, wanted = to; "Georg chose Zurück on the report page: restarting");
            after(Some(State::Back { to: next }));
            sched::sleep_ms(RESTART_S * 1000);
            platform::power::reboot();
        }
        Err(why) => {
            klog::event!(Warn, "kernel", "gp.report.back_refused", reason = why.as_str(); "nothing changed");
            after(Some(State::Failed(String::from("Zurück hat nicht geklappt (der Grund steht im Protokoll)."))));
        }
    }
}

/// The page ended while the question still stands: one line at the prompt says how to get it back.
pub(super) fn after_page() {
    let busy = matches!(SEEN.lock().after, Some(State::Back { .. } | State::Confirming));
    if let Some((v, _)) = confirm_ask::question().filter(|_| !busy) {
        let t = alloc::format!("Version {v} ist noch nicht bestätigt. Die Frage kommt wieder mit: bitsupdate frage\n");
        console::Console::write_str(console::console(), &t, console::FG_PROMPT);
    }
}
