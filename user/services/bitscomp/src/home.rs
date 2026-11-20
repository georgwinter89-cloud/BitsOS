//! B (Georg 12.11.: "Fenster war auf und wird dann nur halb transparent wenn man auf Startseite drückt"): the
//! "Startseite" button. Over a workspace with windows it puts the start page in their place (`start_forced`): the
//! windows are neither drawn nor hit (`drawn_placements`) nor get keys, and a second press (or Esc) brings them
//! back where they were. The start page's image has no ground of its own (on an empty workspace the stage is
//! under it), so drawing the windows under it showed both at once. On an empty workspace the start page is there
//! already: the press says so instead of doing nothing visible.

use crate::state::App;

pub const HOME_NOTE: &str = "Das ist die Startseite dieses Arbeitsraums.";

impl App {
    pub fn home_press(&mut self) {
        let has = !self.ws.tiles.is_empty() || !self.ws.floating.is_empty();
        self.desk.start_forced = has && !self.desk.start_forced;
        self.desk.home_note = None;
        if !has {
            if self.buehne().is_some() {
                self.desk.home_note = Some((self.desk.active, crate::pacer::now() + crate::desk::NOTICE_FOR));
            } else {
                self.notice(HOME_NOTE);
            }
        }
        eprintln!(
            "bitscomp: start page: {}",
            match (has, self.desk.start_forced) {
                (false, _) => "already shown (empty workspace)",
                (true, true) => "over the windows",
                (true, false) => "windows back",
            }
        );
        self.chrome_dirty();
        self.needs_frame = true;
        // The pointer may now be over a window that was hidden (or the other way round).
        self.motion(self.now_ms());
    }

    /// The Bühne's hint line: the home note on its workspace, else `normal`.
    pub fn home_hint(&self, normal: String) -> String {
        match self.desk.home_note {
            Some((ws, _)) if ws == self.desk.active && self.ws.tiles.is_empty() && self.ws.floating.is_empty() => HOME_NOTE.into(),
            _ => normal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;

    #[test]
    fn a_press_on_an_empty_workspace_answers_and_forces_nothing() {
        let d = Display::<App>::new().expect("display");
        let mut app = App::new(d.handle(), "wy-home-test".into(), (2560, 1440));
        assert!(app.buehne().is_some(), "the Bühne is the default");
        app.home_press();
        assert!(!app.desk.start_forced && app.start_shown());
        assert_eq!(app.home_hint("0 Teile auf der Bühne".into()), HOME_NOTE);
        let until = app.desk.home_note.unwrap().1;
        assert_eq!(app.notice_tick(until - std::time::Duration::from_millis(1)), Some(until), "the tick wakes for it");
        app.notice_tick(until);
        assert_eq!(app.home_hint("0 Teile auf der Bühne".into()), "0 Teile auf der Bühne", "it goes after its time");
        // Another workspace does not show it.
        app.home_press();
        app.desk.active += 1;
        assert_eq!(app.home_hint("x".into()), "x");
        app.desk.active -= 1;
        // The classic preset has no hint line: a notice says it.
        app.settings.set("theme.layout.preset", "classic");
        app.apply_settings("theme.layout.preset");
        assert!(app.buehne().is_none());
        app.desk.notice = None;
        app.home_press();
        assert_eq!(app.desk.notice.as_ref().map(|(t, _)| t.as_str()), Some(HOME_NOTE));
        assert!(app.drawn_placements().is_empty());
    }
}
