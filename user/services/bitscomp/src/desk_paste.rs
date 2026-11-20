//! EINGABE: a private clipboard for one window. When a program has no text input for its field, Durchblick's text
//! reaches it as a paste that only this program ever sees: an offer made for its data devices alone (never the
//! seat's selection, so Georg's clipboard, its owner, the X11 bridge and every other program stay as they are),
//! Ctrl+V into its surface, the text written into the program's pipe, then the program gets the real selection
//! back at once. Smithay keeps its list of data devices to itself, so the program's devices are found by their
//! protocol numbers (a client numbers its objects from 1 upward). A program that holds Georg's clipboard itself
//! (he copied something there) pastes its own copy without asking for ours, as Chromium and GTK do: then nothing
//! is offered and no key is pressed.

use std::io::Write;
use std::os::fd::OwnedFd;

use smithay::reexports::wayland_server::protocol::wl_data_device::WlDataDevice;
use smithay::reexports::wayland_server::protocol::wl_data_offer::{self, WlDataOffer};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, Resource};
use smithay::wayland::selection::data_device::{current_data_device_selection_userdata, set_data_device_focus};

use crate::desk_write as dw;

use crate::state::App;

/// The text kinds offered (as bitscomp's own clipboard offers them, `sys_clip`).
const MIMES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];
/// How far the protocol numbers are searched for the program's data devices.
const MAX_ID: u32 = 1 << 16;

/// The offer's mark: which paste it belongs to (a later paste never gets an earlier text).
pub struct Private(pub u64);

#[derive(Default)]
pub struct Paste {
    /// The paste being offered: its number and text, and how often the program asked for it.
    pub open: Option<(u64, String)>,
    pub served: u32,
    next: u64,
    /// The program whose own copy is Georg's clipboard (None: bitscomp's, an X program's, or none).
    owner: Option<ClientId>,
}

fn devices(dh: &DisplayHandle, client: &Client) -> Vec<WlDataDevice> {
    (1..MAX_ID).filter_map(|id| client.object_from_protocol_id::<WlDataDevice>(dh, id).ok()).filter(|d| d.is_alive()).collect()
}

impl App {
    /// A program set Georg's clipboard (or cleared it). bitscomp offers no data-control, so it is always the program
    /// with the keyboard: Smithay takes a selection only from that one. A paste waiting for its program ends here,
    /// since its Ctrl+V may now read the new clipboard.
    pub(crate) fn paste_new_selection(&mut self, set: bool) {
        let kb = self.seat.get_keyboard().and_then(|k| k.current_focus());
        self.paste.owner = kb.filter(|_| set).and_then(|s| self.dh.get_client(s.id()).ok()).map(|c| c.id());
        if self.paste.open.is_some() {
            self.desk_write_stop(dw::SEL_CHANGED);
        }
    }

    /// Offers `text` to the program of `client` alone, as its selection. Err: why not (no data device, or the
    /// program holds the clipboard itself and would paste that).
    pub(crate) fn paste_offer(&mut self, client: &Client, text: &str) -> Result<(), &'static str> {
        let compositor = current_data_device_selection_userdata(&self.seat).is_some();
        if !compositor && self.paste.owner == Some(client.id()) {
            return Err(dw::OWN);
        }
        let devs = devices(&self.dh, client);
        if devs.is_empty() {
            return Err(dw::NO_WAY);
        }
        self.paste.next += 1;
        let n = self.paste.next;
        let mut offered = false;
        for d in devs {
            let Ok(offer) = client.create_resource::<WlDataOffer, Private, App>(&self.dh, d.version(), Private(n)) else { continue };
            d.data_offer(&offer);
            for m in MIMES {
                offer.offer(m.to_string());
            }
            d.selection(Some(&offer));
            offered = true;
        }
        if !offered {
            return Err(dw::NO_WAY);
        }
        self.paste.open = Some((n, text.to_string()));
        self.paste.served = 0;
        Ok(())
    }

    /// The program gets Georg's real selection again (or none, if there is none), and the private text is gone.
    pub(crate) fn paste_restore(&mut self, client: &Client) {
        self.paste.open = None;
        // The private offer leaves the program even if the keyboard has moved on (Smithay would not tell it then).
        for d in devices(&self.dh, client) {
            d.selection(None);
        }
        // Smithay offers the seat's selection when the clipboard's focus changes: away and back to the program.
        set_data_device_focus(&self.dh, &self.seat, None);
        let focused = self.seat.get_keyboard().and_then(|k| k.current_focus()).and_then(|s| self.dh.get_client(s.id()).ok());
        set_data_device_focus(&self.dh, &self.seat, focused.or_else(|| Some(client.clone())));
    }

    fn paste_serve(&mut self, n: u64, mime: &str, fd: OwnedFd) {
        let Some(text) = self.paste.open.as_ref().filter(|(open, _)| *open == n && MIMES.contains(&mime)).map(|(_, t)| t.clone()) else {
            // An old offer or a kind not offered: the pipe ends empty.
            return;
        };
        self.paste.served += 1;
        // A program that never reads cannot hold the loop: the pipe is written on a thread of its own.
        let spawned = std::thread::Builder::new().name("bitscomp-paste".into()).spawn(move || {
            let _ = std::fs::File::from(fd).write_all(text.as_bytes());
        });
        if let Err(e) = spawned {
            eprintln!("bitscomp: durchblick: no thread for the paste: {e}");
        }
        // The write sees it now, before Georg's next move could be taken for a stop.
        self.desk_write_tick();
    }
}

impl Dispatch<WlDataOffer, Private> for App {
    fn request(app: &mut App, _: &Client, _: &WlDataOffer, req: wl_data_offer::Request, data: &Private, _: &DisplayHandle, _: &mut DataInit<'_, App>) {
        if let wl_data_offer::Request::Receive { mime_type, fd } = req {
            app.paste_serve(data.0, &mime_type, fd);
        }
    }
}
