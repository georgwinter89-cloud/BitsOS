//! GX-10 PS: the bootloader's note across a Reload (`rdna4_core::bootnote`), in the Reload's driver-note slots next
//! to the card's stop note, under a tag of its own (`handoff::boot_tag_for`). The kernel glue calls these around
//! the stage `sos` (`livestage`) and at the card's attach (`live`); what they decide is host-tested.

use super::stage::{Session, Sos};
use crate::devs::Key;
use crate::gpucore::handoff::NoteIo;
use rdna4_core::bootnote::BootNote;

/// What the kernel before this Reload handed on about the card's bootloader.
pub fn inherited(io: &NoteIo, key: Key) -> Option<BootNote> {
    BootNote::handed(io.take_boot_for(key).as_deref())
}

pub fn put(io: &NoteIo, key: Key, n: &BootNote) -> bool {
    io.put_boot_for(key, &n.bytes())
}

/// What this kernel hands on after the stage `sos` ended: its own sends once it sent, else what it inherited.
pub fn hand_on(s: &Session) -> BootNote {
    match s.boot_sent() {
        0 => s.boot_before.unwrap_or(BootNote::NOTHING),
        n => BootNote {
            sent: n as u8,
            failed: match s.sos {
                Some(Sos::Failed { part, c35 }) => Some((part, c35)),
                _ => None,
            },
        },
    }
}

/// At the card's attach: a note that says the bootloader got something is handed on again at once, so a kernel in
/// which the stage `sos` never runs does not end the chain (a note crosses exactly one Reload).
pub fn keep(io: &NoteIo, key: Key) -> Option<(BootNote, bool)> {
    let n = inherited(io, key).filter(BootNote::spent)?;
    Some((n, put(io, key, &n)))
}
