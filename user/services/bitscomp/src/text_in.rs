//! EINGABE: text-input-v3 with bitscomp itself as the input method, so Durchblick's "Mach das für mich" can write
//! into a field of a program that offers no EditableText (Chromium, Brave). Written here instead of Smithay's
//! `text_input`: that one drops every request unless a separate input-method-v2 program is bound and keeps its
//! active instance to itself, so the desktop could not be the input method through it.
//! bitscomp sends `enter`/`leave` as the keyboard moves and keeps per text input only whether it is enabled and
//! whether its field asks for a secret (password, PIN). EINGABE2: while one Durchblick step writes into one window
//! (`watch`), that window's text input state (the surrounding text, the caret's place) is kept too, to prove the
//! right field has the keyboard and to remove the old content; it is dropped when the step ends and never kept for
//! a secret field. Text goes out only through `commit`, called for one Durchblick step.

use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::{self as manager, ZwpTextInputManagerV3};
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::{self as ti, ContentHint, ContentPurpose, ZwpTextInputV3};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};

use crate::desk_write::Snap;
use crate::state::App;

struct Input {
    res: ZwpTextInputV3,
    /// Commit requests so far: `done` carries their count (the protocol's serial).
    commits: u32,
    entered: bool,
    enabled: bool,
    secret: bool,
    pending: Pending,
    /// EINGABE2: the state since its last enable, only while watched.
    snap: Option<Snap>,
    /// Enabled when the watch began (it is reset then), and whether the program said enable/disable since.
    before: bool,
    answered: bool,
}

#[derive(Default, Clone)]
struct Pending {
    enable: Option<bool>,
    secret: Option<bool>,
    surrounding: Option<(String, u32, u32)>,
    caret: Option<[i32; 4]>,
}

#[derive(Default)]
pub struct TextIn {
    inputs: Vec<Input>,
    focus: Option<WlSurface>,
    /// The program whose text input state a Durchblick step reads now (`watch`).
    watch: Option<ClientId>,
}

/// One text input as the keyboard moves (`old_mine`/`new_mine`: the old/new surface is a live one of its program):
/// what it had entered leaves, even a surface that is gone (it gets no `leave`, there is nothing to send it for,
/// but its enabled state goes all the same, so the next field waits for a fresh enable). Returns (leave, enter).
fn refocus(entered: &mut bool, enabled: &mut bool, old_mine: bool, new_mine: bool) -> (bool, bool) {
    let leave = *entered && old_mine;
    if *entered {
        (*entered, *enabled) = (false, false);
    }
    *entered = new_mine;
    (leave, new_mine)
}

pub fn global(dh: &DisplayHandle) {
    dh.create_global::<App, ZwpTextInputManagerV3, ()>(1, ());
}

fn s_is(c: &Option<WlSurface>, s: &WlSurface) -> bool {
    c.as_ref() == Some(s)
}

fn of(c: &Option<WlSurface>, res: &ZwpTextInputV3) -> bool {
    c.as_ref().is_some_and(|s| s.is_alive() && s.id().same_client_as(&res.id()))
}

/// Whether a text input stands for the field of `surface` now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// No text input of that program is enabled (no field with the keyboard, or no text input at all).
    None,
    Enabled,
    /// The field asks for a password or a PIN: Durchblick never writes there.
    Secret,
}

impl TextIn {
    /// The keyboard moved to `surface`: the old program's text inputs leave, the new one's enter.
    pub fn focus(&mut self, surface: Option<&WlSurface>) {
        if self.focus.as_ref() == surface {
            return;
        }
        let old = self.focus.take();
        self.focus = surface.cloned();
        let now = self.focus.clone();
        for i in self.inputs.iter_mut() {
            let (leave, enter) = refocus(&mut i.entered, &mut i.enabled, of(&old, &i.res), of(&now, &i.res));
            if leave || enter {
                i.snap = None;
            }
            if let (true, Some(s)) = (leave, &old) {
                i.res.leave(s);
            }
            if let (true, Some(s)) = (enter, &now) {
                i.res.enter(s);
            }
        }
    }

    pub fn field(&self, surface: &WlSurface) -> Field {
        if self.focus.as_ref() != Some(surface) {
            return Field::None;
        }
        match self.inputs.iter().find(|i| i.entered && i.enabled && of(&self.focus, &i.res)) {
            Some(i) if i.secret => Field::Secret,
            Some(_) => Field::Enabled,
            None => Field::None,
        }
    }

    /// EINGABE2 (fixer): a `type`/`abschicken` step is on its way to atspid for a window of `client`: its state from
    /// now on is kept, so what the program sends after atspid's focus change is there even for a program that does
    /// not answer a text input's leave and enter (Chromium may enable only on its own focus changes).
    pub fn prewatch(&mut self, client: ClientId) {
        self.watch = Some(client.clone());
        for i in self.inputs.iter_mut().filter(|i| i.res.client().is_some_and(|c| c.id() == client)) {
            i.snap = None;
        }
    }

    /// EINGABE2: a Durchblick step reads the text input state of `surface`'s program from now on. A state that came
    /// since `prewatch` is kept; otherwise its text inputs leave and enter again, so the program enables them anew
    /// and sends its state for the field that has the keyboard now (not one from before the focus change). Returns
    /// whether one of them was enabled: then the program speaks text-input there, and a write without its fresh
    /// state is not proven (no paste instead).
    pub fn watch(&mut self, surface: &WlSurface) -> bool {
        self.watch = surface.client().map(|c| c.id());
        let mut had = false;
        for i in self.inputs.iter_mut().filter(|i| i.entered && of(&self.focus, &i.res) && s_is(&self.focus, surface)) {
            had |= i.enabled;
            (i.before, i.answered) = (i.enabled, false);
            if i.enabled && i.snap.as_ref().is_some_and(|s| s.caret.is_some() || s.surrounding.is_some()) {
                continue;
            }
            (i.enabled, i.snap, i.pending) = (false, None, Pending::default());
            i.res.leave(surface);
            i.res.enter(surface);
        }
        had
    }

    /// The step is over: nothing of the state stays. A text input the program never enabled anew (it ignores the
    /// leave and enter) is as enabled as before the watch, so bitscomp and the program agree again.
    pub fn unwatch(&mut self) {
        self.watch = None;
        for i in self.inputs.iter_mut() {
            i.snap = None;
            (i.pending.surrounding, i.pending.caret) = (None, None);
            if std::mem::take(&mut i.before) && !i.answered {
                i.enabled = i.entered;
            }
        }
    }

    /// The fresh state of the enabled field of `surface` (only while watched).
    pub fn snap(&self, surface: &WlSurface) -> Option<Snap> {
        if self.field(surface) != Field::Enabled {
            return None;
        }
        self.inputs.iter().find(|i| i.entered && i.enabled && of(&self.focus, &i.res)).and_then(|i| i.snap.clone())
    }

    /// The text goes into the enabled field of `surface` at its cursor, `delete` bytes before and after the cursor
    /// removed first in the same `done` (what the program selected is replaced, as typing would). False: no such field.
    pub fn commit(&mut self, surface: &WlSurface, delete: Option<(u32, u32)>, text: &str) -> bool {
        if self.field(surface) != Field::Enabled {
            return false;
        }
        let focus = self.focus.clone();
        let Some(i) = self.inputs.iter().find(|i| i.entered && i.enabled && of(&focus, &i.res)) else { return false };
        if let Some((before, after)) = delete.filter(|d| *d != (0, 0)) {
            i.res.delete_surrounding_text(before, after);
        }
        i.res.commit_string(Some(text.to_string()));
        i.res.done(i.commits);
        true
    }

    fn add(&mut self, res: ZwpTextInputV3) {
        let entered = of(&self.focus, &res);
        if let (true, Some(s)) = (entered, &self.focus) {
            res.enter(s);
        }
        self.inputs.push(Input { res, commits: 0, entered, enabled: false, secret: false, pending: Pending::default(), snap: None, before: false, answered: false });
    }

    fn request(&mut self, res: &ZwpTextInputV3, req: ti::Request) {
        let watched = self.watch.as_ref().is_some_and(|c| res.client().is_some_and(|r| r.id() == *c));
        let Some(i) = self.inputs.iter_mut().find(|i| i.res == *res) else { return };
        match req {
            ti::Request::Enable => i.pending.enable = Some(true),
            ti::Request::Disable => i.pending.enable = Some(false),
            ti::Request::SetContentType { hint, purpose } => {
                let secret_purpose = matches!(purpose, WEnum::Value(ContentPurpose::Password | ContentPurpose::Pin));
                let sensitive = matches!(hint, WEnum::Value(h) if h.contains(ContentHint::SensitiveData) || h.contains(ContentHint::HiddenText));
                i.pending.secret = Some(secret_purpose || sensitive);
            }
            // EINGABE2: only while a step watches this program (never otherwise).
            ti::Request::SetSurroundingText { text, cursor, anchor } if watched => {
                i.pending.surrounding = Some((text, cursor.max(0) as u32, anchor.max(0) as u32));
            }
            ti::Request::SetCursorRectangle { x, y, width, height } if watched => i.pending.caret = Some([x, y, width, height]),
            ti::Request::Commit => {
                i.commits = i.commits.wrapping_add(1);
                let p = std::mem::take(&mut i.pending);
                i.answered |= p.enable.is_some();
                match p.enable {
                    // Enabling starts from a fresh state (the protocol's reset): an old secret flag does not stay.
                    Some(true) => (i.enabled, i.secret, i.snap) = (i.entered, false, None),
                    Some(false) => (i.enabled, i.snap) = (false, None),
                    None => {}
                }
                if let Some(s) = p.secret {
                    i.secret = s;
                }
                // A secret field's state is never kept; a watched one's from its enable on (double-buffered state).
                if watched && i.enabled && !i.secret {
                    let s = i.snap.get_or_insert_with(Snap::default);
                    if p.surrounding.is_some() {
                        s.surrounding = p.surrounding;
                    }
                    if p.caret.is_some() {
                        s.caret = p.caret;
                    }
                } else {
                    i.snap = None;
                }
            }
            // Why the text changed and everything of an unwatched program are not kept.
            _ => {}
        }
    }

    fn gone(&mut self, res: &ZwpTextInputV3) {
        self.inputs.retain(|i| i.res != *res);
    }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for App {
    fn bind(_: &mut App, _: &DisplayHandle, _: &Client, res: New<ZwpTextInputManagerV3>, _: &(), init: &mut DataInit<'_, App>) {
        init.init(res, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for App {
    fn request(app: &mut App, _: &Client, _: &ZwpTextInputManagerV3, req: manager::Request, _: &(), _: &DisplayHandle, init: &mut DataInit<'_, App>) {
        if let manager::Request::GetTextInput { id, .. } = req {
            let res = init.init(id, ());
            app.text_in.add(res);
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for App {
    fn request(app: &mut App, _: &Client, res: &ZwpTextInputV3, req: ti::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, App>) {
        app.text_in.request(res, req);
    }

    fn destroyed(app: &mut App, _: ClientId, res: &ZwpTextInputV3, _: &()) {
        app.text_in.gone(res);
    }
}

#[cfg(test)]
mod tests {
    use super::refocus;

    #[test]
    fn a_field_on_a_surface_that_is_gone_is_not_enabled_on_the_next() {
        let (mut entered, mut enabled) = (true, true);
        // The focused popup closed, the keyboard moves to another surface of the same program.
        assert_eq!(refocus(&mut entered, &mut enabled, false, true), (false, true), "no leave for a dead surface, enter for the new one");
        assert!(entered && !enabled, "the new surface waits for its own enable");
        let (mut entered, mut enabled) = (true, true);
        assert_eq!(refocus(&mut entered, &mut enabled, true, false), (true, false), "to another program: leave");
        assert!(!entered && !enabled);
        let (mut entered, mut enabled) = (false, false);
        assert_eq!(refocus(&mut entered, &mut enabled, false, false), (false, false), "not this program's: nothing");
    }
}
