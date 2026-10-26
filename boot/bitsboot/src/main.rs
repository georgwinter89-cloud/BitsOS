//! bitsboot: the BitsOS V2 UEFI loader. No menu: it picks a system generation from its own volume by the
//! marker rule (V2-PLAN §7; the only write is the TRY marker below `\BITSOS2\SYS\`), switches the screen to
//! the native mode, loads the kernel ELF and its modules, records everything in a BootInfo (crate
//! `bootinfo`), builds the first page tables and jumps to the kernel. Every failure before the jump ends on a readable screen (also on serial
//! where the firmware mirrors its console) and a return to the firmware; it never writes UEFI variables.
//!
//! # Kernel entry contract
//!
//! - Long mode, interrupts off (`cli`), DF clear.
//! - CR3 = bitsboot's PML4 (record `PAGE_TABLES`, pages of type `BITS_PAGETABLE`). It maps:
//!   the kernel span at its ELF virtual addresses (4 KiB pages, W only for `PF_W`, NX unless `PF_X`);
//!   every RAM range of the memory map at `layout::DIRECT_MAP_BASE + phys` (write-back, NX, writable);
//!   the GOP framebuffer at `layout::VMAP_BASE` (PWT=1 PCD=0: PAT entry 1, which the kernel programs to WC);
//!   the relocation stub page identity-mapped (executable). Nothing else (no identity map of low memory).
//! - EFER.NXE is set when the CPU has NX (flag `NXE_SET`); CR4.CET is clear (flag `CET_TURNED_OFF` if bitsboot
//!   cleared it); LA57 was refused. CR4.OSXSAVE and XCR0 are as the firmware left them: setting them up is
//!   the kernel's job (CP-1). PAT is the firmware's (entry 1 is WT until the kernel reprograms it).
//! - The TLB holds no firmware translations: the stub loads CR3 and toggles CR4.PGE (flushes global pages).
//! - GDT and IDT are still the firmware's, but neither they nor the firmware's handlers are mapped any more:
//!   until the kernel's first `lgdt`/`lidt` any exception, NMI or machine check ends in a triple fault
//!   (reset). The kernel's entry code must therefore load its own GDT and IDT before anything that can fault.
//! - `rdi` = BootInfo virtual address (direct map), `rsi` = BootInfo physical address.
//! - `rsp` = direct-map address of the top of a 16 KiB boot stack (`BITS_KERNEL` pages), 16-byte aligned with
//!   no return address pushed: an `extern "C"` entry must be naked or realign; the kernel switches stacks at once.
//! - Boot services are gone; the kernel never calls runtime services.
#![no_std]
#![no_main]

extern crate alloc;

mod cpu;
mod elf;
mod files;
mod generation;
mod gop;
mod handover;
mod load;
mod logbuf;
mod machine;
mod markers;
mod mem;
mod paging;
mod stick;

use alloc::string::String;
use core::fmt::Write;
use core::time::Duration;
use uefi::prelude::*;
use uefi::proto::console::text::{Color, Key};

/// A readable reason why the boot stopped (shown on screen, then back to the firmware).
pub struct Fail(pub String);
pub type R<T> = core::result::Result<T, Fail>;

#[macro_export]
macro_rules! fail {
    ($($t:tt)*) => { $crate::Fail(alloc::format!($($t)*)) };
}

/// Attaches "what was being done" to a UEFI error.
pub trait Ctx<T> {
    fn ctx(self, what: &str) -> R<T>;
}
impl<T, D: core::fmt::Debug> Ctx<T> for uefi::Result<T, D> {
    fn ctx(self, what: &str) -> R<T> {
        self.map_err(|e| fail!("{what} (UEFI status {:?})", e.status()))
    }
}

#[entry]
fn main() -> Status {
    logbuf::init();
    match load::run() {
        Ok(never) => match never {},
        Err(f) => {
            gop::restore_firmware_mode();
            show_error(&f.0);
            let pages = mem::free_all();
            ::log::info!("bitsboot: gave {pages} pages back; returning to the firmware");
            Status::LOAD_ERROR
        }
    }
}

/// The error screen: what went wrong in words, then a key or a timeout, then the caller returns to the
/// firmware (which boots the next entry of Georg's boot order). Under a hypervisor (the OVMF tests) the wait
/// is 5 s instead of 60 s.
fn show_error(msg: &str) {
    ::log::error!("bitsboot: cannot start BitsOS: {msg}");
    let secs: u32 = if cpu::hypervisor() { 5 } else { 60 };
    uefi::system::with_stdout(|o| {
        let _ = o.set_color(Color::White, Color::Red);
        let _ = write!(o, "\nBitsOS kann nicht starten. / BitsOS cannot start.");
        let _ = o.set_color(Color::LightGray, Color::Black);
        let _ = write!(o, "\n{msg}\n");
        let _ = writeln!(
            o,
            "Taste drücken oder {secs} s warten: zurück zur Firmware. / Press a key (or wait {secs} s) to return to the firmware."
        );
    });
    uefi::system::with_stdin(|i| {
        let _ = i.reset(false);
        for _ in 0..secs * 10 {
            if let Ok(Some(Key::Printable(_) | Key::Special(_))) = i.read_key() {
                break;
            }
            boot::stall(Duration::from_millis(100));
        }
    });
}

/// Before ExitBootServices: the same error screen, then return to the firmware. After it: a line on COM1
/// and halt for good (nothing else can show anything).
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    if logbuf::console_on() {
        show_error(&alloc::format!("internal error: {info}"));
        mem::free_all();
        // SAFETY: Exit with our own image handle and no exit data; boot services are still active.
        unsafe {
            let _ = boot::exit(boot::image_handle(), Status::ABORTED, 0, core::ptr::null_mut());
        }
    }
    logbuf::serial_line(&[b"bitsboot: internal error after ExitBootServices; halted"]);
    loop {
        // SAFETY: stopping this CPU for good.
        unsafe { core::arch::asm!("cli; hlt") };
    }
}
