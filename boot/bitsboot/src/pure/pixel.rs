//! GOP pixel formats and modes as BootInfo records them.

use alloc::string::String;
use bootinfo::{Mode, PixelFormat};
use core::fmt::Write;

/// UEFI `EFI_GRAPHICS_PIXEL_FORMAT`: 0 = PixelRedGreenBlueReserved8BitPerColor (red in the lowest byte),
/// 1 = PixelBlueGreenRedReserved8BitPerColor, 2 = PixelBitMask, 3 = PixelBltOnly. A bit mask that is exactly
/// one of the two 8-bit layouts is recorded as that layout, so the kernel needs only two fast paths.
pub fn from_uefi(format: u32, masks: [u32; 4]) -> (PixelFormat, [u32; 4]) {
    const R_LOW: [u32; 3] = [0x0000_00FF, 0x0000_FF00, 0x00FF_0000];
    const B_LOW: [u32; 3] = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF];
    match format {
        0 => (PixelFormat::Rgbx, [0; 4]),
        1 => (PixelFormat::Bgrx, [0; 4]),
        2 if masks[..3] == R_LOW => (PixelFormat::Rgbx, [0; 4]),
        2 if masks[..3] == B_LOW => (PixelFormat::Bgrx, [0; 4]),
        2 if masks[..3].iter().all(|&m| m != 0) => (PixelFormat::Bitmask, masks),
        _ => (PixelFormat::None, [0; 4]),
    }
}

/// Bytes per pixel the kernel will assume for a linear format (bit masks: the highest set bit decides).
pub fn bytes_per_pixel(format: PixelFormat, masks: [u32; 4]) -> u64 {
    match format {
        PixelFormat::Rgbx | PixelFormat::Bgrx => 4,
        PixelFormat::Bitmask => {
            let all = masks.iter().fold(0u32, |a, m| a | m);
            (32 - all.leading_zeros()).div_ceil(8).max(1) as u64
        }
        PixelFormat::None => 0,
    }
}

/// A linear framebuffer the kernel can draw into: non-zero size, stride at least the width, and the firmware's
/// size covering `stride * height` pixels. `Err` says what is wrong.
pub fn check_framebuffer(phys: u64, size: u64, width: u32, height: u32, stride: u32, bpp: u64) -> Result<(), &'static str> {
    if phys == 0 || size == 0 {
        return Err("the firmware reports no framebuffer address");
    }
    if width == 0 || height == 0 || bpp == 0 {
        return Err("the mode has no pixels");
    }
    if stride < width {
        return Err("the stride is smaller than the width");
    }
    if (stride as u64) * (height as u64) * bpp > size {
        return Err("the framebuffer is smaller than stride x height");
    }
    if phys.checked_add(size).is_none() {
        return Err("the framebuffer runs past the end of the address space");
    }
    Ok(())
}

/// The picture far above 512 GiB (plan/spec/gaming-pc.md §5): the whole framebuffer lies inside what the CPU can
/// address (`phys_bits` from CPUID 0x80000008; 0 or an implausible value counts as x86's most, 52). The Gaming-PC's
/// lies far above 512 GiB (the graphics card's 16 GiB BAR at 0xa0_0000_0000) and is fine with its 46 bits; an address
/// past the CPU's bits would put reserved bits into the page table entries bitsboot builds for it, and the first
/// pixel drawn would fault. Such a framebuffer is handed over as none (headless), with a line.
pub fn check_reachable(phys: u64, size: u64, phys_bits: u32) -> Result<(), &'static str> {
    let limit = if (1..=52).contains(&phys_bits) { 1u64 << phys_bits } else { 1u64 << 52 };
    match phys.checked_add(size).and_then(|e| e.checked_add(0xFFF)) {
        Some(end) if end & !0xFFF <= limit => Ok(()),
        _ => Err("the framebuffer lies above what this CPU can address"),
    }
}

/// Spec core.md 2.1: a framebuffer larger than this is never chosen (BT-2); for the firmware's own mode it is
/// only a warning, the kernel maps what the firmware reports.
pub const FB_LARGE: u64 = 64 << 20;

/// True if `[phys, phys + size)` touches any RAM range `(start, end)` of the memory map: the kernel would then
/// map RAM a second time as a write-combining alias, so the loader hands over no framebuffer instead.
pub fn overlaps_ram(phys: u64, size: u64, ram: impl IntoIterator<Item = (u64, u64)>) -> bool {
    let end = phys.saturating_add(size);
    ram.into_iter().any(|(s, e)| phys < e && s < end)
}

pub fn format_name(f: PixelFormat) -> &'static str {
    match f {
        PixelFormat::Bgrx => "BGR",
        PixelFormat::Rgbx => "RGB",
        PixelFormat::Bitmask => "bitmask",
        PixelFormat::None => "no framebuffer",
    }
}

/// One line for the loader log: `0:1280x800 1:800x600 ... (30 modes, current 0)`.
pub fn mode_summary(modes: &[Mode], current: u32) -> String {
    let mut s = String::new();
    for m in modes {
        let _ = write!(s, "{}:{}x{}", m.index, m.width, m.height);
        if m.format != PixelFormat::Bgrx {
            let _ = write!(s, "/{}", format_name(m.format));
        }
        s.push(' ');
    }
    let _ = write!(s, "({} modes, current {current})", modes.len());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_bgr_and_masks() {
        assert_eq!(from_uefi(0, [1, 2, 3, 4]), (PixelFormat::Rgbx, [0; 4]));
        assert_eq!(from_uefi(1, [0; 4]), (PixelFormat::Bgrx, [0; 4]));
        assert_eq!(from_uefi(2, [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000]).0, PixelFormat::Rgbx);
        assert_eq!(from_uefi(2, [0xFF_0000, 0xFF00, 0xFF, 0]).0, PixelFormat::Bgrx);
        let m565 = [0xF800, 0x07E0, 0x001F, 0];
        assert_eq!(from_uefi(2, m565), (PixelFormat::Bitmask, m565));
        assert_eq!(from_uefi(2, [0, 0, 0, 0]).0, PixelFormat::None);
        assert_eq!(from_uefi(3, [0; 4]).0, PixelFormat::None);
        assert_eq!(from_uefi(77, [0; 4]).0, PixelFormat::None);
    }

    #[test]
    fn bpp() {
        assert_eq!(bytes_per_pixel(PixelFormat::Bgrx, [0; 4]), 4);
        assert_eq!(bytes_per_pixel(PixelFormat::Bitmask, [0xF800, 0x07E0, 0x001F, 0]), 2);
        assert_eq!(bytes_per_pixel(PixelFormat::Bitmask, [0xFF_0000, 0xFF00, 0xFF, 0]), 3);
        assert_eq!(bytes_per_pixel(PixelFormat::None, [0; 4]), 0);
    }

    #[test]
    fn framebuffer_checks() {
        // The Mini-PC's 1440p scanout (HW-MINIPC round 6).
        assert!(check_framebuffer(0x7C_0000_0000, 2560 * 1440 * 4, 2560, 1440, 2560, 4).is_ok());
        assert!(check_framebuffer(0, 100, 1, 1, 1, 4).is_err());
        assert!(check_framebuffer(0x8000_0000, 1280 * 800 * 4 - 1, 1280, 800, 1280, 4).is_err());
        assert!(check_framebuffer(0x8000_0000, u64::MAX, 1280, 800, 1000, 4).is_err());
        assert!(check_framebuffer(u64::MAX - 10, 1 << 20, 16, 16, 16, 4).is_err());
    }

    #[test]
    fn a_framebuffer_far_above_512_gib_is_fine_and_one_past_the_cpu_is_not() {
        let picture = 2560 * 1440 * 4;
        // The Gaming-PC: 46 address bits, the picture at 640 GiB (14.11.2026) or 672 GiB (5.11.2026).
        assert!(check_reachable(0xa0_0000_0000, picture, 46).is_ok());
        assert!(check_reachable(0xa8_0000_0000, picture, 46).is_ok());
        // The Mini-PC (48 bits, picture below 512 GiB) and QEMU, as before.
        assert!(check_reachable(0x7C_0000_0000, picture, 48).is_ok());
        assert!(check_reachable(0x8000_0000, 1280 * 800 * 4, 40).is_ok());
        // Made up: the same address on a CPU with 39 bits (512 GiB), and addresses past 46 bits.
        assert_eq!(check_reachable(0xa0_0000_0000, picture, 39), Err("the framebuffer lies above what this CPU can address"));
        assert!(check_reachable(1 << 46, picture, 46).is_err());
        assert!(check_reachable((1 << 46) - 4096, 4097, 46).is_err(), "the last page straddles the limit");
        assert!(check_reachable((1 << 46) - 4096, 4096, 46).is_ok());
        assert!(check_reachable(1 << 50, picture, 46).is_err(), "inside 52 bits, past this CPU's");
        // No usable CPUID answer: x86's 52 bits.
        assert!(check_reachable(1 << 50, picture, 0).is_ok());
        assert!(check_reachable(1 << 52, picture, 0).is_err());
        assert!(check_reachable(1 << 52, picture, 64).is_err());
        assert!(check_reachable(u64::MAX - 10, 1 << 20, 46).is_err(), "wraps");
    }

    #[test]
    fn framebuffer_inside_ram_is_seen() {
        let ram = [(0x1000, 0x9_F000), (0x10_0000, 0x8000_0000)];
        assert!(!overlaps_ram(0x7C_0000_0000, 64 << 20, ram));
        assert!(!overlaps_ram(0x8000_0000, 16 << 20, ram), "right after RAM");
        assert!(overlaps_ram(0x7FFF_F000, 0x2000, ram), "last RAM page");
        assert!(overlaps_ram(0x4000_0000, 4096, ram), "inside RAM");
        assert!(overlaps_ram(0, u64::MAX, ram), "no overflow");
        assert!(!overlaps_ram(0x9_F000, 0x6_1000, ram), "the hole below 1 MiB");
    }

    #[test]
    fn summary_line() {
        let modes = [
            Mode { index: 0, width: 1280, height: 800, format: PixelFormat::Bgrx, stride: 1280 },
            Mode { index: 1, width: 2560, height: 1440, format: PixelFormat::Rgbx, stride: 2560 },
        ];
        assert_eq!(mode_summary(&modes, 0), "0:1280x800 1:2560x1440/RGB (2 modes, current 0)");
    }
}
