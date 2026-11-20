//! Platform rules for Georg's Gaming-PC (ASRock Z890 Taichi OCF, Intel 265K): `plan/spec/gaming-pc.md`. Pure logic with tests; the wiring crates (pci, block, acpi, time, bitsboot) call in
//! here so the rules are proven on the host. Every rule is keyed so that the Mini-PC's behaviour stays identical.
//!
//! One owner per module (wave GP-A, 14.11.2026), so three builders can work side by side:
//! - `facts`, `peer_bus`, `never_touch`, `vmd`: wave GP-BUS;
//! - `disk_rule`, `tiers`: wave GP-GUARD;
//! - `clock`, `madt`, `dmar`: wave GP-CPU.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod clock;
pub mod disk_rule;
pub mod dmar;
pub mod facts;
pub mod madt;
pub mod never_touch;
pub mod peer_bus;
pub mod tiers;
pub mod vmd;
