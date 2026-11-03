//! The main loop's side of `hung`: pings each client that has a window through that window's xdg_wm_base,
//! takes the pongs, marks windows "antwortet nicht" and back (B4, B6 lines on stderr). The xdg_wm_base a ping
//! went out on is kept (`App::pinged`), so its pong finds the client even when the window used for it is gone,
//! and a ping whose xdg_wm_base was destroyed before the pong is sent again on another one instead of making
//! the client look hung. WY-13: a hung client keeps being probed; when Smithay still has its ping open (the
//! pong was lost, or a ping or pong waits unread in a socket after a missed wake-up), the probe sends that ping
//! again with its open serial on the same xdg_wm_base, so new data reaches the client and its pong clears
//! Smithay's open ping. The probes come ever further apart (`hung::probe_gap`).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::win::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{SERIAL_COUNTER, Serial};
use smithay::wayland::shell::PingError;
use smithay::wayland::shell::xdg::{ShellClient, XdgWmBaseUserData};

use crate::ClientState;
use crate::hung::{Sent, Step};
use crate::state::App;

/// The line of `resend_ping` comes once per run: it means Smithay's layout changed (see `is_object_of`).
static NOT_FOUND_LOGGED: AtomicBool = AtomicBool::new(false);

/// Whether `wm` is the xdg_wm_base `sc` stands for. `ShellClient` keeps its object private, but the user data
/// map it hands out lives inside that object's `XdgWmBaseUserData` (smithay-0.7.0 xdg/mod.rs `with_data`,
/// handlers/wm_base.rs: a Mutex of the client data held inline), so the map's address lies within it.
fn is_object_of(sc: &ShellClient, wm: &XdgWmBase) -> bool {
    let Some(ud) = wm.data::<XdgWmBaseUserData>() else { return false };
    let start = ud as *const XdgWmBaseUserData as usize;
    let end = start + std::mem::size_of::<XdgWmBaseUserData>();
    sc.with_data(|m| m as *const UserDataMap as usize).is_ok_and(|a| (start..end).contains(&a))
}

impl App {
    /// WY-5: since when a hung window's program has not answered (`pacer` time).
    pub fn hung_since(&self, w: &Window) -> Option<Duration> {
        self.client_of(w).and_then(|c| self.answer.get(&c.id()).and_then(|t| t.since()))
    }

    pub fn is_hung(&self, w: &Window) -> bool {
        self.client_of(w).is_some_and(|c| self.answer.get(&c.id()).is_some_and(|t| t.hung()))
    }

    /// One look at every client with a window; at most one ping per client and second. True when a ping went
    /// out (the caller flushes at once, so a long render does not eat into the client's 2 s).
    pub fn answer_tick(&mut self, now: Duration) -> bool {
        let mut seen = HashSet::new();
        let mut became_hung = Vec::new();
        let mut noted = Vec::new();
        let mut resend = Vec::new();
        let mut sent = false;
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
                    let result = sc.send_ping(SERIAL_COUNTER.next_serial());
                    let what = match result {
                        Ok(()) => Sent::New,
                        // A ping still open on this xdg_wm_base from before counts as this one.
                        Err(PingError::PingAlreadyPending(_)) => Sent::Open,
                        Err(PingError::DeadSurface) => Sent::Dead,
                    };
                    let after = tracker.sent(what);
                    sent |= what == Sent::New;
                    if what != Sent::Dead {
                        self.pinged.insert(client.id(), top.client());
                    }
                    if let (true, Err(PingError::PingAlreadyPending(serial))) = (after.resend, result) {
                        resend.push((client.clone(), sc, serial));
                    }
                    if after.note {
                        noted.push((
                            w.clone(),
                            match what {
                                Sent::Open => {
                                    "its ping is still open (pong lost or unread): sent again with that serial"
                                }
                                _ => "a new ping went out (the open one was answered or its xdg_wm_base is gone)",
                            },
                        ));
                    }
                }
                Step::Hung => became_hung.push(w.clone()),
                Step::Nothing => {}
            }
        }
        self.answer.retain(|id, _| seen.contains(id));
        self.pinged.retain(|id, _| seen.contains(id));
        for (client, sc, serial) in resend {
            sent |= self.resend_ping(&client, &sc, serial);
        }
        for (w, what) in noted {
            eprintln!("bitscomp: {}: still hung: {what}", self.describe(&w));
        }
        for w in became_hung {
            eprintln!(
                "bitscomp: {}: antwortet nicht (no pong for {} s)",
                self.describe(&w),
                crate::hung::HUNG_AFTER.as_secs()
            );
            self.hung_changed();
        }
        sent
    }

    /// The open ping `serial` once more, on the xdg_wm_base it is open on (`sc`): Smithay's `send_ping` refuses
    /// while it is open, and its handle to the object is private. Only the pong on that object clears Smithay's
    /// open ping (handlers/wm_base.rs compares the serial stored in that object's data). True when sent.
    fn resend_ping(&self, client: &Client, sc: &ShellClient, serial: Serial) -> bool {
        // A client that has left that much unread (a stopped program) gets no more: the probes would pile up in
        // bitscomp's bounded output queue and get the client dropped. It reads them all when it runs again.
        if client.get_data::<ClientState>().and_then(|d| d.watch.as_ref()).is_some_and(|w| !crate::clients::writable(w))
        {
            return false;
        }
        let mut objects = Vec::new();
        let _ = self.dh.backend_handle().with_all_objects_for(client.id(), |o| {
            if o.interface().name == "xdg_wm_base" {
                objects.push(o);
            }
        });
        let wm = objects
            .into_iter()
            .filter_map(|o| XdgWmBase::from_id(&self.dh, o).ok())
            .find(|wm| wm.is_alive() && is_object_of(sc, wm));
        match wm {
            Some(wm) => {
                wm.ping(serial.into());
                true
            }
            None => {
                if !NOT_FOUND_LOGGED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "bitscomp: answer: the xdg_wm_base of an open ping was not found; the probe is not sent again"
                    );
                }
                false
            }
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
