//! Sound above the controllers. Owner: SN. SN-3: the mixer (`snd/mix`, KernelRt): any number of clients, each
//! with its ring at its own rate (resampled to 48 kHz by a polyphase filter), volume per client and master,
//! saturating sum, one output (`hda` jack, HDMI once SN-5 opens it) switched without losing frames, client and
//! output underruns as silence and counted. SN-4 builds the ALSA PCM on `Playback`.
//!
//! Layout: `fmath` (f64 helpers without libm, filter design only), `resample` (tables, per-client filter),
//! `ring` (client ring), `client` (`Playback`: write/start/pause/drain/delay/volume), `backend` (what an
//! output is; the null output), `mixer` (`Core::step`, kernel-free), `hdaback` (the back end on `hda`),
//! `usbback` (USB audio, SN-8b, and the kernel mixer's hda + USB pair), `thread` (`Mixer`, the thread,
//! `mixer()`), `output` and `tool` (SN-10: `/sys/bitsos/snd_output` and the MCP tool `snd`), `boot` (SN-11: a stored `hdmi`
//! switched on after a boot), `capture` and `capsrc` (SN-9: recording from a USB microphone or an hda input), `ktests`. Latency: plan/notes/sn-SN-3.md.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod alsa;
pub mod backend;
pub mod boot;
pub mod capsrc;
pub mod capture;
pub mod client;
pub mod fmath;
mod hdaback;
mod ktests;
mod ktests_alsa;
mod ktests_cap;
pub mod mixer;
pub mod resample;
pub mod ring;
pub mod testsig;
mod thread;
mod usbback;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_alsa;
#[cfg(test)]
mod tests_capture;
#[cfg(test)]
mod tests_alsa_abi;
#[cfg(test)]
mod tests_hda;
#[cfg(test)]
mod tests_rs;
#[cfg(test)]
mod tests_usb;
mod ktests_usb;
mod ktests_out;
mod helper;
pub mod output;
mod tool;
#[cfg(test)]
mod tests_out;

pub use backend::{Endpoint, OutError, Port, Target};
pub use client::{Playback, SndError, State, UNITY};
pub use mixer::{ClientInfo, Config, Status};
pub use thread::{Mixer, config, mixer, parse_output, set_output, started};
