//! The page "KI"'s section of the helper programs found on this machine (`helpers`): each by its own name with
//! one click to make it a provider ("Übernehmen": kind Hilfsprogramm, its path, its place when it names one)
//! and one to let it answer in the chat ("Fürs Gespräch nehmen"). The folder is read when a page "KI" opens and
//! on "Neu suchen", never on every change.

use mcpreg::Json;

use crate::helpers::Helper;
use crate::ki::{self, Built, Facts, PROGRAM, Provider, plain};
use crate::ki_ask::What;
use crate::page::Kind;
use crate::page_draw::Tone;
use crate::settings_write::Req;
use crate::state::App;

pub const PREFIX: &str = "ki.helper.";
pub const RESCAN: &str = "ki.helpers";
pub const HEAD: &str = "HILFSPROGRAMME AUF DIESEM RECHNER";

/// The provider that already is helper `h` (kind Hilfsprogramm, the same program).
pub fn provider_of<'a>(f: &'a Facts, h: &Helper) -> Option<&'a Provider> {
    f.providers.iter().find(|p| p.kind == PROGRAM && p.address.trim() == h.path)
}

/// The name a new provider for `h` gets: its own, or with -2, -3 … when that is taken.
pub fn free_id(f: &Facts, h: &Helper) -> String {
    let taken = |id: &str| f.provider(id).is_some();
    if !taken(&h.id) {
        return h.id.clone();
    }
    (2..).map(|n| format!("{}-{n}", h.id)).find(|id| !taken(id)).unwrap_or_default()
}

pub fn section(b: &mut Built, f: &Facts, helpers: &[Helper]) {
    let from = b.model.rows.len();
    let chat = f.roles.get(&ki::role_key("chat", false)).map(|s| s.trim().to_string()).unwrap_or_default();
    for h in helpers {
        let key = format!("{PREFIX}{}", h.id);
        let label = format!("Hilfsprogramm: {}", h.name);
        let about = if h.about.is_empty() { h.path.clone() } else { h.about.clone() };
        match provider_of(f, h) {
            Some(p) if p.id == chat => {
                b.row(&key, Kind::Fixed("antwortet im Gespräch".into()), &label, &h.name, &format!("Anbieter „{}“", p.id), plain(&about));
            }
            Some(p) => {
                let sub = (format!("Eingerichtet als Anbieter „{}“. {about}", p.id), Tone::Plain);
                b.row(&key, Kind::Buttons(vec![("chat".into(), "Fürs Gespräch nehmen".into())]), &label, &h.name, "", sub);
            }
            None => b.row(&key, Kind::Buttons(vec![("adopt".into(), "Übernehmen".into())]), &label, &h.name, "", plain(&about)),
        }
    }
    let (label, sub) = if helpers.is_empty() {
        ("Keins gefunden", "Ein Hilfsprogramm meldet sich mit einer kleinen Beschreibung in ~/.local/share/bitsos/helpers an.")
    } else {
        ("Eins fehlt?", "Ein Hilfsprogramm meldet sich mit einer kleinen Beschreibung in ~/.local/share/bitsos/helpers an.")
    };
    b.row(RESCAN, Kind::Buttons(vec![("rescan".into(), "Neu suchen".into())]), label, "Hilfsprogramme", "", plain(sub));
    b.section(HEAD, from);
}

impl App {
    /// Reads the helpers' folder of the user bitscomp runs as.
    pub(crate) fn ki_helpers_scan(&mut self) {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let dir = crate::helpers::dir(uid);
        self.ki.helpers = crate::helpers::scan(&dir, uid);
        eprintln!("bitscomp: ki: {} helper(s) described in {}", self.ki.helpers.len(), dir.display());
    }

    /// A button of the helpers' section.
    pub(crate) fn ki_helper_act(&mut self, pid: u32, key: &str, button: &str) {
        if button == "rescan" {
            self.ki_helpers_scan();
            self.ki_refresh();
            let n = self.ki.helpers.len();
            let text = match n {
                0 => "Kein Hilfsprogramm gefunden.".to_string(),
                1 => "Ein Hilfsprogramm gefunden.".to_string(),
                _ => format!("{n} Hilfsprogramme gefunden."),
            };
            return self.page_say(pid, text, false);
        }
        let Some(h) = key.strip_prefix(PREFIX).and_then(|id| self.ki.helpers.iter().find(|h| h.id == id)).cloned() else { return };
        let f = ki::facts(&self.settings);
        match button {
            "adopt" => {
                if let Some(p) = provider_of(&f, &h) {
                    return self.page_say(pid, format!("„{}“ ist schon als Anbieter „{}“ eingetragen.", h.name, p.id), false);
                }
                // The program may have gone since the folder was read: assist would refuse it anyway.
                if crate::helpers::program(&h.path).is_err() {
                    return self.page_say(pid, format!("Das Programm von „{}“ ist nicht mehr da. „Neu suchen“ liest die Liste neu.", h.name), true);
                }
                let id = free_id(&f, &h);
                self.ki_add_start(pid, id, PROGRAM.into(), h.path.clone(), h.place.clone(), Some(h.name.clone()));
            }
            "chat" => {
                let Some(id) = provider_of(&f, &h).map(|p| p.id.clone()) else { return };
                let key = ki::role_key("chat", false);
                let now = f.roles.get(&key).map(|s| s.trim().to_string()).unwrap_or_default();
                if now.is_empty() || f.provider(&now).is_none() {
                    self.page_send(pid, Req::set(&key, Json::str(&id)));
                } else if now != id {
                    self.ki_ask(pid, What::Chat { id, before: now, name: h.name });
                }
            }
            _ => {}
        }
    }
}
