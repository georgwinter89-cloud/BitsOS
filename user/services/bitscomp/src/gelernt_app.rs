//! DB4: the page "Was BitsOS über dich gelernt hat" in the settings window: its rows follow ctxd's list, "Löschen"
//! forgets one entry at once (it can be learned anew), "Alles vergessen" asks first in the page's own dialog.

use crate::gelernt;
use crate::page::{Kind, Row};
use crate::page_draw::Tone;
use crate::state::App;

impl App {
    /// ctxd's list changed: the page's rows anew (a page shown keeps its scroll; its image is redrawn).
    pub fn gelernt_changed(&mut self) {
        let rows = gelernt::rows(&self.habit.learned);
        gelernt::set_now(rows.clone());
        let m = gelernt::splice(&self.page_model, &rows);
        if m != self.page_model {
            self.page_model_set(m);
            self.needs_frame = true;
        }
    }

    /// The lines under the learned rows (merged into `subs`, one per row).
    pub fn gelernt_subs(&self, rows: &[Row], subs: &mut Vec<Option<(String, Tone)>>) {
        if !rows.iter().any(|r| gelernt::is_own(&r.key)) {
            return;
        }
        subs.resize(rows.len(), None);
        for (i, r) in rows.iter().enumerate() {
            if let Some(s) = gelernt::sub_of(&self.habit.learned, &r.key) {
                subs[i] = Some((s, Tone::Plain));
            }
        }
    }

    /// A button of a learned row: "Löschen" forgets it, "Alles vergessen …" asks first. False: not ours.
    pub fn gelernt_button(&mut self, pid: u32, r: usize) -> bool {
        let Some(row) = self.page_model.rows.get(r).cloned() else { return false };
        let (Some(id), Kind::Buttons(_)) = (row.key.strip_prefix(gelernt::PREFIX), &row.kind) else { return false };
        if row.key == gelernt::ALL_KEY {
            let lines = vec![
                "BitsOS vergisst alles, was es auf diesem Rechner über dich gelernt hat: Gewohnheiten, Zeiten, was du oft wählst, welche Dateien zusammengehören, das Suchgedächtnis.".into(),
                "Deine Arbeitsbereiche bleiben; die löschst du einzeln. Das lässt sich nicht rückgängig machen.".into(),
            ];
            self.address_end();
            self.page_edit_end();
            if let Some(s) = self.pages.by_pid.get_mut(&pid) {
                s.confirm = Some(crate::page_confirm::plain("Alles Gelernte vergessen?".into(), lines, "Vergessen"));
                s.msg = None;
            }
            self.habit.asking.insert(pid, "all".into());
            self.needs_frame = true;
        } else {
            self.habit_forget(pid, id);
        }
        true
    }

    pub fn gelernt_asks(&self, pid: u32) -> bool {
        self.habit.asking.contains_key(&pid)
    }

    /// The answer to "Alles Gelernte vergessen?".
    pub fn gelernt_confirmed(&mut self, pid: u32, yes: bool) {
        let Some(what) = self.habit.asking.remove(&pid) else { return };
        if yes {
            eprintln!("bitscomp: settings page {pid}: Georg confirmed forgetting {what}");
            self.habit_forget(pid, &what);
        } else {
            self.page_say(pid, "Nichts vergessen (abgebrochen).", false);
        }
    }

    /// ctxd's answer to a forget, on the page that asked.
    pub fn gelernt_said(&mut self, pid: u32, text: String) {
        self.habit.said = Some(text.clone());
        if pid != 0 {
            self.page_say(pid, text, false);
        }
    }
}
