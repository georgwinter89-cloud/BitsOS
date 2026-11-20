//! EINS-B (PLAN K5, K4): what a control socket peer inside the assistant's process tree never gets, before
//! `ctl_bits_gate` judges its line by the level (`ctl_bits::take`, the one gate): no list, picture or history of a
//! program on `assistant.never_see` (as `fenster`, `sehen` and `foto`), and no local `set`/`unset` that gives the
//! assistant more freedom (PLAN K2).

use crate::assist_level::{self as lv, Level};
use crate::state::App;
use crate::ui_reg::Full;

pub const SCREEN_HIDDEN: &str = "Auf dem Bildschirm ist ein Fenster, das der Assistent nicht sieht; ein Bild des Bildschirms gibt es darum nicht.";
pub const UI_HIDDEN: &str = "Ein Fenster, das der Assistent nicht sieht, ist offen; „bits ui <fenster>“ liest ein einzelnes anderes Fenster.";
pub const NO_LOOSENING: &str = "Mehr Freiheit für den Assistenten gibt nur Georgs eigene Hand, auf der Seite Einstellungen › Assistent.";

/// `list`'s lines with the title of a never-see program's window left out (its program stays, as in `fenster`).
pub fn hide_titles(list: &str, hidden: &dyn Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for line in list.lines() {
        let (a, t, p) = (line.find(" app_id="), line.find(" title="), line.rfind(" pid="));
        match (a, t, p) {
            (Some(a), Some(t), Some(p)) if a < t && t < p && hidden(&line[a + 8..t]) => {
                out.push_str(&format!("{} title={:?}{}", &line[..t], lv::NOT_SEEN, &line[p..]));
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

impl App {
    /// What the tree may not have (None: the command runs on): a never-see program's list, picture or history,
    /// and a local `set`/`unset` that gives the assistant more freedom.
    pub(crate) fn tree_private(&self, words: &[&str]) -> Option<Result<String, String>> {
        let v = &self.settings;
        let hidden = |app: &str| lv::never_sees(v, app);
        let first = *words.first()?;
        if first == "set" || first == "unset" {
            let key = *words.get(1)?;
            let old = v.text(key).unwrap_or("");
            // `unset` goes back to the key's default (the level's is Fragt vorher).
            let default = || self.page_model.rows.iter().find(|r| r.key == key).map(|r| r.default.clone()).unwrap_or_else(|| if key == lv::LEVEL { Level::Ask.word().into() } else { String::new() });
            let new = if first == "set" { words[2..].join(" ") } else { default() };
            return crate::page_join::loosens(key, old, &new).then(|| Err(NO_LOOSENING.into()));
        }
        if !matches!(first, "shot" | "ui" | "list" | "ui-history") || lv::never_list(v).is_empty() {
            return None;
        }
        // Every window by its program (no element read): which of them the assistant never sees.
        let all = self.ui_windows(&Full::One(String::new()));
        let hidden_open: Vec<&uilist::Window> = all.iter().filter(|w| hidden(&w.app)).collect();
        match first {
            "shot" => (!hidden_open.is_empty()).then(|| Err(SCREEN_HIDDEN.into())),
            "ui" => {
                let rest = if words.get(1) == Some(&"text") { &words[2..] } else { &words[1..] };
                match rest.first().copied() {
                    None => (!hidden_open.is_empty()).then(|| Err(UI_HIDDEN.into())),
                    Some(w) => {
                        let id = if w == "focus" { self.ui_focused_id() } else { w.to_string() };
                        hidden_open.iter().any(|h| h.window == id).then(|| Err(format!("{}. Es wurde nichts gelesen.", lv::NOT_SEEN)))
                    }
                }
            }
            "list" => Some(Ok(hide_titles(&crate::ctl::list(self), &hidden).trim_end().to_string())),
            _ => {
                let lines = crate::ui_history::with(|h| h.lines(None, crate::ui_history::now_s()));
                // A wrong number goes on to `ui-history` itself, which says so (and shows nothing).
                let secs = match words.get(1) {
                    Some(s) => Some(s.parse::<u64>().ok()?),
                    None => None,
                };
                let from = secs.map_or(0, |s| crate::ui_history::now_s().saturating_sub(s));
                let kept: Vec<&str> = lines
                    .lines()
                    .filter(|l| {
                        let e: serde_json::Value = serde_json::from_str(l).unwrap_or_default();
                        let app = e.get("app").and_then(serde_json::Value::as_str).unwrap_or("");
                        e.get("t").and_then(serde_json::Value::as_u64).unwrap_or(0) >= from && !hidden(app)
                    })
                    .collect();
                Some(Ok(kept.join("\n")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_lines_of_a_never_see_program_keep_only_its_program() {
        let list = "tile=0 page=0/1 visible=true focused=true frame=0,0 10x10 id=3 app_id=keepassxc title=\"Bank – pid=7\" pid=41 hung=false ws=1\n\
                    tile=1 page=0/1 visible=true focused=false frame=10,0 10x10 id=4 app_id=foot title=\"~\" pid=42 hung=false ws=1\n";
        let out = hide_titles(list, &|a| a == "keepassxc");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("tile=0 page=0/1 visible=true focused=true frame=0,0 10x10 id=3 app_id=keepassxc title={:?} pid=41 hung=false ws=1", lv::NOT_SEEN));
        assert!(!out.contains("Bank"), "{out}");
        assert_eq!(lines[1], list.lines().nth(1).unwrap(), "other programs as they are");
    }

    #[test]
    fn the_tree_never_reads_a_never_see_program_nor_loosens_by_a_local_set() {
        let d = smithay::reexports::wayland_server::Display::<App>::new().unwrap();
        let mut a = App::new(d.handle(), "tree-test".into(), (2000, 1125));
        let w = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let ask = |a: &App, s: &str| {
            let v = w(s);
            a.tree_private(&v.iter().map(String::as_str).collect::<Vec<_>>())
        };
        // Nothing on the list: everything reads as before.
        for c in ["shot /x.ppm", "ui", "ui text", "list", "ui-history"] {
            assert_eq!(ask(&a, c), None, "{c}");
        }
        // The desktop's program on the list (it is always on the screen).
        a.settings.set(lv::NEVER_SEE, "bitscomp");
        assert_eq!(ask(&a, "shot /x.ppm"), Some(Err(SCREEN_HIDDEN.into())));
        assert_eq!(ask(&a, "ui"), Some(Err(UI_HIDDEN.into())));
        assert_eq!(ask(&a, "ui text"), Some(Err(UI_HIDDEN.into())));
        assert!(ask(&a, "ui desktop").is_some_and(|r| r.is_err_and(|e| e.starts_with(lv::NOT_SEEN))));
        assert!(ask(&a, "ui text focus").is_some_and(|r| r.is_err()), "the desktop has the keyboard");
        assert_eq!(ask(&a, "ui w77"), None, "another window: `ui` itself answers");
        assert!(matches!(ask(&a, "list"), Some(Ok(_))), "list: filtered here");
        assert_eq!(ask(&a, "ui-history abc"), None, "a wrong number: `ui-history` says so itself");
        let ev = |app: &str, element: &str| uilist::Event { t: crate::ui_history::now_s(), kind: "press".into(), app: app.into(), window: "w1".into(), element: element.into(), text: None, name: None, by: None };
        crate::ui_history::with(|h| {
            h.push(ev("bitscomp", "geheim-tree-test"), false);
            h.push(ev("foot", "offen-tree-test"), false);
        });
        let Some(Ok(hist)) = ask(&a, "ui-history 60") else { panic!("history filtered here") };
        assert!(hist.contains("offen-tree-test") && !hist.contains("geheim-tree-test"), "{hist}");
        // A local set that gives more freedom: refused at every level; one that gives less runs.
        a.settings.set(lv::LEVEL, "answer");
        for c in ["set assistant.never_see foot", "unset assistant.never_see", "set assistant.sidebar_sees true", "set ai.level auto", "unset ai.level"] {
            assert_eq!(ask(&a, c), Some(Err(NO_LOOSENING.into())), "{c}");
        }
        for c in ["set assistant.never_see bitscomp, foot", "set theme.mode dark", "set ai.level answer"] {
            assert_eq!(ask(&a, c), None, "{c}");
        }
    }
}
