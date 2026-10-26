//! The two machine resets safe code cannot reach (owner: CP, item CP-5; spec/core.md §4): the chipset's reset
//! control register 0xCF9 (inside the PCI configuration port range that `portio` refuses) and the triple fault.
//! The order and the waits between the paths are `acpi::power`'s; this file only does the one thing each.

/// The reset control register of Intel and AMD chipsets ("CF9").
pub const CF9: u16 = 0xCF9;
/// CF9 bit 1: system reset (hard reset when bit 3 is set too), bit 2: do it now (Linux `BOOT_CF9_FORCE`).
pub const CF9_SYS_RST: u8 = 0x02;
pub const CF9_RST_CPU: u8 = 0x04;
/// CF9 bit 3: full reset (power cycles the platform on Intel PCHs; Linux's cold-reboot code 0x0E sets it).
pub const CF9_FULL_RST: u8 = 0x08;
/// The two reset codes: hard reset (spec: 2 then 6) and full reset (Linux `reboot.c:695` cold reboot).
pub const CF9_CODE_HARD: u8 = CF9_SYS_RST | CF9_RST_CPU;
pub const CF9_CODE_FULL: u8 = CF9_SYS_RST | CF9_RST_CPU | CF9_FULL_RST;

/// The two writes of a CF9 reset with `code`, the register's other bits kept from `cur` (pure; host-tested):
/// first `SYS_RST` alone (arming), then the whole code (Linux `reboot.c:694-700`).
pub const fn cf9_writes(cur: u8, code: u8) -> (u8, u8) {
    let base = cur & !code;
    (base | CF9_SYS_RST, base | code)
}

/// Reads 0xCF9 (the bits a reset write keeps).
pub fn read_cf9() -> u8 {
    crate::portio::raw::inb(CF9)
}

/// Writes `value` to 0xCF9 (2 then 6 is the reset sequence; the FADT's reset register may name 0xCF9 with its own
/// value). Only the reset paths call this.
pub fn write_cf9(value: u8) {
    crate::portio::raw::outb(CF9, value);
}

/// Ends this machine's run with a triple fault: interrupts off, an empty IDT, then `int3` (Linux `BOOT_TRIPLE`).
/// The CPU shuts down and the chipset resets the machine. Never returns.
pub fn triple_fault() -> ! {
    imp::triple_fault()
}

#[cfg(target_os = "none")]
mod imp {
    pub fn triple_fault() -> ! {
        // SAFETY: deliberately ending the machine's run (the caller asked for a reset): with interrupts off and a
        // zero-limit IDT, int3 cannot be delivered (#GP, then #DF, then shutdown), and NMIs fault the same way.
        unsafe {
            let null = [0u8; 10];
            core::arch::asm!("cli", "lidt [{}]", "int3", "2:", "hlt", "jmp 2b", in(reg) null.as_ptr(), options(noreturn));
        }
    }
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn triple_fault() -> ! {
        panic!("triple fault requested on the host")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf9_codes_keep_other_bits() {
        assert_eq!(cf9_writes(0, CF9_CODE_HARD), (0x02, 0x06));
        assert_eq!(cf9_writes(0, CF9_CODE_FULL), (0x02, 0x0E));
        assert_eq!(cf9_writes(0xF1, CF9_CODE_HARD), (0xF3, 0xF7), "bits outside the code stay");
        assert_eq!(cf9_writes(0xF9, CF9_CODE_HARD), (0xFB, 0xFF), "bit 3 too (Linux keeps it: firmware's choice)");
        assert_eq!(cf9_writes(0x0E, CF9_CODE_FULL), (0x02, 0x0E), "an earlier attempt's bits are cleared first");
    }
}
