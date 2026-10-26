//! CP's kernel-side platform code (owner: CP), on top of `hal` (the unsafe primitives) and `acpi` (the tables):
//! - `cpu_setup`: the BSP's CPU set-up that needs memory and the log (CP-1: guarded IST stacks, facts, MCA);
//! - `smp`: SMP bring-up (CP-4): the APs from the MADT started one by one through `hal::smp`, each one checked
//!   (relaxed), its topology recorded for the scheduler, its TSC offset measured, everything logged (B6);
//! - `power` (CP-5): power-off and reboot for the console command (quiesce, then `acpi::power`);
//! - `timers` (CP-5): every AP's LAPIC timer started on that AP, the scheduler's tick and interrupt-exit hooks;
//! - pure helpers with host tests: `plan` (which APs to start), `limit` (CONFIG `smp=`/`cpus=`, CP-4b), `tsc`
//!   (offsets from the handshake samples), `report` (comparisons and text).
//!
//! CP-5's ktests (`time_ktest`, `power_ktest`) live here because they need the scheduler.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cpu_setup;
pub mod limit;
pub mod plan;
pub mod power;
pub mod report;
pub mod smp;
pub mod timers;
pub mod tsc;

mod cpu_ktest;
mod smp_ktest;
mod limit_ktest;
mod power_ktest;
mod time_ktest;
