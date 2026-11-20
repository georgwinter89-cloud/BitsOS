//! DESK-LEFT: the small x on a minimised window's chip (Bühne), entry (classic) or line in the "+N" list closes the
//! window without restoring it, by the same request as the head's x (`xwm::close`). A program may answer with a
//! question of its own (an unsaved document): a new window of the same program while the close is pending, or the
//! window still there after `ANSWER_AFTER` (a question drawn inside the window, or a program that hangs), brings
//! the window back, so the question is seen and not answered blind. When the user went to another workspace
//! meanwhile, the window's own workspace is shown again for it.

use std::time::Duration;

use crate::ctl::win_id;
use crate::state::App;
use crate::win::Window;

/// How long a closed minimised window may stay before it is shown again.
pub const ANSWER_AFTER: Duration = Duration::from_secs(3);

pub struct Closing {
    pub win: Window,
    /// The program's pid (0 when unknown: then only a dialog of the window counts).
    pub pid: i32,
    pub due: Duration,
}

/// Whether `new` (its pid, its parent window) answers the close of `c`: a dialog of the window, or any new window
/// of the same program.
pub fn answers(c_pid: i32, c_is_parent: bool, new_pid: i32) -> bool {
    c_is_parent || (c_pid > 0 && c_pid == new_pid)
}

impl App {
    /// The x of minimised window `i` of the shown workspace.
    pub fn min_close(&mut self, i: usize, why: &str) {
        let Some(w) = self.ws.minimized.get(i).map(|m| m.win.clone()) else { return };
        self.desk.min_list.open = false;
        eprintln!("bitscomp: window {} closed while minimised ({why})", win_id(&w));
        let due = crate::pacer::now() + ANSWER_AFTER;
        let pid = self.pid_of(&w);
        self.desk.min_closing.retain(|c| c.win != w);
        self.desk.min_closing.push(Closing { win: w.clone(), pid, due });
        crate::xwm::close(&w);
        self.chrome_dirty();
    }

    /// A new window (its first frame, an X11 window's map): when it belongs to a minimised window being closed,
    /// that window comes back with it.
    pub fn min_close_answer(&mut self, new: &Window) {
        if self.desk.min_closing.is_empty() {
            return;
        }
        let pid = self.pid_of(new);
        let parent = self.parent_of(new);
        let hit = self.desk.min_closing.iter().position(|c| &c.win != new && answers(c.pid, parent.as_ref() == Some(&c.win), pid));
        if let Some(k) = hit {
            let c = self.desk.min_closing.remove(k);
            if self.minimised_on(&c.win).is_some() {
                eprintln!("bitscomp: window {} answers the close of minimised window {}: restored", win_id(new), win_id(&c.win));
                self.min_close_restore(&c.win, Some(new), "its program asks before closing");
            }
        }
    }

    /// The closes still pending; a window still minimised at its deadline comes back.
    pub fn min_close_tick(&mut self, now: Duration) -> Option<Duration> {
        let (due, later): (Vec<Closing>, Vec<Closing>) = std::mem::take(&mut self.desk.min_closing).into_iter().partition(|c| now >= c.due);
        self.desk.min_closing = later;
        for c in due {
            if self.minimised_on(&c.win).is_some() {
                eprintln!("bitscomp: minimised window {} still there {} s after its close: restored", win_id(&c.win), ANSWER_AFTER.as_secs());
                self.min_close_restore(&c.win, None, "it did not close");
            }
        }
        // A window that went (or came back another way) needs no deadline; one minimised on another workspace (the
        // user went there meanwhile) keeps it.
        let keep: Vec<bool> = self.desk.min_closing.iter().map(|c| self.minimised_on(&c.win).is_some()).collect();
        let mut keep = keep.into_iter();
        self.desk.min_closing.retain(|_| keep.next().unwrap_or(false));
        self.desk.min_closing.iter().map(|c| c.due).min()
    }

    /// The workspace whose minimised windows hold `w`: the shown one or a parked one.
    fn minimised_on(&self, w: &Window) -> Option<usize> {
        if self.ws.is_minimized(w) {
            return Some(self.desk.active);
        }
        (0..self.desk.parked.len()).find(|&i| i != self.desk.active && self.desk.parked[i].is_minimized(w))
    }

    /// Brings closed minimised window `w` back where it was, its workspace shown first when the user went to another
    /// one meanwhile, so the question (`with`, which came on the workspace shown then) is seen beside its window.
    fn min_close_restore(&mut self, w: &Window, with: Option<&Window>, why: &str) {
        let Some(i) = self.minimised_on(w) else { return };
        let from = self.desk.active;
        if i != from {
            eprintln!("bitscomp: workspace {} shown again for minimised window {} ({why})", i + 1, win_id(w));
            self.ws_switch(i);
            if let Some(n) = with
                && self.desk.parked[from].find(n).is_some()
            {
                self.desk.parked[from].remove(n);
                self.ws.add(n.clone());
            }
        }
        self.unminimize(w, why);
        if let Some(n) = with {
            self.ws.focus_window(n);
        }
        self.apply_layout();
    }

    /// The window a dialog belongs to: an xdg toplevel's parent, an X11 window's WM_TRANSIENT_FOR.
    fn parent_of(&self, w: &Window) -> Option<Window> {
        let all = self.all_windows();
        if let Some(x) = w.x11() {
            let t = x.transient_for.get();
            return (t != 0).then(|| all.into_iter().find(|c| c.x11().is_some_and(|p| p.xid == t))).flatten();
        }
        let p = w.toplevel()?.parent()?;
        all.into_iter().find(|c| c.toplevel().is_some_and(|t| *t.wl_surface() == p))
    }
}

#[cfg(test)]
mod tests {
    use super::answers;

    #[test]
    fn a_dialog_or_a_window_of_the_same_program_answers() {
        assert!(answers(42, false, 42), "same program");
        assert!(answers(0, true, 7), "a dialog of the window, pid unknown");
        assert!(!answers(42, false, 43), "another program");
        assert!(!answers(0, false, 0), "two unknown pids are not the same program");
    }
}
