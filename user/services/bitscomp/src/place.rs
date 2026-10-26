//! Where a new window goes (WY-5): a program opened from the address bar or the start page lands as a new tab of
//! the stack it was opened for, or in a new tile; a restored session's programs go back into their workspace and
//! the stacks they shared. Matched by the client's pid (SO_PEERCRED) against what bitscomp started; anything
//! else becomes a new tile of the shown workspace, as before.

use std::collections::HashMap;

use smithay::desktop::Window;

use crate::layout::{Place, Workspace};
use crate::state::App;
use crate::zones::Drop;

/// A restored program: its workspace and the stack (group) it shares with others.
pub struct Restore {
    pub pid: u32,
    pub ws: usize,
    pub group: u64,
}

/// The first window of each restored group that has mapped (the others stack onto its tile).
pub type Anchors = HashMap<u64, Window>;

fn stack_onto(ws: &mut Workspace<Window>, w: &Window, target: &Window) {
    if let Some(Place::Tiled { tile, .. }) = ws.find(target) {
        ws.move_window(w, Drop::Stack(tile));
        ws.focus_window(w);
    }
}

impl App {
    pub fn place_new(&mut self, w: Window) {
        let pid = self.pid_of(&w).max(0) as u32;
        if let Some(i) = self.pending_open.iter().position(|p| p.0 == pid) {
            let (_, target, _) = self.pending_open.remove(i);
            self.ws.add(w.clone());
            if let Some(t) = target {
                stack_onto(&mut self.ws, &w, &t);
            }
            self.ws.focus_window(&w);
            return self.note_opened(&w);
        }
        if let Some(i) = self.restoring.iter().position(|r| r.pid == pid) {
            let r = self.restoring.remove(i);
            let active = r.ws == self.desk.active || r.ws >= self.ws_count();
            let anchor = self.anchors.get(&r.group).cloned();
            let ws = if active { &mut self.ws } else { &mut self.desk.parked[r.ws] };
            ws.add_behind(w.clone());
            match anchor {
                Some(a) if ws.find(&a).is_some() => stack_onto(ws, &w, &a),
                _ => {
                    self.anchors.insert(r.group, w.clone());
                }
            }
            if !self.restoring.iter().any(|x| x.group == r.group) {
                self.anchors.remove(&r.group);
            }
            if !active {
                self.space.unmap_elem(&w);
            }
            self.chrome_dirty();
            return self.note_opened(&w);
        }
        self.ws.add_behind(w.clone());
        self.note_opened(&w);
    }

    fn note_opened(&mut self, _w: &Window) {
        self.desk.start_forced = false;
        // Whatever way the window came, the last session is no longer offered and this one is saved.
        self.session.take_up();
        self.session_dirty();
    }

    /// Opens and restores whose program has ended without a window are dropped.
    pub fn prune_pending(&mut self) {
        let alive: Vec<u32> = self.children.iter().map(|(c, _)| c.id()).collect();
        self.pending_open.retain(|p| alive.contains(&p.0));
        self.restoring.retain(|r| alive.contains(&r.pid));
    }
}
