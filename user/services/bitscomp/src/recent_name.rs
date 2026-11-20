//! DESK-LEFT: the start page's "Zuletzt benutzt" names a deep settings address by its row, not by its last word
//! (`einstellungen/ki/abo/model` was "Model"): a row of the page "KI" as "Modell von „abo“", any other settings key
//! by its row's short name ("Akzentfarbe"), a key the page does not know by its group's name ("Aussehen"), the page
//! "KI" by its own title. Everything else keeps `suggest::short_name`.

use crate::page_kind::PageKind;
use crate::state::App;

/// The KI rows' own labels for when the page "KI" has not been built yet (ki.rs `provider_rows`).
const KI_FIELDS: [(&str, &str); 5] = [("kind", "Art"), ("place", "Ort"), ("address", "Adresse"), ("model", "Modell"), ("key", "Schlüssel")];

/// "Modell von „abo“" for a KI field `label` of provider `id`; "Anbieter „abo“" for the provider itself.
pub fn ki_title(id: &str, label: Option<&str>) -> String {
    match label {
        Some(l) => format!("{l} von „{id}“"),
        None => format!("Anbieter „{id}“"),
    }
}

impl App {
    pub fn recent_name(&self, addr: &str) -> String {
        let a = crate::address::normalise(addr);
        let a = a.trim_end_matches('/');
        let short = || crate::suggest::short_name(a);
        if PageKind::of_address(a) == PageKind::Ki {
            let key = PageKind::ki_prefix(a);
            let Some(rest) = key.strip_prefix("ai.provider.") else { return crate::ki::TITLE.into() };
            return match rest.split_once('.') {
                Some((id, field)) => {
                    let own = self.ki.built.model.rows.iter().find(|r| r.key == key).map(|r| r.label.clone());
                    let known = KI_FIELDS.iter().find(|(f, _)| *f == field).map(|(_, l)| l.to_string());
                    // A field nobody named: the provider it belongs to.
                    ki_title(id, own.or(known).as_deref())
                }
                None => ki_title(rest, None),
            };
        }
        // A settings key below its group (einstellungen/aussehen/akzent): the row's own short name, else its group's
        // name, else the group's address word (a namespace nobody named).
        match a.strip_prefix("einstellungen/") {
            Some(path) if path.contains('/') => {
                let key = crate::page::prefix_of(path);
                let row = self.page_model.rows.iter().find(|r| r.key == key).map(|r| if r.short.is_empty() { r.label.clone() } else { r.short.clone() });
                row.or_else(|| crate::page_schema::group_name(&key).map(String::from))
                    .unwrap_or_else(|| crate::suggest::short_name(path.split('/').next().unwrap_or(path)))
            }
            _ => short(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::wayland_server::Display;

    #[test]
    fn deep_settings_addresses_get_their_rows_names() {
        let d = Display::<App>::new().expect("display");
        let mut app = App::new(d.handle(), "wy-recent-name".into(), (2560, 1440));
        assert_eq!(app.recent_name("einstellungen/ki/abo/model"), "Modell von „abo“");
        assert_eq!(app.recent_name("einstellungen/ki/abo/address"), "Adresse von „abo“");
        assert_eq!(app.recent_name("einstellungen/ki/abo"), "Anbieter „abo“");
        assert_eq!(app.recent_name("einstellungen/ki"), "KI", "the page by its own title");
        assert_eq!(app.recent_name("einstellungen/ki/abo/zzz"), "Anbieter „abo“", "a field nobody named: its provider");
        assert_eq!(app.recent_name("einstellungen/aussehen"), "Aussehen");
        assert_eq!(app.recent_name("programm/terminal"), "Terminal");
        // A key the page knows: its short name; one it does not: its group's name, never the raw address word.
        app.page_model_set(crate::page_schema::model(&crate::page_schema::builtin()));
        let accent = app.page_model.rows.iter().find(|r| r.key == "theme.accent").map(|r| r.short.clone()).unwrap();
        assert_eq!(app.recent_name("einstellungen/aussehen/akzent"), accent);
        assert_eq!(app.recent_name("einstellungen/aussehen/gibtsnicht"), "Aussehen");
        assert_eq!(app.recent_name("einstellungen/aufbau/gibtsnicht"), "Aufbau");
        assert_eq!(app.recent_name("einstellungen/net/gibtsnicht"), "Netz");
        assert_eq!(app.recent_name("einstellungen/neuland/gibtsnicht"), "Neuland", "a namespace nobody named: its word");
    }
}
