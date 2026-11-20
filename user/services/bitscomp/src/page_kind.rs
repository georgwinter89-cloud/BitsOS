//! PK-7a: the kinds of bitscomp's own page windows. "Einstellungen" (WY-5b) and "Programme" (PK-7a) share the
//! generic part of a page (page_app: the window, scroll, hit, the edited field, the confirmation dialog, the
//! cached image); what a page shows and does is its kind's (settings: page, page_draw; programme: programme_*).
//! CL-5: the page "KI" (`einstellungen/ki`) is a kind of its own inside the settings window: same app id and
//! title, the settings page's layout and drawing, its own rows and actions (ki, ki_app).

use crate::layout::Rect;
use crate::page::Part;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageKind {
    #[default]
    Settings,
    Programme,
    Ki,
    /// DB2: "Wer hat was gemacht" (werhatwas, werhatwas_app), a settings window like "KI".
    WerHatWas,
    /// SYS-A: the program "System" (`system`, `system/<tab>`; sys_app and its files), a window of its own.
    System,
    /// UPDATE: "Aktualisieren" (update_page, update_app), a settings window like "KI".
    Update,
}

/// The address of the page "KI"; a provider's name may follow (`einstellungen/ki/<name>` shows its section).
pub const KI: &str = "einstellungen/ki";

pub const SETTINGS_ID: &str = "bitscomp.einstellungen";
pub const PROGRAMME_ID: &str = "bitscomp.programme";

impl PageKind {
    /// The kind of a (normalised) address: `programme` and below it is the program page, `einstellungen/ki`
    /// and below it the page "KI", the rest settings.
    pub fn of_address(a: &str) -> PageKind {
        let a = crate::address::normalise(a);
        let a = a.trim_end_matches('/');
        if a == "programme" || a.starts_with("programme/") {
            PageKind::Programme
        } else if a == KI || a.starts_with("einstellungen/ki/") {
            PageKind::Ki
        } else if a == crate::werhatwas::ADDRESS || crate::sys_wer::program_of(a).is_some() {
            PageKind::WerHatWas
        } else if a == crate::update_page::ADDRESS {
            PageKind::Update
        } else if crate::sys_view::tab_of(a).is_some() {
            PageKind::System
        } else {
            PageKind::Settings
        }
    }

    /// The kind of a page window by its app id; the page "KI" is a settings window.
    pub fn of_app_id(id: &str) -> Option<PageKind> {
        match id {
            SETTINGS_ID => Some(PageKind::Settings),
            PROGRAMME_ID => Some(PageKind::Programme),
            crate::sys_view::APP_ID => Some(PageKind::System),
            _ => None,
        }
    }

    pub fn app_id(self) -> &'static str {
        match self {
            PageKind::Settings | PageKind::Ki | PageKind::WerHatWas | PageKind::Update => SETTINGS_ID,
            PageKind::Programme => PROGRAMME_ID,
            PageKind::System => crate::sys_view::APP_ID,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            PageKind::Settings | PageKind::Ki | PageKind::WerHatWas | PageKind::Update => "Einstellungen",
            PageKind::Programme => "Programme",
            PageKind::System => crate::sys_view::TITLE,
        }
    }

    /// The key prefix the page "KI" jumps to for its address: the named provider's section, or one of its rows
    /// (`einstellungen/ki/<name>/model`, where the sidebar's model list sends Georg for other models).
    pub fn ki_prefix(a: &str) -> String {
        let a = crate::address::normalise(a);
        let mut parts = a.trim_end_matches('/').strip_prefix("einstellungen/ki/").unwrap_or("").split('/');
        match (parts.next(), parts.next()) {
            (Some(id), Some(field)) if !id.is_empty() && !field.is_empty() && field.bytes().all(|b| b.is_ascii_lowercase()) => {
                format!("ai.provider.{id}.{field}")
            }
            (Some(id), _) if !id.is_empty() => format!("ai.provider.{id}"),
            _ => String::new(),
        }
    }
}

/// The part at (x, y) of a page's content `c` (the first of `parts` that holds it), else its body.
pub fn hit(c: Rect, parts: &[(Part, Rect)], x: f64, y: f64) -> Option<Part> {
    if !c.contains(x, y) {
        return None;
    }
    Some(parts.iter().find(|(_, b)| b.contains(x, y)).map_or(Part::Body, |(p, _)| *p))
}

/// The largest scroll that still shows a page's end (`total` high in content `h` high).
pub fn max_scroll(total: i32, h: i32) -> i32 {
    (total + 24 - h).max(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_pick_the_kind_and_its_window_names() {
        for a in ["programme", "programme/", "Programme/hinzufuegen", "apps/", "apps", "programme/foot"] {
            assert_eq!(PageKind::of_address(a), PageKind::Programme, "{a}");
        }
        for a in ["einstellungen", "einstellungen/programme", "settings/aussehen", "programm/terminal", "programmes", "einstellungen/kinder", "einstellungen/ai"] {
            assert_eq!(PageKind::of_address(a), PageKind::Settings, "{a}");
        }
        for a in ["einstellungen/ki", "Einstellungen/KI/", "settings/ki", "einstellungen/ki/strata"] {
            assert_eq!(PageKind::of_address(a), PageKind::Ki, "{a}");
        }
        assert_eq!(PageKind::ki_prefix("einstellungen/ki/strata"), "ai.provider.strata");
        assert_eq!(PageKind::ki_prefix("einstellungen/ki/"), "");
        assert_eq!(PageKind::ki_prefix("einstellungen/ki/abo/model"), "ai.provider.abo.model");
        assert_eq!(PageKind::ki_prefix("einstellungen/ki/abo/Mo-del"), "ai.provider.abo");
        for a in ["system", "System/Prozesse", "system/geraete/"] {
            assert_eq!(PageKind::of_address(a), PageKind::System, "{a}");
        }
        assert_eq!((PageKind::System.app_id(), PageKind::System.title()), ("bitscomp.system", "System"));
        for k in [PageKind::Settings, PageKind::Programme, PageKind::System] {
            assert_eq!(PageKind::of_app_id(k.app_id()), Some(k));
        }
        // The page "KI" lives in the settings window.
        assert_eq!(PageKind::Ki.app_id(), SETTINGS_ID);
        assert_eq!(PageKind::of_app_id("foot"), None);
        assert_eq!(PageKind::Programme.title(), "Programme");
        for a in ["werhatwas", "Wer-hat-was", "einstellungen/werhatwas", "settings/werhatwas/"] {
            assert_eq!(PageKind::of_address(a), PageKind::WerHatWas, "{a}");
        }
        assert_eq!(PageKind::WerHatWas.app_id(), SETTINGS_ID);
    }

    #[test]
    fn hit_and_scroll_are_shared() {
        let c = Rect { x: 10, y: 10, w: 100, h: 100 };
        let parts = [(Part::Undo, Rect { x: 20, y: 20, w: 10, h: 10 })];
        assert_eq!(hit(c, &parts, 25.0, 25.0), Some(Part::Undo));
        assert_eq!(hit(c, &parts, 60.0, 60.0), Some(Part::Body));
        assert_eq!(hit(c, &parts, 5.0, 5.0), None);
        assert_eq!(max_scroll(50, 100), 0);
        assert_eq!(max_scroll(500, 100), 424);
    }
}
