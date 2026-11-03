//! IN-9: a model fed by log events (the console's boot step list and, through it, the desktop start) sees its
//! events whatever the loglevel. At `loglevel=error` the level filter drops every Info line before the ring and
//! every sink; the watch gets the dropped events its owner asks for (`wants`, asked before the event's fields are
//! built) as a rendered line, at once in the emitter's context. Nothing else sees them: the ring, serial, the
//! mirror and the console stay filtered (Linux keeps below-level lines in its ring and filters only the console,
//! linux-6.6/kernel/printk/printk.c `suppress_message_printing`; here "no extra lines at error" wins).
//!
//! One watch (the kernel installs the console's before `kernel start`); Info and Warn only (Error is never
//! dropped, Debug and Trace are never watched, so their call sites stay one compare); never the panic/NMI path.
//! `feed` runs in the emitter's context under whatever locks it holds: it must not allocate and must take at
//! most a leaf lock (the console's step model does both, IN-9 fixer). Not rate limited on purpose: the step
//! events are a few per boot, and the model keeps one state per step whatever the rate.

use crate::{Event, Level, render};
use ksync::Once;

struct Watch {
    wants: fn(&str, &str) -> bool,
    feed: fn(&str),
}

static WATCH: Once<Watch> = Once::new();
crate::counter!(WATCHED, "klog.watched");

/// Installs the watch: `wants(subsystem, name)` picks events (compares only: it runs for every dropped Info/Warn
/// event), `feed(line)` takes the rendered line from any context but the panic/NMI path (no waiting). Once: a
/// second call keeps the first and returns false.
pub fn set_watch(wants: fn(&str, &str) -> bool, feed: fn(&str)) -> bool {
    let mut first = false;
    WATCH.get_or_init(|| {
        first = true;
        Watch { wants, feed }
    });
    first
}

/// The `event!` macro's second check (after the level): may the watch want an event of this level at all?
/// Without the subsystem and name, so a dropped Debug/Trace event never evaluates them.
#[doc(hidden)]
#[inline]
pub fn __watch_may(level: Level) -> bool {
    level <= Level::Info && WATCH.get().is_some()
}

/// The `event!` macro's third check: does the watch want this event?
#[doc(hidden)]
#[inline]
pub fn __watched(subsystem: &str, name: &str) -> bool {
    WATCH.get().is_some_and(|w| (w.wants)(subsystem, name))
}

/// `record`'s part for an event the level filter dropped.
pub(crate) fn dropped(ev: &Event<'_>) {
    let Some(w) = WATCH.get() else { return };
    if ev.level > Level::Info || !(w.wants)(ev.subsystem, ev.name) {
        return;
    }
    let mut line = render::LineBuf::new();
    render::line(&mut line, crate::now(), ev);
    WATCHED.inc();
    (w.feed)(line.as_str());
}
