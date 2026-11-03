//! Where a new window goes (WY-5): a program opened from the address bar or the start page lands as a new tab of
//! the stack it was opened for, or in a new tile; a restored session's programs go back into their workspace and
//! the stacks they shared. Matched by the client's pid (SO_PEERCRED) against what bitscomp started; anything
//! else becomes a new tile of the shown workspace, as before.

use std::collections::HashMap;

use crate::win::Window;

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

/// LO-1: which of the started programs `started` (pids, in order) a new window of process `pid` in process group
/// `group` belongs to: its own pid first, else its group (a launcher's program: soffice.bin is a child of
/// bitscomp's `sh libreoffice`, which `spawn` made the leader of its group).
pub fn slot(started: impl Iterator<Item = u32> + Clone, pid: u32, group: u32) -> Option<usize> {
    started.clone().position(|p| p == pid).or_else(|| started.into_iter().position(|p| p == group))
}

fn stack_onto(ws: &mut Workspace<Window>, w: &Window, target: &Window) {
    if let Some(Place::Tiled { tile, .. }) = ws.find(target) {
        ws.move_window(w, Drop::Stack(tile));
        ws.focus_window(w);
    }
}

impl App {
    pub fn place_new(&mut self, w: Window) {
        let pid = self.pid_of(&w).max(0) as u32;
        // LO-1: a launcher's program (soffice.bin under bitscomp's `sh libreoffice`) is in the launcher's group.
        let group = Self::group_of(pid);
        if let Some(i) = slot(self.pending_open.iter().map(|p| p.0), pid, group) {
            let (_, target, _) = self.pending_open.remove(i);
            self.ws.add(w.clone());
            if let Some(t) = target {
                stack_onto(&mut self.ws, &w, &t);
            }
            self.ws.focus_window(&w);
            return self.note_opened(&w);
        }
        if let Some(i) = slot(self.restoring.iter().map(|r| r.pid), pid, group) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    /// LO-1: a window of a launcher's child finds the launcher's slot (pending open or restore): a real process group
    /// as `spawn` makes it (the started program leads its own group), with a child under it as soffice.bin is.
    #[test]
    fn a_launchers_child_finds_the_launchers_slot() {
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(leader.stdout.take().unwrap()), &mut line).unwrap();
        let child: u32 = line.trim().parse().unwrap();
        let group = App::group_of(child);
        let _ = std::process::Command::new("kill").arg(child.to_string()).status();
        let _ = leader.wait();
        assert_eq!(group, leader.id(), "the child is in its launcher's group");
        let started = [7u32, leader.id(), 9];
        assert_eq!(slot(started.iter().copied(), child, group), Some(1), "the launcher's slot");
        assert_eq!(slot(started.iter().copied(), 9, App::group_of(9)), Some(2), "a program's own pid comes first");
        assert_eq!(slot(started.iter().copied(), child, child), None, "neither its pid nor its group: no slot");
        // A pid that is its own slot wins over a group that is another slot.
        assert_eq!(slot([leader.id(), child].iter().copied(), child, leader.id()), Some(1));
    }
}
