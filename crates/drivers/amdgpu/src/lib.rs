//! Radeon 780M as one whole-GPU device object (owner: GX). GX-D1: `gpu::Gpu` owns the PCI function, its BARs and
//! the MMIO owner (`dcn::DcnRegs`); GX-D2: IP blocks by version (`ip`); the display engine is the `dcn` part:
//! the read-only attach (`attach`, V1 gates 1-8), the KMS view (`kms`, GX-D5), the snapshot and its MCP tools
//! (`snapshot`, `tools`); GX-D3: buffer objects over the carve-out (`bo`); GX-3: page flips (`flip` register
//! sequences, `scanout` state, `live` kernel glue and gopfb's fbdev engine). Design notes for GX-D3..D6 and D9:
//! plan/notes/gx-GX-2.md.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod attach;
pub mod bo;
pub mod cursor;
pub mod dcn;
pub mod edid_modes;
pub mod flip;
pub mod flip_test;
pub mod gpu;
pub mod ip;
pub mod kms;
pub mod layout;
pub mod live;
pub mod mode;
pub mod modeset;
pub mod switch;
pub mod refusal;
pub mod scanout;
pub mod snapshot;
pub mod tools;
pub mod tools_cursor;

mod ktests;

#[cfg(test)]
mod tests;
