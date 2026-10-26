//! The AMD FCH watchdog (owner: RB, item RB-3; spec/core.md §10.1, V2-PLAN §4 "Watchdog"): it resets the machine
//! when the kernel stops answering, so a hang comes back to V2 by Georg's boot order.
//!
//! - `regs`: the chip (FCH 1022:790B rev >= 0x51, ACPI MMIO 0xFED80300 PM / 0xFED80B00 watchdog) and its
//!   sequence as `V1:hw/watchdog.asm:103-154` and Linux `drivers/watchdog/sp5100_tco.c` (efch_mmio); a fake
//!   register block for tests.
//! - `live`: who is alive (heartbeat moved within the window, or idle with interrupts on and answering probes).
//! - `dog`: one watchdog (registers + CPU source + per-CPU tracks); `check` kicks only when all CPUs are alive.
//! - `system`: the machine's watchdog (start, FIRED at boot, kicker hrtimer, reporter with NMI backtraces,
//!   Quiesce, `deliberate_hang`).
//! - `tool`: MCP `watchdog` and `hang`.
#![cfg_attr(target_os = "none", no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod cmd;
pub mod config;
pub mod dog;
mod ktests;
pub mod live;
pub mod regs;
pub mod system;
pub mod tool;

pub use system::{deliberate_hang, state, HangMode, State};

#[cfg(test)]
mod tests;
