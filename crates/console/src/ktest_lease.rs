//! The console for a ktest (desk fix/a2b). Since UL-3a init runs a BusyBox shell on the console: it is the
//! console's session, reads every key (a reader takes the bytes whatever the foreground group) and keeps its own
//! terminal settings (non-canonical, no echo, no ISIG). The console tests were written for a ktest boot, where
//! tests run before init and the console is free with the kernel monitor (`bits>`) at its prompt. A lease brings
//! back exactly that state for one test and then gives the console back to the shell:
//!
//! - take: the shell's process group gets SIGSTOP (init's wait4 has no WUNTRACED: no restart), the session and
//!   foreground group are saved and cleared, the settings saved and set to the VT's, queued input dropped; then
//!   the monitor finds the console free and shows its prompt (waited for, as the tests expect it there).
//! - drop: the session, foreground group and settings come back, test input is dropped, SIGCONT.
//!
//! A program in the shell's foreground (Georg's `vi`, say) is never stopped: the test is skipped then. Without
//! a session on the console (ktest boots, `init=off`) a lease changes nothing.

use crate::jobs::{self, Ctl, SIGCONT, SIGSTOP};
use crate::termios::Termios;
use alloc::format;
use alloc::string::String;

const MS: u64 = 1_000_000;

pub struct ConsoleLease {
    saved: Option<(Ctl, Termios)>,
}

impl ConsoleLease {
    /// Takes the console from its session for one test. `Err` is a failure or a skip (`ktest::skip` text), as a
    /// test returns it.
    pub fn take() -> Result<ConsoleLease, String> {
        let t = crate::tty();
        let p = jobs::procs();
        let c = t.ctl();
        let Some(sid) = c.session else { return Ok(ConsoleLease { saved: None }) };
        if c.pgrp != Some(sid) {
            let why = format!("a program runs in the foreground of the console's session {sid} (pgrp {:?}); the test would take its keys", c.pgrp);
            return Err(ktest::skip(&why).err().unwrap_or(why));
        }
        p.signal_pgrp(sid, SIGSTOP);
        let end = time::now_ns() + 5_000 * MS;
        while !p.pgrp_stopped(sid) {
            if time::now_ns() > end {
                p.signal_pgrp(sid, SIGCONT);
                return Err(format!("the console's session {sid} did not stop within 5 s"));
            }
            sched::sleep_ns(10 * MS);
        }
        let termios = t.termios();
        t.with_ctl(p, |cur, _| {
            if *cur == c {
                *cur = Ctl::default();
            }
        });
        t.set_termios(Termios::vt(), true);
        klog::event!(Info, "console", "ktest.lease", session = sid; "a ktest borrows the console: its shell is stopped until the test ends");
        wait_monitor(true);
        Ok(ConsoleLease { saved: Some((c, termios)) })
    }
}

/// Waits (at most 3 s) until the monitor is at its prompt (`at`) or has left it; nothing to wait for when no
/// monitor thread runs.
fn wait_monitor(at: bool) {
    let end = time::now_ns() + 3_000 * MS;
    while crate::monitor::running() && crate::monitor::at_prompt() != at && time::now_ns() < end {
        sched::sleep_ns(20 * MS);
    }
}

impl Drop for ConsoleLease {
    fn drop(&mut self) {
        let Some((c, termios)) = self.saved.take() else { return };
        // Keys the test injected that the VT has not taken yet would reach the shell after the flush below.
        let end = time::now_ns() + 2_000 * MS;
        while crate::vt::input_pending() > 0 && time::now_ns() < end {
            sched::sleep_ns(5 * MS);
        }
        sched::sleep_ns(50 * MS);
        let t = crate::tty();
        let p = jobs::procs();
        let sid = c.session.unwrap_or(0);
        // A test's own program that still holds the console is the test's to clear; the shell's session only
        // comes back to a free console.
        let back = t.with_ctl(p, |cur, _| {
            let free = cur.session.is_none();
            if free {
                *cur = c;
            }
            free
        });
        t.set_termios(termios, true);
        p.signal_pgrp(sid, SIGCONT);
        // The monitor leaves its prompt within a second once the console is held again; keys typed meanwhile
        // would be its lines.
        wait_monitor(false);
        if back {
            klog::event!(Info, "console", "ktest.lease_end", session = sid);
        } else {
            klog::event!(Warn, "console", "ktest.lease_lost", session = sid; "a test's program still held the console: the shell's session did not get it back");
        }
    }
}
