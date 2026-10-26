//! Reload (owner: RB): a new kernel without a reboot, kexec-like (V2-PLAN §7, spec/core.md §2.4, net.md §5.3).
//!
//! RB-5a, single CPU: the MCP tool `reload` fetches a bundle (kernel ELF + initramfs) over http from the LAN
//! (NW-3's client; R9), checks the ELF while it streams in and places its segments in frames the running
//! kernel's allocator hands out (`stage`), builds the new kernel's page tables the way bitsboot does (`tables`)
//! and its BootInfo v1 (`handover`: every record handed on unchanged except kernel image, initramfs module,
//! page tables, TSC, Reload counter + 1 and the memory map, `memmap`), answers, quiesces every device
//! (`devmodel::quiesce_all`) and jumps through the stub in bitsboot's `KEEP` stub page (`hal::kexec_stub`).
//! The new kernel starts at the same entry a cold boot uses, reads its CONFIG and its new initramfs from the
//! BootInfo and logs `kexec handover reload=n` with the initramfs hash (`boot`).
//! RB-5b, all CPUs: before the quiesce the other CPUs are parked (park IPI, NMI if needed), their LAPICs made
//! quiet, and after it they get INIT, so they wait for the new kernel's SIPI (`hal::kexec_stub::stop_others`);
//! every LAPIC LVT and I/O APIC pin masked. The scanout stays the GOP's: nothing in this kernel changes it yet
//! (SN-5's DP audio writes leave OTG/HUBP alone and are undone in Quiesce), so there is no display restore.
//! RB-6, `install`: the MCP tool `install_kernel` writes a checked bundle as a new system generation on the boot
//! stick (FS-7's SYS API), and a kernel started as a generation's one try confirms itself once MCP has answered
//! for 2 minutes.
//! RB-2, the panic path (`crash`): NMI stop of the other CPUs with their registers, frame-pointer backtraces with
//! symbol names, a crash record in the crash page that the next boot logs, `panic=halt|reboot`.
#![cfg_attr(target_os = "none", no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod boot;
pub mod crash;
mod cpu_ktests;
mod quiesce_ktests;
pub mod elf;
pub mod handover;
pub mod install;
pub(crate) mod ktests;
pub mod testkit;
pub mod memmap;
pub mod refresh;
pub mod stage;
pub mod tables;
pub mod tool;
pub mod url;

pub use boot::{boot_info, set_boot_info};

/// FNV-1a 64 (the initramfs hash in the log; the scenario computes the same on the host).
#[derive(Clone, Copy, Debug)]
pub struct Fnv(u64);

impl Fnv {
    pub const fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
