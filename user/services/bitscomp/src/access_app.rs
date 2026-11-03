//! MC-10: the full-access dialog on the `App` side (model and look: `access_dialog`): opened by the installer's
//! access question or by Georg himself with Strg+Alt+V at the real keyboard; the choice goes back to bitsinstall
//! (`install_link::access`), which keeps the grant and tells the kernel. While a grant runs the status bar shows
//! "Fernzugriff offen bis HH:MM" (the installer's `access` lines; dropped here at its end by bitscomp's own clock).

use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::access_dialog::{AStep, AccessDialog, OWN_TEXT, chip, title};
use crate::input::Origin;
use crate::install_link::{self as link, AccessShown, ClientShown};
use crate::state::App;

fn now() -> Duration {
    crate::pacer::now()
}

fn fp_short(fp: &str) -> String {
    fp.as_bytes().chunks(4).take(4).filter_map(|c| std::str::from_utf8(c).ok()).collect::<Vec<_>>().join(" ")
}

impl App {
    /// Strg+Alt+V: Georg opens full access himself (real keyboard only; injected: logged, ignored).
    pub fn access_open(&mut self) {
        if self.input_origin == Origin::Injected {
            self.install_note("injected Strg+Alt+V ignored: full access is set at the real keyboard only".into());
            return;
        }
        if self.install.shown() {
            return;
        }
        if !self.install.connected() {
            self.install_note("Strg+Alt+V: no installer connected".into());
            self.notice("Kein Installer verbunden: Vollzugriff geht nicht.");
            return;
        }
        let Some(c) = self.install.clients.first().cloned() else {
            self.install_note("Strg+Alt+V: no paired client".into());
            self.notice("Kein Gaming-PC gekoppelt (host/mcp/pair.mjs).");
            return;
        };
        let live = !self.install.access_live.is_empty();
        self.cancel_pointer_modes();
        self.address_end();
        self.page_edit_end();
        self.install.access = Some(AccessDialog::new(None, c.client, c.name, 24, OWN_TEXT.to_string(), live, now(), None));
        self.install_note("access dialog opened at the keyboard".into());
        self.install_changed();
    }

    /// The installer's access question (one question at a time: a second one is answered no).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn access_ask(&mut self, id: u64, text: String, timeout_ms: u64, client: String, name: String, hours: i64) {
        if self.install.ask.is_some() || self.install.access.as_ref().is_some_and(|a| a.id.is_some()) {
            self.install_note(format!("access ask {id} while another question is shown: answered no"));
            self.install_send(&link::answer(id, false, "none"));
            return;
        }
        self.install_note(format!("asks {id} (access for {}, {hours} h asked, {} s)", fp_short(&client), timeout_ms.div_ceil(1000)));
        self.cancel_pointer_modes();
        self.address_end();
        self.page_edit_end();
        if self.install.grant.take().is_some() || self.install.access.take().is_some() {
            self.install_note("own dialog closed for a question".into());
        }
        let live = !self.install.access_live.is_empty();
        self.install.access = Some(AccessDialog::new(Some(id), client, name, hours, text, live, now(), Some(Duration::from_millis(timeout_ms))));
        self.install_changed();
    }

    pub(crate) fn access_update(&mut self, access: Vec<AccessShown>, clients: Vec<ClientShown>) {
        let n = crate::status::epoch_secs();
        self.install.access_live = access.into_iter().filter(|a| a.until > n).collect();
        self.install.clients = clients;
        self.chrome_dirty();
    }

    pub(crate) fn access_key(&mut self, sym: Keysym) {
        let origin = self.input_origin;
        let Some(a) = self.install.access.as_mut() else { return };
        let s = a.key(sym, origin, now());
        self.access_step(s, &format!("key {sym:?}"));
    }

    pub(crate) fn access_press(&mut self) {
        let origin = self.input_origin;
        let full = self.area_full();
        let lines = self.access_lines().len();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let Some(a) = self.install.access.as_mut() else { return };
        let hit = crate::access_dialog::hit(full, a, lines, x, y);
        let s = a.click(hit, origin, now());
        self.access_step(s, "click");
    }

    pub(crate) fn access_motion(&mut self) -> bool {
        let full = self.area_full();
        let lines = self.access_lines().len();
        let (x, y) = (self.pointer.x, self.pointer.y);
        let Some(a) = self.install.access.as_mut() else { return false };
        let h = crate::access_dialog::hit(full, a, lines, x, y);
        std::mem::replace(&mut a.hover, h) != h
    }

    /// The dialog's text wrapped to its width.
    pub(crate) fn access_lines(&self) -> Vec<String> {
        let Some(a) = &self.install.access else { return Vec::new() };
        crate::install_draw::ask_lines(self.area_full(), &self.theme, &a.text).0
    }

    fn access_step(&mut self, s: AStep, how: &str) {
        let Some(a) = self.install.access.as_ref() else { return };
        let (id, client) = (a.id, a.client.clone());
        match s {
            AStep::Stay(changed) => self.needs_frame |= changed,
            AStep::Ignored => self.install_note(format!("injected {how} on the access dialog ignored")),
            AStep::No { input } => {
                self.install.access = None;
                match id {
                    Some(id) => {
                        let sent = self.install_send(&link::answer(id, false, input.name()));
                        self.install_note(format!("access {id} Ablehnen ({how}, {}){}", input.name(), if sent { "" } else { " (not delivered)" }));
                    }
                    None => self.install_note("access dialog closed, nothing granted".into()),
                }
                self.install_changed();
            }
            AStep::Revoke => {
                self.install.access = None;
                let sent = self.install_send(&link::revoke("access"));
                if let Some(id) = id {
                    self.install_send(&link::answer(id, false, Origin::Physical.name()));
                }
                self.install_note(format!("full access revoked at the keyboard{}", if sent { "" } else { " (not delivered)" }));
                self.install_changed();
            }
            AStep::Grant { hours, nvme, input } => {
                self.install.access = None;
                let sent = self.install_send(&link::access(id, &client, hours, nvme, input.name()));
                let who = if input == Origin::Injected { " from injected input: sent as injected, so it is refused" } else { "" };
                self.install_note(format!(
                    "access {} for {} {hours} h{}{who}{}",
                    id.map_or_else(|| String::from("(own)"), |i| i.to_string()),
                    fp_short(&client),
                    if nvme { " with NVMe" } else { "" },
                    if sent { "" } else { " (not delivered)" }
                ));
                self.install_changed();
            }
        }
    }

    /// The end of `hours` from now as the dialog names it: "14:00", or "03.11. 14:00" on another day.
    pub fn access_until_text(&self, until: i64) -> String {
        let n = crate::status::epoch_secs();
        let (a, b) = (crate::status::tm_of(n, self.status.fixed), crate::status::tm_of(until, self.status.fixed));
        if (a.tm_year, a.tm_yday) == (b.tm_year, b.tm_yday) {
            format!("{:02}:{:02}", b.tm_hour, b.tm_min)
        } else {
            format!("{:02}.{:02}. {:02}:{:02}", b.tm_mday, b.tm_mon + 1, b.tm_hour, b.tm_min)
        }
    }

    /// The dialog's title for its current choice.
    pub fn access_title(&self, a: &AccessDialog) -> String {
        let until = crate::status::epoch_secs().saturating_add(a.hours_chosen() * 3600);
        title(&a.name, &fp_short(&a.client), &self.access_until_text(until))
    }

    /// "Fernzugriff offen bis HH:MM" while a grant runs (the latest end).
    pub fn access_chip(&self) -> Option<String> {
        let until = self.install.access_live.iter().map(|a| a.until).max()?;
        Some(chip(&self.access_until_text(until)))
    }

    /// Ends what bitscomp's own clock says is over (the installer ends it too); true: the chip changed.
    pub(crate) fn access_tick(&mut self, mono: Duration) -> bool {
        if let Some(a) = self.install.access.as_ref()
            && let (Some(end), Some(id)) = (a.deadline, a.id)
            && mono >= end
        {
            self.install_note(format!("access dialog {id}: time is up, that means no"));
            self.install.access = None;
            self.install_send(&link::answer(id, false, "none"));
            self.install_changed();
        }
        let n = crate::status::epoch_secs();
        let before = self.install.access_live.len();
        self.install.access_live.retain(|a| a.until > n);
        before != self.install.access_live.len()
    }
}
