//! EINGABE/EINGABE2: the desktop writes a step's text into a foreign window, or presses Enter there (`desk_write`
//! decides, this does it): atspid answered that the field is the one with the keyboard and that the program offers
//! no EditableText. Runs on a short timer of the main loop; Georg's own input stops it at once (`desk_write_hand`,
//! from `handle_input_as`). An agent's or a script's write is answered only after the window was read again and the
//! field holds the text (B); Durchblick's own steps are checked the same way when the window is read after the step.

use std::time::{Duration, Instant};

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Resource};
use uilist::{Element, Window as UiWindow};

use crate::desk_write::{self as dw, Do, Kind, Replace, Route, Seen, Write};
use crate::state::App;
use crate::ui_fetch::To;

const TICK: Duration = Duration::from_millis(30);
const KEY_LEFTCTRL: u32 = 29;
const KEY_V: u32 = 47;
const KEY_ENTER: u32 = 28;

/// A field of the window before a write: its id, name and text (B: where a text that went astray landed).
pub type FieldBefore = (String, String, Option<String>);

pub struct Writing {
    w: Write,
    seq: u64,
    to: Option<To>,
    surface: WlSurface,
    client: Client,
    text: String,
    window: Box<UiWindow>,
    element: Box<Element>,
    /// When the step went on its way (to atspid): Georg's own input since then stops it.
    since: Instant,
    /// atspid's answer whether the field still has the keyboard.
    focused: Option<bool>,
    /// atspid's answer whether all of the field's text is selected.
    selected: Option<bool>,
    /// The field's place in the program's coordinates (logical px), for proof (ii).
    place: Vec<[i32; 4]>,
    fields: Vec<FieldBefore>,
}

#[derive(Default)]
pub struct Desk {
    pub now: Option<Writing>,
    /// The text input's state (text, cursor, anchor) right after the last write by text-input, for B (an inline
    /// completion of the address bar); taken by whoever reads the window again.
    pub after: Option<(String, u32, u32)>,
    seq: u64,
    pub handle: Option<LoopHandle<'static, App>>,
    /// Georg's last own key press, click or mouse move (only its time).
    hand: Option<Instant>,
}

impl App {
    pub fn desk_write_start(&mut self, handle: &LoopHandle<'static, App>) {
        self.writing.handle = Some(handle.clone());
    }

    /// The field's rect (desktop physical px, as the list has it) on the surface of window `window` (logical px),
    /// and the same counted from the window's geometry (without client-side shadows).
    fn desk_surface_rect(&self, window: &str, r: [i32; 4]) -> Vec<[i32; 4]> {
        let Some(p) = self.ui_drawn().into_iter().find(|p| crate::ui_reg::wid(&p.win) == window).filter(|_| r[2] > 0 && r[3] > 0) else { return Vec::new() };
        let content = if p.fullscreen { p.frame } else { self.content_rect(p.frame) };
        let ([ox, oy], _) = self.ui_surface(&p.win, content);
        let g = p.win.geometry().loc;
        let s = self.theme.scale.max(0.01);
        let l = |v: i32| (f64::from(v) / s).round() as i32;
        let on_surface = [l(r[0]) - ox, l(r[1]) - oy, l(r[2]), l(r[3])];
        vec![on_surface, [on_surface[0] - g.x, on_surface[1] - g.y, on_surface[2], on_surface[3]]]
    }

    /// atspid gave the field the keyboard: write `text` into window `window` (its elements cleared) through the
    /// desktop, or press Enter there (`kind`), then answer `to`. `fields`: the window's fields before (B).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn desk_write(&mut self, to: To, window: Box<UiWindow>, element: Box<Element>, text: Option<String>, since: Instant, kind: Kind, fields: Vec<FieldBefore>) {
        let fail = |app: &mut App, to: To, why: &str| {
            if app.writing.now.is_none() {
                app.text_in.unwatch();
            }
            app.desk_write_answer(to, &window, &element, Err(why.to_string()), None)
        };
        if self.writing.now.is_some() {
            return fail(self, to, dw::BUSY);
        }
        let text = match kind {
            Kind::Enter => String::new(),
            Kind::Text => match text.filter(|t| !t.is_empty() && t.chars().count() <= dw::MAX_CHARS) {
                Some(t) => t,
                None => return fail(self, to, "Zum Schreiben fehlt der Text, oder er ist länger als 2000 Zeichen; es wurde nichts geschrieben."),
            },
        };
        if dw::terminal(&window.app) {
            return fail(self, to, dw::TERMINAL);
        }
        let found = self.all_windows().into_iter().find(|w| crate::ui_reg::wid(w) == window.window);
        let Some((surface, client)) = found.and_then(|w| w.surface()).and_then(|s| self.dh.get_client(s.id()).ok().map(|c| (s, c))) else {
            return fail(self, to, "Das Fenster gibt es nicht mehr; es wurde nichts geschrieben.");
        };
        self.writing.seq += 1;
        let seq = self.writing.seq;
        let place = self.desk_surface_rect(&window.window, element.rect);
        // Proof (ii) needs the program's state after the focus change: its text inputs enter anew and say it.
        let had_input = self.text_in.watch(&surface);
        let w = Write::new(Instant::now(), kind, element.value.clone(), had_input);
        self.writing.now = Some(Writing { w, seq, to: Some(to), surface, client, text, window, element, since, focused: None, selected: None, place, fields });
        self.desk_write_tick();
        if self.writing.now.is_none() {
            return;
        }
        let Some(h) = self.writing.handle.clone() else { return self.desk_write_stop(dw::NO_WAY) };
        let r = h.insert_source(Timer::from_duration(TICK), move |_, _, app: &mut App| {
            if app.writing.now.as_ref().is_none_or(|w| w.seq != seq) {
                return TimeoutAction::Drop;
            }
            app.desk_write_tick();
            if app.writing.now.is_some() { TimeoutAction::ToDuration(TICK) } else { TimeoutAction::Drop }
        });
        if r.is_err() {
            self.desk_write_stop(dw::NO_WAY);
        }
    }

    /// Georg's own key press, click or mouse move (also while atspid still gives the field the focus).
    pub(crate) fn desk_write_hand(&mut self) {
        self.writing.hand = Some(Instant::now());
        if self.writing.now.is_some() {
            self.desk_write_tick();
        }
    }

    /// Durchblick stopped: its write ends (an agent's or a script's goes on).
    pub(crate) fn desk_write_stop_db(&mut self, why: &str) {
        if self.writing.now.as_ref().is_some_and(|w| matches!(w.to, Some(To::Db(_)))) {
            self.desk_write_stop(why);
        }
    }

    /// atspid answered whether the field of write `seq` still has the focus.
    pub(crate) fn desk_write_focused(&mut self, seq: u64, yes: bool) {
        if let Some(w) = self.writing.now.as_mut().filter(|w| w.seq == seq) {
            w.focused = Some(yes);
            self.desk_write_tick();
        }
    }

    /// atspid answered whether all of the field's text of write `seq` is selected.
    pub(crate) fn desk_write_selected(&mut self, seq: u64, yes: bool) {
        if let Some(w) = self.writing.now.as_mut().filter(|w| w.seq == seq) {
            w.selected = Some(yes);
            self.desk_write_tick();
        }
    }

    /// Whether Durchblick's write has already put its text in (a stop now cannot take it back).
    pub(crate) fn desk_write_in_db(&self) -> bool {
        self.writing.now.as_ref().is_some_and(|w| matches!(w.to, Some(To::Db(_))) && w.w.written(self.desk_paste_served()))
    }

    fn desk_paste_served(&self) -> bool {
        self.paste.open.is_some() && self.paste.served > 0
    }

    /// Ends a write from outside; a private paste not yet taken is taken back.
    pub(crate) fn desk_write_stop(&mut self, why: &str) {
        let Some(mut wr) = self.writing.now.take() else { return };
        let todo = wr.w.stop(why, self.desk_paste_served());
        self.desk_write_do(wr, todo);
    }

    /// EINGABE2 (fixer): before a `type`/`abschicken` step goes to atspid, the window's program's text input state
    /// is kept from now on (`TextIn::prewatch`); not while another write runs (its watch stays).
    pub(crate) fn desk_write_prewatch(&mut self, window: &str) {
        if self.writing.now.is_some() {
            return;
        }
        let found = self.all_windows().into_iter().find(|w| crate::ui_reg::wid(w) == window);
        if let Some(c) = found.and_then(|w| w.surface()).and_then(|s| self.dh.get_client(s.id()).ok()) {
            self.text_in.prewatch(c.id());
        }
    }

    /// The act did not come to the desktop's write: the kept state goes.
    pub(crate) fn desk_write_unprewatch(&mut self) {
        if self.writing.now.is_none() {
            self.text_in.unwatch();
        }
    }

    pub(crate) fn desk_write_tick(&mut self) {
        let Some(mut wr) = self.writing.now.take() else { return };
        let kb = self.seat.get_keyboard();
        let alive = wr.surface.is_alive() && self.all_windows().iter().any(|w| crate::ui_reg::wid(w) == wr.window.window);
        let focus_here = alive && kb.and_then(|k| k.current_focus()).as_ref() == Some(&wr.surface);
        let served = self.desk_paste_served();
        let hand = self.writing.hand.is_some_and(|h| h >= wr.since);
        let snap = self.text_in.snap(&wr.surface);
        let others: Vec<&str> = wr.fields.iter().filter(|(id, ..)| *id != wr.element.id).filter_map(|(_, _, v)| v.as_deref()).collect();
        let proof = dw::prove(snap.as_ref(), &wr.place, wr.element.value.as_deref(), &others);
        let seen = Seen { focus_here, field: self.text_in.field(&wr.surface), proof, hand, served, focused: wr.focused, selected: wr.selected };
        let todo = wr.w.step(Instant::now(), &seen);
        self.desk_write_do(wr, todo);
    }

    fn desk_write_do(&mut self, mut wr: Writing, todo: Vec<Do>) {
        let mut end = None;
        for d in todo {
            match d {
                Do::Commit(r) => {
                    let delete = if let Replace::Delete(b, a) = r { Some((b, a)) } else { None };
                    if !self.text_in.commit(&wr.surface, delete, &wr.text) {
                        end = Some(Err(dw::MOVED.to_string()));
                        break;
                    }
                }
                Do::Select => {
                    if !self.ui_ask_select(&wr.window, &wr.element.id, wr.seq) {
                        end = Some(Err(dw::NOT_EMPTIED.to_string()));
                        break;
                    }
                }
                Do::Check => {
                    if !self.ui_ask_focused(&wr.window, &wr.element.id, wr.seq) {
                        end = Some(Err(dw::MOVED.to_string()));
                        break;
                    }
                }
                Do::Offer => {
                    if let Err(why) = self.paste_offer(&wr.client, &wr.text) {
                        let mut rest = wr.w.stop(why, false);
                        rest.retain(|d| *d != Do::Restore);
                        end = rest.into_iter().find_map(|d| if let Do::End(r) = d { Some(r) } else { None });
                        break;
                    }
                }
                Do::PasteKey => {
                    // The one key of the step, as the control socket's input, through the panel to the window.
                    let (origin, stepping) = (std::mem::replace(&mut self.ctl_origin, crate::input::Origin::Injected), self.act.stepping);
                    self.act.stepping = true;
                    self.ui_chord(&[KEY_LEFTCTRL], KEY_V);
                    (self.act.stepping, self.ctl_origin) = (stepping, origin);
                }
                // EINGABE2: one Enter press and release, the keyboard checked to be that window's this very look.
                Do::EnterKey => {
                    let (origin, stepping) = (std::mem::replace(&mut self.ctl_origin, crate::input::Origin::Injected), self.act.stepping);
                    self.act.stepping = true;
                    self.ui_chord(&[], KEY_ENTER);
                    (self.act.stepping, self.ctl_origin) = (stepping, origin);
                }
                Do::Restore => self.paste_restore(&wr.client),
                Do::End(r) => end = Some(r),
            }
        }
        match end {
            None => self.writing.now = Some(wr),
            Some(r) => {
                if self.paste.open.is_some() {
                    self.paste_restore(&wr.client);
                }
                self.writing.after = if r == Ok(Route::TextInput) { self.text_in.snap(&wr.surface).and_then(|s| s.surrounding) } else { None };
                self.text_in.unwatch();
                let route = r.as_ref().ok().copied();
                if let Some(to) = wr.to.take() {
                    let fields = std::mem::take(&mut wr.fields);
                    self.desk_write_answer_checked(to, &wr.window, &wr.element, r.map(|_| ()), route.map(|r| (r, wr.text)), fields);
                }
            }
        }
    }

    /// B for an agent's or a script's write: the window is read again first, then answered (Durchblick's own step
    /// is read again after it anyway, `db_act_read`).
    fn desk_write_answer_checked(&mut self, to: To, window: &UiWindow, element: &Element, r: Result<(), String>, done: Option<(Route, String)>, fields: Vec<FieldBefore>) {
        let text = done.as_ref().filter(|(route, _)| *route != Route::Enter && !matches!(to, To::Db(_)) && r.is_ok()).map(|(_, t)| t.clone());
        let Some(text) = text else { return self.desk_write_answer(to, window, element, r, done) };
        self.desk_write_journal(&r, done.as_ref());
        let (win, el, name) = (window.window.clone(), element.id.clone(), element.name.clone());
        let secret = element.has(uilist::State::Secret);
        let full = crate::ui_reg::Full::One(win.clone());
        let then = crate::ui_fetch::Then::Verify { to, window: win, element: el, name, text, fields, secret };
        if let Err(crate::ui_fetch::Then::Verify { to, window, element, name, text, fields, secret }) = self.ui_fetch_give(&full, then) {
            self.desk_write_verified(to, &window, &element, &name, &text, &fields, secret);
        }
    }

    /// The window was read again after an agent's write: the answer says whether the field holds the text.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn desk_write_verified(&mut self, to: To, window: &str, element: &str, name: &str, text: &str, fields: &[FieldBefore], secret: bool) {
        let w = self.ui_windows(&crate::ui_reg::Full::One(window.to_string())).into_iter().find(|w| w.window == window);
        let now = |id: &str| w.as_ref().and_then(|w| w.find(id)).and_then(|e| e.value.clone());
        let others: Vec<(String, Option<String>, Option<String>)> = fields.iter().filter(|(id, ..)| id != element).map(|(id, n, v)| (n.clone(), v.clone(), now(id))).collect();
        let after = self.writing.after.take();
        let r = match dw::landed(now(element).as_deref(), text, &others, after.as_ref()) {
            dw::Landed::Arrived => {
                if let (Some(w), Some(e)) = (w.as_ref(), w.as_ref().and_then(|w| w.find(element))) {
                    self.ui_note(w, e, "type", Some(text).filter(|_| !secret), false);
                }
                Ok("ok".to_string())
            }
            dw::Landed::Unchecked => Ok("Geschrieben, aber nicht geprüft: der Text des Feldes lässt sich nicht lesen.".to_string()),
            dw::Landed::Missing(elsewhere) => Err(dw::missing(name, elsewhere.as_deref())),
        };
        crate::ui_fetch::answer(to, r);
    }

    fn desk_write_journal(&self, r: &Result<(), String>, done: Option<&(Route, String)>) {
        let chars = done.map_or(0, |(_, t)| t.chars().count());
        crate::db_app::journal(&format!(
            "bitscomp durchblick.write route={} result={} chars={chars}",
            done.map_or("-", |(r, _)| r.word()),
            if r.is_ok() { "ok" } else { "stopped" }
        ));
    }

    /// The write's answer to whoever asked (Durchblick's step, the control socket, an MCP agent).
    fn desk_write_answer(&mut self, to: To, window: &UiWindow, element: &Element, r: Result<(), String>, done: Option<(Route, String)>) {
        self.desk_write_journal(&r, done.as_ref());
        match to {
            To::Db(g) => match r {
                Ok(()) => self.db_act_done(g, Ok(())),
                Err(why) => self.db_act_write_stopped(g, &why),
            },
            to => {
                if let (Ok(()), Some((Route::Enter, _))) = (&r, &done) {
                    self.ui_note(window, element, "submit", None, false);
                }
                crate::ui_fetch::answer(to, r.map(|()| "ok".to_string()));
            }
        }
    }
}
