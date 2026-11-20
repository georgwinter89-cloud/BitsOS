//! USB Audio Class 2, for the Gaming-PC's board jacks: an ASRock 26ce:0a09 behind a hub on the xHCI, not the Intel
//! audio controller, which has no codec. Lane GP built it as the pure half of AU-3 ;
//! GP-SOUND (`plan/spec/gaming-pc.md` section 7) binds it in `usb-audio` (`uac2dev.rs`), which only devices with
//! bInterfaceProtocol 0x20 reach, so the UAC1 path of the JBL is unchanged.
//!
//! Sources: `V:` is Linux 6.6 `include/linux/usb/audio-v2.h`, `F:` is `sound/usb/format.c`, `C:` is
//! `sound/usb/clock.c`, `E:` is `sound/usb/endpoint.c` of 6.6 (the skeleton's); what GP-SOUND added names Linux 7.3.1
//! files with their lines in full. Layout:
//! - `desc`: a function as a tree of clocks, terminals, feature units and alternate settings;
//! - `parse`: the configuration descriptor's bytes into that tree;
//! - `range`: the answer to a clock's RANGE request;
//! - `clock`: the class requests of a clock source, and Linux's order for starting a stream;
//! - `plan`: from "the speaker, S16, stereo, 48 kHz" to an interface, an alternate setting and a clock;
//! - `feature`: mute and volume of a feature unit (UAC2 CUR and RANGE);
//! - `fixture` (tests, feature `fixture`): the board device's configuration descriptor.
//!
//! `plan/spec/gaming-pc.md` section 7 lists what is not built.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod clock;
pub mod desc;
pub mod feature;
#[cfg(any(test, feature = "fixture"))]
pub mod fixture;
pub mod parse;
pub mod plan;
pub mod range;

#[cfg(test)]
mod tests;
