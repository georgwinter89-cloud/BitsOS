//! Trusted base (V2-PLAN §2.1, §2.2): the only crate besides `ksync`, `mm-core`, `kreg` and `kernel/` with
//! `unsafe`. Every unsafe block carries a `// SAFETY:` rule and is reviewed by the hal checker.
//!
//! Module ownership (V2-LANES §2): cpu, gdt, idt, apic, smp, trampoline, msr, percpu, vectors -> CP;
//! paging, dma, pat -> MM; switch, xsave -> SC; mmio, portio, msi -> PC; nmi, panic, kexec_stub -> RB;
//! entry, usercopy, signal_frame -> AB; serial and this file -> desk; drv_<lane> -> that driver lane.
//! The G0 versions are crude but have the final API shape.
//!
//! Host builds (`cargo test` on CachyOS): privileged code is compiled only for `target_os = "none"`; the host
//! gets harmless shims so the crates above can be unit-tested.
#![no_std]
#![allow(clippy::missing_safety_doc)]

extern crate alloc;

pub mod apic;
pub mod cpu;
pub mod dma;
pub mod dma_buf;
pub mod entry;
pub mod gdt;
pub mod idt;
pub mod kexec_stub;
pub mod mmio;
pub mod msi;
pub mod msr;
pub mod nmi;
pub mod paging;
pub mod panic;
pub mod pat;
pub mod percpu;
mod percpu_free;
pub mod portio;
pub mod serial;
pub mod signal_frame;
pub mod smp;
pub mod switch;
pub mod trampoline;
pub mod usercopy;
pub mod vectors;
pub mod xsave;

pub mod drv_gx;
pub mod drv_in;
pub mod drv_dp;
pub mod drv_ks;
pub mod drv_nv;
pub mod drv_sn;
pub mod drv_wl;

/// True when compiled for the bare-metal kernel target (false in host unit tests).
pub const BARE_METAL: bool = cfg!(target_os = "none");

pub use bootinfo::layout::{phys_to_virt, virt_to_phys, DIRECT_MAP_BASE, KERNEL_BASE, PAGE_SIZE, USER_TOP, VMAP_BASE};
