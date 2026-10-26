//! Settings applied live (WY-5): every message of the settings thread rebuilds the theme from the values; a
//! different theme replaces the old one at once (no restart), drops every image drawn with the old colours and
//! sizes, and with a new scale tells the output (clients get the new fractional scale). Favourites and the
//! assistant's name follow `desktop.favourites` and `claude.name`.

use std::rc::Rc;

use crate::settings::Msg;
use crate::state::App;
use crate::theme::Theme;

pub const DEFAULT_FAVOURITES: [&str; 2] = ["programm/terminal", "einstellungen/aussehen"];

impl App {
    pub fn settings_msg(&mut self, m: Msg) {
        match m {
            Msg::All(v, keys, specs) => {
                self.settings = v;
                self.setting_keys = keys;
                self.page_model_set(crate::page_schema::model(&specs));
                self.settings_live = true;
                self.apply_settings("list");
            }
            Msg::Changed(k, v) => {
                // WY-5c: the echo of a fold that a newer fold of the same bar already overtook.
                if (k == crate::desk::WORKSPACES_KEY || k == crate::desk::SIDEBAR_KEY)
                    && self.fold_echo_held(&k, crate::pacer::now())
                {
                    return;
                }
                match v {
                    Some(v) => self.settings.set(&k, &v),
                    None => self.settings.remove(&k),
                }
                self.apply_settings(&k);
            }
            Msg::Place(dir) => self.session_place(dir),
            Msg::Lost => {
                self.settings_live = false;
                self.page_dialogs_drop(None, "der Einstellungsdienst antwortet nicht");
            }
        }
    }

    /// Rebuilds the theme and the desk values from the settings; `why` names the change for the log line (B6).
    pub fn apply_settings(&mut self, why: &str) {
        let v = &self.settings;
        self.desk.favourites = v
            .list("desktop.favourites")
            .unwrap_or_else(|| DEFAULT_FAVOURITES.iter().map(|s| s.to_string()).collect());
        self.desk.claude_name =
            v.text("claude.name").map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Claude").to_string();
        let t = Theme::build(v);
        // A bar docked again (by anyone) ends its reveal.
        let docked = match self.desk.reveal.shown {
            Some(crate::reveal::Edge::Left) => t.layout.workspaces,
            Some(crate::reveal::Edge::Right) => t.layout.sidebar,
            None => false,
        };
        if docked {
            self.desk.reveal.clear();
        }
        if *self.theme != t {
            if t.font_ui != self.theme.font_ui && t.font_ui != "Exo 2" {
                eprintln!("bitscomp: theme.font.ui {:?}: only Exo 2 is shipped, it stays", t.font_ui);
            }
            let rescale = t.scale != self.theme.scale;
            // WY-11: a new pointer size: a new image (the scale changes it by itself) and the programs' XCURSOR_SIZE.
            if t.cursor_size != self.cursor.size {
                self.cursor.size = t.cursor_size;
                crate::terminal::cursor_env(|k| std::env::var(k).ok(), &mut self.spawn_env, t.cursor_size);
            }
            eprintln!(
                "bitscomp: theme applied ({why}): mode={} accent={} scale={} layout address={} favourites={} workspaces={} sidebar={} status={}",
                if t.dark { "dark" } else { "light" },
                v.text("theme.accent").unwrap_or(crate::theme::ACCENT),
                t.scale,
                if t.layout.address_top { "top" } else { "tile" },
                t.layout.favourites,
                t.layout.workspaces,
                t.layout.sidebar,
                t.layout.statusbar
            );
            self.theme = Rc::new(t);
            self.theme_gen += 1;
            self.deco_img.clear();
            self.corner_img.clear();
            self.menu_img = None;
            self.hung_img.clear();
            if rescale && let Some(mode) = self.output.current_mode() {
                self.set_mode((mode.size.w, mode.size.h));
            }
            self.layout_dirty = true;
        }
        // WY-5b: foot.ini follows (only rewritten when its text changes), running terminals follow the mode.
        self.foot_sync();
        self.chrome_dirty();
    }
}
