//! GPY-PASTE: Ctrl+V and Shift+Insert in a page's field (the settings' rows and search, the page "KI"'s fields and
//! keys, the GitHub token of "Aktualisieren", the Programme and System searches). The fields are single-line: the
//! first line of the clipboard's text that is not blank goes in at the cursor (`clip_read`). Its owner's pipe is
//! read on a thread of its own and the text comes back through a channel (B4); the field it was asked for must
//! still be open then, or the text is wiped unseen. A paste is Georg's only when its key came from his real
//! keyboard: an injected one is refused like an injected key (in a key's or the token's field the guards end the
//! edit before this). The text is never logged nor reported, and the buffers it went through are wiped.

use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Event};

use crate::clip_read::{self as cr, Limits, Source, Why};
use crate::page::Part;
use crate::settings_write::wipe;
use crate::state::App;

pub const EMPTY: &str = "In der Zwischenablage ist kein Text: nichts eingefügt.";
pub const SLOW: &str = "Das Programm, aus dem du kopiert hast, gibt den Text nicht her. Kopiere ihn bitte noch einmal.";
pub const TOO_LONG: &str = "Der kopierte Text ist zu lang für dieses Feld: nichts eingefügt.";
pub const FAILED: &str = "Das Einfügen hat nicht geklappt: nichts eingefügt.";
/// The most characters a page's field holds (as `page_key` lets type).
pub const FIELD_MAX: usize = 4096;

/// A paste's text, back from its thread.
pub struct Pasted {
    pub n: u64,
    pub text: Result<String, Why>,
}

#[derive(Default)]
pub struct PagePaste {
    tx: Option<channel::Sender<Pasted>>,
    /// The paste on its way: its number, page and field.
    pub(crate) open: Option<(u64, u32, Part)>,
    next: u64,
}

fn say(why: Why) -> &'static str {
    match why {
        Why::Empty => EMPTY,
        Why::Slow => SLOW,
        Why::TooLong => TOO_LONG,
        Why::Failed => FAILED,
    }
}

impl App {
    pub fn page_paste_start(&mut self, handle: &LoopHandle<'static, App>) {
        let (tx, rx) = channel::channel::<Pasted>();
        match handle.insert_source(rx, |ev, _, app: &mut App| {
            if let Event::Msg(p) = ev {
                app.page_pasted(p);
            }
        }) {
            Ok(_) => self.pages.paste.tx = Some(tx),
            Err(e) => eprintln!("bitscomp: paste: no channel ({e}); the pages' fields take no paste"),
        }
    }

    /// Ctrl+V or Shift+Insert in the field being edited on `pid` (`pages.real`: the key's origin).
    pub(crate) fn page_paste(&mut self, pid: u32) {
        let Some(part) = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()).map(|(p, _)| *p) else { return };
        if !self.pages.real {
            cr::log("a paste by injected input refused");
            return self.page_say(pid, crate::ki_ask::REAL_INPUT, true);
        }
        // One at a time: a second Ctrl+V while the first one's text is on its way does nothing.
        if self.pages.paste.open.is_some() {
            return;
        }
        self.pages.paste.next += 1;
        let n = self.pages.paste.next;
        match self.clip_source() {
            Source::None => self.page_say(pid, EMPTY, true),
            Source::Own(text) => {
                self.pages.paste.open = Some((n, pid, part));
                let line = cr::first_line(&text).ok_or(Why::Empty);
                self.page_pasted(Pasted { n, text: line });
            }
            Source::Pipe(r) => {
                let Some(tx) = self.pages.paste.tx.clone() else { return self.page_say(pid, FAILED, true) };
                self.pages.paste.open = Some((n, pid, part));
                let spawned = std::thread::Builder::new().name("bitscomp-paste".into()).spawn(move || {
                    let text = cr::read(r, Limits::LINE);
                    if let Err(e) = tx.send(Pasted { n, text }) {
                        // The loop is gone: the text goes unseen.
                        if let Ok(t) = e.0.text {
                            wipe(t);
                        }
                    }
                });
                if let Err(e) = spawned {
                    self.pages.paste.open = None;
                    cr::log(&format!("no thread for the paste: {e}"));
                    self.page_say(pid, FAILED, true);
                }
            }
        }
    }

    /// A paste's text is back: into its field, if that is still open.
    pub(crate) fn page_pasted(&mut self, p: Pasted) {
        let open = self.pages.paste.open.filter(|(n, ..)| *n == p.n);
        let Some((_, pid, part)) = open else {
            if let Ok(t) = p.text {
                wipe(t);
            }
            return;
        };
        self.pages.paste.open = None;
        let Some((_, e)) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.edit.as_mut()).filter(|(q, _)| *q == part)
        else {
            if let Ok(t) = p.text {
                wipe(t);
            }
            return;
        };
        let line = match p.text {
            Ok(t) if e.text.chars().count() + t.chars().count() > FIELD_MAX => {
                wipe(t);
                return self.page_say(pid, TOO_LONG, true);
            }
            Ok(t) => t,
            Err(why) => {
                cr::log(&format!("nothing pasted into a field ({why:?})"));
                return self.page_say(pid, say(why), true);
            }
        };
        for c in line.chars() {
            e.insert(c);
        }
        wipe(line);
        self.needs_frame = true;
        self.page_typed(pid);
    }
}

#[cfg(test)]
#[path = "page_paste_tests.rs"]
mod tests;
