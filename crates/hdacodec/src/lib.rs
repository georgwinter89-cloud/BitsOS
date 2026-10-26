//! HD Audio codec graph (owner: SN, item SN-1).
//!
//! Turns a codec's answers to read-only verbs into a model ([`model::Codec`]), finds the output
//! pins and their way to a converter ([`path`]), and gives the verbs that set an output up as a
//! plain list ([`setup`], [`ati`]) for the controller driver to send. Nothing here touches
//! hardware: answers come from a [`VerbSource`] (the CORB/RIRB driver later, a text dump now,
//! see [`dump`] and [`walk`]).
//!
//! V1 sources this follows: `BitsOS-Plattform/snd/codec.asm`, `widget.asm`, `path.asm`,
//! `pins.asm`, `hdmi.asm`, `switch.asm`.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod ati;
pub mod caps;
pub mod conn;
pub mod dpaudio;
#[cfg(any(test, feature = "fake"))]
pub mod dpaudio_fake;
pub mod dump;
pub mod eld;
pub mod input;
pub mod model;
pub mod path;
pub mod setup;
pub mod verb;
pub mod walk;

#[cfg(any(test, feature = "synth"))]
pub mod synth;

#[cfg(test)]
mod tests;

/// A node ID. The command word has 7 bits for it (bits 26:20; bit 27 must be 0), so only
/// `0..=NID_LAST` can be addressed; anything larger in a codec's answers is treated as invalid.
pub type Nid = u8;

/// Highest addressable node ID (a protocol fact, not a chosen limit).
pub const NID_LAST: Nid = 0x7F;

/// Highest codec address on a link (4 bits in the command word).
pub const CAD_LAST: u8 = 0x0F;

/// The codec did not answer this verb (timeout on hardware, a missing or `-` line in a dump).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoResponse;

/// Something that answers read-only verbs for one codec.
pub trait VerbSource {
    /// Sends `verb` (20 bits: verb ID and payload) to node `nid` and returns the 32-bit answer.
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse>;
}

impl<S: VerbSource + ?Sized> VerbSource for &mut S {
    fn get(&mut self, nid: Nid, verb: u32) -> Result<u32, NoResponse> {
        (**self).get(nid, verb)
    }
}
