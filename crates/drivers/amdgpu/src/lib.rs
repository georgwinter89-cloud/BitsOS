//! Radeon 780M as one whole-GPU device object (owner: GX). GX-D1: `gpu::Gpu` owns the PCI function, its BARs and
//! the MMIO owner (`dcn::DcnRegs`); GX-D2: IP blocks by version (`ip`); the display engine is the `dcn` part:
//! the read-only attach (`attach`, V1 gates 1-8), the KMS view (`kms`, GX-D5), the snapshot and its MCP tools
//! (`snapshot`, `tools`); GX-D3: buffer objects over the carve-out (`bo`); GX-3: page flips (`flip` register
//! sequences, `scanout` state, `live` kernel glue and gopfb's fbdev engine). Design notes for GX-D3..D6 and D9:
//! plan/notes/gx-GX-2.md. GX-6/GX-6m (render side): the common core (`gpucore`: discovery, firmware names), one
//! module per IP block and version chosen from the discovery table (`ipmod`), the read-only probe (`probe`,
//! `tools_gx`). GX-7a: the PSP bring-up against a fake (`gpucore::psp`, `ucode`, `fwload`, `pspfake`,
//! `ipmod::psp13`). GX-7b: the bring-up register owner and the start-state classifier (`gpucore::bringup`,
//! `gpucore::classify`), their live glue (`state`) and the carve-out as the CPU reads it (`carve`).
//! GX-7c: the fakes grown to the real chip, the scanout watcher and the Linux-diff harness (`gpucore::{smufake,
//! gcfake, fakechip, watch, regtrace, regdiff}`, `ktests_fakes`). GX-7d: the real PSP path, M1/M2 (`gpucore::
//! pspbringup` the run, `psplive` the live device, bus master and Quiesce, MCP `gx_bringup`; `ktests_bringup`).
//! GX-7h: the GART and VMID 0 on both hubs (`gpucore::{gmc, carvetable, hubfake}`, `gx_bringup {"until":"gart"}`;
//! `ktests_gmc`).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod attach;
pub mod bo;
pub mod carve;
pub mod cursor;
pub mod dcn;
pub mod edid_modes;
pub mod flip;
pub mod flip_test;
pub mod gpu;
pub mod gpucore;
pub mod ip;
pub mod ipmod;
pub mod kms;
pub mod layout;
pub mod live;
pub mod mode;
pub mod modeset;
pub mod probe;
pub mod psplive;
pub mod switch;
pub mod refusal;
pub mod scanout;
pub mod snapshot;
pub mod state;
pub mod tools;
pub mod tools_cursor;
pub mod tools_gx;

mod ktests;
mod ktests_bringup;
mod ktests_fakes;
mod ktests_gmc;
mod ktests_gx;
mod ktests_psp;
mod ktests_gfxring;
mod ktests_mes;
mod ktests_sdma;
mod ktests_reinit;
mod ktests_vm;

#[cfg(test)]
mod tests;
