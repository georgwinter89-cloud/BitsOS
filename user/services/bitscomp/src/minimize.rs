//! WY-12: minimise and restore (Georg 24.10.: Brave's own minimise button froze the screen, because bitscomp
//! ignored xdg_toplevel.set_minimized while Chromium stopped drawing). A minimised window is unmapped (not
//! drawn, no input), the focus goes to the next window, and it waits in its workspace's list until the window
//! list, Alt+Tab or its launcher tile restores it. xdg-shell has no minimized state: the client learns nothing
//! from the request itself, so bitscomp tells it `activated` is gone (and `fullscreen`, which minimising ends),
//! and on restore sends a configure with `activated`, the sign Chromium waits for to draw again.

use smithay::desktop::PopupManager;
use crate::win::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

use crate::address::{Open, normalise, of_window, resolve};
use crate::ctl::{names, win_id};
use crate::state::App;

/// The window list's name of a window: its title, else its program.
pub fn label(w: &Window) -> String {
    let (app_id, title) = names(w);
    if !title.trim().is_empty() { title } else if !app_id.is_empty() { app_id } else { "Fenster".into() }
}

/// Whether the launcher address `addr` starts the program of a window with `app_id` (`programm/terminal` and
/// `terminal` for foot, `programm/brave-browser` for Brave).
pub fn launches(addr: &str, app_id: &str) -> bool {
    let (a, mine) = (normalise(addr), of_window(app_id));
    a == mine || mine.strip_prefix("programm/") == Some(a.as_str()) || (resolve(addr) == Open::Terminal && mine == "programm/terminal")
}

impl App {
    /// Minimises `w` (its own request, `ctl minimize`, Super+Down); `why` goes into the log. A window of a parked
    /// workspace is minimised there. False when it is not shown anywhere (already minimised, gone) or has not
    /// had its first configure yet (it is not on the screen: nothing to hide).
    /// Its dialogs (xdg_toplevel parent) go with it, deepest first, so the focus never lands on a dialog that
    /// is about to be hidden too (KWin and Mutter minimise transient dialogs with their window).
    pub fn minimize(&mut self, w: &Window, why: &str) -> bool {
        if !w.toplevel().is_none_or(|t| t.is_initial_configure_sent()) {
            return self.minimize_one(w, why);
        }
        let family = self.dialog_tree(w);
        for d in family.iter().skip(1).rev() {
            if !self.ws.is_minimized(d) && !self.desk.parked.iter().any(|ws| ws.is_minimized(d)) {
                self.minimize_one(d, "with its parent window");
            }
        }
        self.minimize_one(w, why)
    }

    fn minimize_one(&mut self, w: &Window, why: &str) -> bool {
        let id = win_id(w);
        if !w.toplevel().is_none_or(|t| t.is_initial_configure_sent()) {
            eprintln!("bitscomp: window {id} asks to be minimised before its first configure: ignored");
            return false;
        }
        let active = self.ws.minimize(w);
        if !active && !self.desk.parked.iter_mut().any(|ws| ws.minimize(w)) {
            return false;
        }
        eprintln!("bitscomp: window {id} minimised ({why}){}", if active { "" } else { " on a parked workspace" });
        self.space.unmap_elem(w);
        self.dismiss_popups(w);
        self.tell_hidden(w);
        if self.grab.as_ref().is_some_and(|g| g.window() == Some(w)) {
            self.grab = None;
        }
        if self.menu.as_ref().is_some_and(|m| m.target.as_ref() == Some(w)) {
            self.menu = None;
        }
        if self.hover.as_ref().is_some_and(|(h, _)| h == w) {
            self.hover = None;
        }
        // A tile's own address bar may belong to the freed tile: tile indices move.
        if self.desk.edit.as_ref().is_some_and(|e| e.tile.is_some()) {
            self.address_end();
        }
        if active {
            // The keyboard focus moves in this pass, so no key goes to the hidden window.
            self.apply_layout();
            self.motion(self.now_ms());
        }
        self.chrome_dirty();
        true
    }

    /// `w` and every dialog below it (breadth first, `w` first); the visited check keeps a parent loop finite.
    fn dialog_tree(&self, w: &Window) -> Vec<Window> {
        let all = self.all_windows();
        let mut tree = vec![w.clone()];
        let mut i = 0;
        while i < tree.len() {
            let Some(s) = tree[i].toplevel().map(|t| t.wl_surface().clone()) else { i += 1; continue };
            for c in &all {
                if !tree.contains(c) && c.toplevel().and_then(|t| t.parent()).as_ref() == Some(&s) {
                    tree.push(c.clone());
                }
            }
            i += 1;
        }
        tree
    }

    /// The window to restore for `w`: its topmost parent that is minimised here (a dialog comes back with its
    /// window, never alone over a hidden one).
    fn minimized_root(&self, w: &Window) -> Window {
        let all = self.all_windows();
        let mut root = w.clone();
        let mut seen = vec![root.clone()];
        while let Some(p) = root.toplevel().and_then(|t| t.parent()) {
            match all.iter().find(|c| c.toplevel().is_some_and(|t| *t.wl_surface() == p)) {
                Some(c) if !seen.contains(c) && self.ws.is_minimized(c) => {
                    root = c.clone();
                    seen.push(root.clone());
                }
                _ => break,
            }
        }
        root
    }

    /// Menus and other popups of the hidden window are closed (popup_done): they would stay open in the
    /// client's mind, unseen, and come back stale with the restore.
    fn dismiss_popups(&self, w: &Window) {
        let Some(t) = w.toplevel() else { return };
        let s = t.wl_surface();
        let popups: Vec<_> = PopupManager::popups_for_surface(s).map(|(p, _)| p).collect();
        if !popups.is_empty() {
            eprintln!("bitscomp: window {} minimised: {} popup(s) closed", win_id(w), popups.len());
        }
        for p in popups {
            let _ = PopupManager::dismiss_popup(s, &p);
        }
    }

    /// The hidden window is no longer `activated` (nor fullscreen); a hung client gets nothing (B4), it gets
    /// the state on restore or when it answers again.
    fn tell_hidden(&self, w: &Window) {
        let Some(t) = w.toplevel() else { return };
        t.with_pending_state(|s| {
            s.states.unset(xdg_toplevel::State::Activated);
            s.states.unset(xdg_toplevel::State::Fullscreen);
        });
        if !self.is_hung(w) {
            t.send_pending_configure();
        }
    }

    /// Restores a minimised window of the shown workspace where it was and focuses it. Its configure carries
    /// `activated` and goes out even if nothing differs from the last one (a client that had been hung while
    /// minimised), so the client draws again.
    /// Its minimised dialogs come back after it (the last one keeps the focus, as it was on top).
    pub fn unminimize(&mut self, w: &Window, why: &str) -> bool {
        let root = self.minimized_root(w);
        if !self.unminimize_one(&root, why) {
            return false;
        }
        for d in self.dialog_tree(&root).iter().skip(1) {
            if self.ws.is_minimized(d) {
                self.unminimize_one(d, "with its parent window");
            }
        }
        true
    }

    fn unminimize_one(&mut self, w: &Window, why: &str) -> bool {
        if !self.ws.restore(w) {
            return false;
        }
        eprintln!("bitscomp: window {} restored ({why})", win_id(w));
        let (full, header) = (self.area_full(), self.theme.header_height);
        self.ws.fit_floats(full, header);
        let sent = self.placements().into_iter().find(|p| p.win == *w).is_some_and(|p| self.configure(&p));
        if let Some(t) = w.toplevel()
            && !sent
            && t.is_initial_configure_sent()
            && !self.is_hung(w)
        {
            t.send_configure();
        }
        self.desk.start_forced = false;
        self.apply_layout();
        self.motion(self.now_ms());
        self.chrome_dirty();
        true
    }

    /// Alt+Tab (`dir` 1) and Shift+Alt+Tab (-1): the next window, a minimised one is restored.
    pub fn switch_window(&mut self, dir: i32) {
        if let Some(w) = self.ws.switch_step(dir) {
            self.unminimize(&w, "Alt+Tab");
        }
    }

    /// A launcher tile (start page, favourites bar) restores the newest minimised window of its program on the
    /// shown workspace; false if there is none (the tile then starts the program as before).
    pub fn restore_program(&mut self, addr: &str) -> bool {
        // PK-L1a: a library program's windows are also known by its StartupWMClass.
        let entry = self.library.by_address(addr).cloned();
        let ids: Vec<String> = self.ws.minimized.iter().map(|m| names(&m.win).0).collect();
        let found = newest_of(addr, entry.as_ref(), &ids).map(|i| self.ws.minimized[i].win.clone());
        found.is_some_and(|w| self.unminimize(&w, "launcher tile"))
    }

    /// `ctl list` lines of the shown workspace's minimised windows (they have no placement).
    pub fn minimized_lines(&self) -> String {
        let mut out = String::new();
        for (i, m) in self.ws.minimized.iter().enumerate() {
            let (app_id, title) = names(&m.win);
            out.push_str(&format!(
                "minimized={i} visible=false focused=false id={} app_id={app_id} title={title:?} pid={} hung={} ws={}\n",
                win_id(&m.win),
                self.pid_of(&m.win),
                self.is_hung(&m.win),
                self.desk.active + 1
            ));
        }
        out
    }
}

/// The launcher rule's pick: the newest of the minimised windows (`app_ids`, oldest first) that `addr`'s program
/// owns, by its address or its library entry's StartupWMClass.
pub fn newest_of(addr: &str, entry: Option<&crate::library::Entry>, app_ids: &[String]) -> Option<usize> {
    app_ids.iter().rposition(|id| launches(addr, id) || entry.is_some_and(|e| e.owns(id)))
}

#[cfg(test)]
mod tests {
    use super::{launches, newest_of};

    #[test]
    fn launcher_tile_restores_the_newest_window_by_startup_wm_class() {
        // A program whose windows' app_id is not its address word: only StartupWMClass ties them together.
        let mut notes = crate::library::Index::builtin().entries[0].clone();
        notes.id = "notes".into();
        notes.address = "programm/notes".into();
        notes.wm_class = "org.example.Notizen".into();
        let ids: Vec<String> = ["org.example.Notizen", "foot", "Org.Example.Notizen", "foot"].iter().map(|s| s.to_string()).collect();
        assert_eq!(newest_of("programm/notes", Some(&notes), &ids), Some(2), "the newest Notizen window, foot left alone");
        assert_eq!(newest_of("programm/notes", None, &ids), None, "without the entry the address alone finds none");
        assert_eq!(newest_of("programm/terminal", None, &ids), Some(3));
        assert_eq!(newest_of("programm/notes", Some(&notes), &[]), None);
    }

    #[test]
    fn launcher_addresses_match_their_programs() {
        assert!(launches("programm/terminal", "foot"));
        assert!(launches("terminal", "foot"));
        assert!(launches("programm/brave-browser", "Brave-Browser"));
        assert!(launches("brave-browser", "brave-browser"));
        assert!(launches("programm/wyclient", "wyclient"));
        assert!(!launches("programm/terminal", "brave-browser"));
        assert!(!launches("einstellungen/aussehen", "foot"));
        assert!(!launches("programm/fenster", "wyclient"));
    }
}
