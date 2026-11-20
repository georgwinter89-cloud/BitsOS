//! Can the framebuffer be mapped at all? (plan/spec/gaming-pc.md §5.) The Gaming-PC's picture lies in the graphics
//! card's 16 GiB BAR far above 512 GiB (0xa0_0000_0000 on 14.11.2026); that is fine for a CPU with 46 address bits.
//! An address past the CPU's bits is not: a page table entry for it has reserved bits set and the first pixel
//! written would fault. `mm::vmap` refuses only past 2^52, so the check sits here, before any mapping, and a
//! refusal is one plain line and a kernel without a picture instead of a crash.

/// Why a framebuffer range is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreachable {
    /// No address or no bytes.
    Empty,
    /// `phys + size` runs past the end of the address space.
    Wraps,
    /// The last byte lies at or past `limit`, the first address the CPU cannot form.
    PastCpu { end: u64, limit: u64 },
}

/// `[phys, phys + size)` rounded out to whole pages lies inside the CPU's physical address space.
/// `limit`: 2^(physical address bits), `hal::mmio::phys_limit`.
pub fn reachable(phys: u64, size: u64, limit: u64) -> Result<(), Unreachable> {
    if phys == 0 || size == 0 {
        return Err(Unreachable::Empty);
    }
    let end = phys.checked_add(size).and_then(|e| e.checked_add(0xFFF)).ok_or(Unreachable::Wraps)? & !0xFFF;
    if end > limit {
        return Err(Unreachable::PastCpu { end, limit });
    }
    Ok(())
}

/// The CPU's limit from CPUID 0x80000008 (2^52 when the leaf is missing), as `hal::mmio::init` takes it.
pub fn cpu_limit() -> u64 {
    let v = hal::cpu::live();
    let bits = if v.max_ext() >= 0x8000_0008 { v.get(0x8000_0008, 0).eax & 0xFF } else { 0 };
    hal::mmio::phys_limit(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2560 x 1440 at 4 bytes: what the Gaming-PC's firmware hands over.
    const PICTURE: u64 = 2560 * 1440 * 4;
    const BITS_46: u64 = 1 << 46;

    #[test]
    fn the_gaming_pcs_picture_far_above_512_gib_is_reachable() {
        for base in [0xa0_0000_0000u64, 0xa8_0000_0000] {
            assert!(base > 512 << 30);
            assert_eq!(reachable(base, PICTURE, BITS_46), Ok(()));
        }
        // The Mini-PC's picture below 512 GiB with its 48 bits, and QEMU's below 4 GiB, as before.
        assert_eq!(reachable(0x7C_0000_0000, PICTURE, 1 << 48), Ok(()));
        assert_eq!(reachable(0x8000_0000, 1280 * 800 * 4, 1 << 40), Ok(()));
    }

    #[test]
    fn a_made_up_address_past_the_cpus_bits_is_refused() {
        // The same picture on a CPU with 39 bits: 0xa0_0000_0000 is past 512 GiB, which is all it can form.
        assert_eq!(reachable(0xa0_0000_0000, PICTURE, 1 << 39), Err(Unreachable::PastCpu { end: 0xa0_0000_0000 + PICTURE.next_multiple_of(4096), limit: 1 << 39 }));
        // Just past 46 bits, and one page straddling the limit.
        assert!(matches!(reachable(BITS_46, PICTURE, BITS_46), Err(Unreachable::PastCpu { .. })));
        assert!(matches!(reachable(BITS_46 - 4096, 4097, BITS_46), Err(Unreachable::PastCpu { .. })));
        assert_eq!(reachable(BITS_46 - 4096, 4096, BITS_46), Ok(()), "the last page below the limit");
        // Between 2^46 and 2^52: `mm::vmap` would have mapped this.
        assert!(matches!(reachable(1 << 50, PICTURE, BITS_46), Err(Unreachable::PastCpu { .. })));
    }

    #[test]
    fn empty_and_wrapping_ranges_are_refused() {
        assert_eq!(reachable(0, PICTURE, BITS_46), Err(Unreachable::Empty));
        assert_eq!(reachable(0xa0_0000_0000, 0, BITS_46), Err(Unreachable::Empty));
        assert_eq!(reachable(u64::MAX - 100, PICTURE, u64::MAX), Err(Unreachable::Wraps));
        assert_eq!(reachable(u64::MAX - 0x1000, 0x800, u64::MAX), Err(Unreachable::Wraps), "the page rounding wraps");
    }

    #[test]
    fn this_hosts_cpu_gives_a_plausible_limit() {
        let l = cpu_limit();
        assert!(l.is_power_of_two() && (1 << 32..=1 << 52).contains(&l), "{l:#x}");
    }
}
