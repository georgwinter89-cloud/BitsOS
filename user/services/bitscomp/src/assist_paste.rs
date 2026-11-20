//! CL-3b: a paste into the assistant's input line (Ctrl+V, Shift+Insert): the clipboard's text is asked of its
//! owner (`clip_read`: a Wayland client, bitscomp itself, or its bridge for an X11 program) and read on a thread
//! of its own (B4) with a deadline; it comes back to the main loop as `Msg::Paste`.

use crate::assist::INPUT_MAX;
use crate::assist_client::Msg;
use crate::clip_read::{self as cr, Limits, Source};
use crate::input::Origin;
use crate::state::App;

/// No more is read than the input line takes (`INPUT_MAX`; the model cuts there and says so).
const LIMITS: Limits = Limits { cap: INPUT_MAX + 1, idle: cr::IDLE, total: cr::TOTAL, line: false };

impl App {
    /// Ctrl+V or Shift+Insert: the clipboard's text, read on a thread of its own (B4) with a deadline; it comes
    /// back as `Msg::Paste` with the origin of the key that asked for it.
    pub(crate) fn assist_paste(&mut self, origin: Origin) {
        let Some(tx) = self.assist_client.as_ref().map(|c| c.tx.clone()) else { return };
        let r = match self.clip_source() {
            Source::Pipe(r) => r,
            Source::Own(text) => {
                // The main loop never waits on the bounded queue.
                let _ = tx.try_send(Msg::Paste(text, origin));
                return;
            }
            Source::None => {
                self.assist.note = Some(("Die Zwischenablage hat keinen Text.".into(), false));
                return self.chrome_dirty();
            }
        };
        let spawned =
            std::thread::Builder::new().name("bitscomp-paste".into()).spawn(move || match cr::read(r, LIMITS) {
                Ok(text) => {
                    let _ = tx.send(Msg::Paste(text, origin));
                }
                Err(why) => cr::log(&format!("the assistant's line got nothing from the clipboard ({why:?})")),
            });
        if let Err(e) = spawned {
            eprintln!("bitscomp: assist: no thread for the paste: {e}");
        }
    }
}
