//! The main loop's side of `hung`: pings each client that has a window through that window's xdg_wm_base,
//! takes the pongs, marks windows "antwortet nicht" and back (B4, B6 lines on stderr). The xdg_wm_base a ping
//! went out on is kept (`App::pinged`), so its pong finds the client even when the window used for it is gone,
//! and a ping whose xdg_wm_base was destroyed before the pong is sent again on another one instead of making
//! the client look hung.

use std::collections::HashSet;
use std::time::Duration;

use smithay::desktop::Window;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::shell::PingError;
use smithay::wayland::shell::xdg::ShellClient;

use crate::hung::Step;
use crate::state::App;

impl App {
    /// WY-5: since when a hung window's program has not answered (`pacer` time).
    pub fn hung_since(&self, w: &Window) -> Option<Duration> {
        self.client_of(w).and_then(|c| self.answer.get(&c.id()).and_then(|t| t.since()))
    }

    pub fn is_hung(&self, w: &Window) -> bool {
        self.client_of(w).is_some_and(|c| self.answer.get(&c.id()).is_some_and(|t| t.hung()))
    }

    /// One look at every client with a window; at most one ping per client and second.
    pub fn answer_tick(&mut self, now: Duration) {
        let mut seen = HashSet::new();
        let mut became_hung = Vec::new();
        for w in self.all_windows() {
            let (Some(top), Some(client)) = (w.toplevel(), self.client_of(&w)) else { continue };
            if !seen.insert(client.id()) {
                continue;
            }
            let tracker = self.answer.entry(client.id()).or_default();
            if self.pinged.get(&client.id()).is_some_and(|sc| !sc.alive()) {
                // The client destroyed that xdg_wm_base (a request: it is not stuck); its pong can never come.
                self.pinged.remove(&client.id());
                tracker.not_sent();
            }
            match tracker.tick(now) {
                Step::Ping => {
                    let sc = top.client();
                    match sc.send_ping(SERIAL_COUNTER.next_serial()) {
                        // A ping still open on this xdg_wm_base from before counts as this one.
                        Ok(()) | Err(PingError::PingAlreadyPending(_)) => {
                            self.pinged.insert(client.id(), sc);
                        }
                        Err(PingError::DeadSurface) => tracker.not_sent(),
                    }
                }
                Step::Hung => became_hung.push(w.clone()),
                Step::Nothing => {}
            }
        }
        self.answer.retain(|id, _| seen.contains(id));
        self.pinged.retain(|id, _| seen.contains(id));
        for w in became_hung {
            eprintln!("bitscomp: {}: antwortet nicht (no pong for {} s)", self.describe(&w), crate::hung::HUNG_AFTER.as_secs());
            self.hung_changed();
        }
    }

    /// A pong from `sc`; a hung client answers again.
    pub fn pong(&mut self, sc: &ShellClient) {
        let by_ping = self.pinged.iter().find(|(_, p)| *p == sc).map(|(id, _)| id.clone());
        let by_window = || {
            self.all_windows()
                .into_iter()
                .find(|w| w.toplevel().is_some_and(|t| t.client() == *sc))
                .and_then(|w| self.client_of(&w))
                .map(|c| c.id())
        };
        let Some(id) = by_ping.or_else(by_window) else { return };
        self.pinged.remove(&id);
        if self.answer.get_mut(&id).is_some_and(|t| t.pong()) {
            let w = self.all_windows().into_iter().find(|w| self.client_of(w).is_some_and(|c| c.id() == id));
            let what = w.map_or_else(|| format!("client {id:?}"), |w| self.describe(&w));
            eprintln!("bitscomp: {what}: answers again");
            self.hung_changed();
        }
    }

    /// Header labels, the veil, the keyboard focus and the held-back configures depend on it.
    fn hung_changed(&mut self) {
        self.layout_dirty = true;
        self.needs_frame = true;
    }

    /// The next time `answer_tick` has something to do.
    pub fn answer_due(&self) -> Option<Duration> {
        self.answer.values().filter_map(|t| t.due()).min()
    }
}
