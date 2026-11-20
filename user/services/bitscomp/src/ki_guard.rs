//! CL-5: nobody but Georg has the keyboard while he types a provider's key. He types blind, so he cannot see
//! when his keys land somewhere else: an injected click into another field, a control command that opens the
//! address bar or a program that takes the focus would show (and report) the rest of the key in clear.
//! - While the key's field is open, injected input events are dropped and the control socket answers `BUSY` to
//!   everything that changes something.
//! - The field ends by Georg's own Enter, Esc, click or shortcut. When it loses the keyboard any other way
//!   (its row or window went away, another window took the focus, nothing typed for `IDLE`), his keys go
//!   nowhere until his next real click, and the page says so in red.

use std::time::Duration;

use mcpreg::Json;

use crate::input::Origin;
use crate::state::App;

/// Without a real key for this long the field ends, so an abandoned one never keeps injected input out for good.
pub const IDLE: Duration = Duration::from_secs(120);
pub const LOST: &str = "Die Eingabe des Schlüssels wurde unterbrochen: nichts gespeichert. Bis zu deinem nächsten Mausklick nimmt BitsOS keine Tasten an.";
pub const IDLE_END: &str = "Zwei Minuten lang nichts getippt: die Eingabe des Schlüssels ist beendet, nichts gespeichert. Bis zu deinem nächsten Mausklick nimmt BitsOS keine Tasten an.";
/// The control socket's answer while a key is typed.
pub const BUSY: &str = "busy: a provider's key is being typed; injected input and commands that change something wait until it ends";

/// The key being typed.
pub(crate) struct Typing {
    pub pid: u32,
    pub id: String,
    /// Address, kind and place as the dialog showed them: settingsd's `while` for the key.
    pub whilst: Vec<(String, Json)>,
    /// The last real key (`pacer` time).
    pub last: Duration,
    /// The log said once that injected input is kept out.
    pub noted: bool,
}

/// The control commands that only read (the rest waits while a key is typed).
fn reads(words: &[&str]) -> bool {
    matches!(words, ["page"] | ["hot"] | ["shot", ..] | ["list"] | ["theme"])
}

impl App {
    /// The page whose field has the keyboard now (no library or sofa screen over it).
    fn ki_keys_at(&self) -> Option<u32> {
        #[cfg(test)]
        if self.ki.keys_at.is_some() {
            return self.ki.keys_at;
        }
        self.page_edit_pid().filter(|_| !self.lib.open && !self.feld.open && !self.rewind.open && !self.sofa.listens())
    }

    /// A key is being typed and its field still has the keyboard. A field that lost it, or that nobody typed
    /// into for `IDLE`, ends here.
    pub(crate) fn ki_key_open(&mut self) -> bool {
        let Some(t) = self.ki.typing.as_ref() else { return false };
        let (pid, idle) = (t.pid, crate::pacer::now().saturating_sub(t.last) >= IDLE);
        if !idle && self.ki_keys_at() == Some(pid) && self.ki_secret_edit(pid) {
            return true;
        }
        self.ki.typing = None;
        self.ki.swallow = true;
        self.page_edit_end();
        eprintln!("bitscomp: ki: the key's field {}: edit ended, nothing stored, real keys go nowhere until a real click", if idle { "was idle" } else { "lost the keyboard" });
        self.page_say(pid, if idle { IDLE_END } else { LOST }, true);
        false
    }

    /// An input event of `origin` must not be handled: injected, while a key is typed.
    pub(crate) fn ki_drops(&mut self, origin: Origin) -> bool {
        if origin == Origin::Physical || !self.ki_key_open() {
            return false;
        }
        if let Some(t) = self.ki.typing.as_mut().filter(|t| !t.noted) {
            t.noted = true;
            eprintln!("bitscomp: ki: injected input is dropped while a provider's key is typed");
        }
        true
    }

    /// The control command `words` must wait: a key is typed and the command is not one that only reads.
    pub(crate) fn ki_ctl_waits(&mut self, words: &[&str]) -> bool {
        self.ctl_origin == Origin::Injected && !reads(words) && self.ki_key_open()
    }

    /// A pressed key of a real keyboard, before anything takes it. True: it goes nowhere.
    pub(crate) fn ki_key_swallowed(&mut self) -> bool {
        if self.input_origin != Origin::Physical {
            return false;
        }
        if self.ki.typing.is_some() && self.ki_key_open() {
            if let Some(t) = self.ki.typing.as_mut() {
                t.last = crate::pacer::now();
            }
            return false;
        }
        self.ki.swallow
    }

    /// Georg ended the key's edit himself (Esc, a shortcut of his): nothing is stored, his keys stay his.
    pub(crate) fn ki_key_own_end(&mut self) {
        if self.ki.typing.take().is_some() {
            self.page_edit_end();
        }
    }

    /// A real mouse button went down and was handled: his keys are his again, and a key's field his click
    /// left is ended by him.
    pub(crate) fn ki_own_click(&mut self) {
        self.ki.swallow = false;
        if let Some(pid) = self.ki.typing.as_ref().map(|t| t.pid)
            && !(self.ki_keys_at() == Some(pid) && self.ki_secret_edit(pid))
        {
            self.ki_key_own_end();
        }
    }

    /// The pages `gone` closed: their questions go with them.
    pub(crate) fn ki_pages_gone(&mut self, gone: &[u32]) {
        for pid in gone {
            self.ki.asking.remove(pid);
        }
    }
}
