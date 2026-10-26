//! Scanout pixel layouts GOP reports (UEFI 2.10 §12.9: PixelRedGreenBlueReserved8BitPerColor,
//! PixelBlueGreenRedReserved8BitPerColor, PixelBitMask) and the conversion from the RAM frame's 0x00RRGGBB.
//! The only conversion point (spec/gpu.md §4): everything above draws 0x00RRGGBB.
//!
//! Bit masks: each channel is one contiguous run of bits, found as Linux does (drivers/firmware/efi/libstub/gop.c
//! `find_bits`). The pixel size is the byte count up to the highest bit of all four masks (reserved included),
//! as EDK2's FrameBufferBltLib (the firmware's own Blt) derives it, so 16-bit 565 and 24-bit layouts work too.
//! Linux's libstub instead sums the four channel sizes (`lfb_depth`). Both agree for every dense layout (565,
//! 24-bit, 10-10-10-2, the 8888 orders); they differ only for a sparse set such as 8-8-8 at bits 8..31 with no
//! reserved mask, where libstub would assume 3 bytes and draw a skewed picture. We follow the firmware: the
//! highest bit is where its own Blt writes, so such a layout is 4 bytes with an unused low byte.
//! Channels wider than 8 bits get the 8-bit value with its top bits repeated (0xFF stays full scale), narrower
//! ones keep its top bits.

use bootinfo::{Framebuffer, PixelFormat as BiFormat};

/// One colour channel: `bits` bits starting at bit `shift` of the pixel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chan {
    pub shift: u8,
    pub bits: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Blue in byte 0: the RAM frame's own layout, copied as it is (the Mini-PC's GOP).
    Bgrx,
    /// Red in byte 0.
    Rgbx,
    /// Anything else GOP describes by masks; `bytes` per pixel (1..=4).
    Mask { r: Chan, g: Chan, b: Chan, bytes: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatError {
    /// GOP has no linear framebuffer (Blt only): the kernel runs headless.
    NoFramebuffer,
    /// A colour mask is zero.
    EmptyMask(char),
    /// A mask's bits are not one contiguous run.
    NotContiguous(char),
    /// A channel is wider than 16 bits.
    TooWide(char),
    /// Two colour masks share bits.
    Overlap,
    /// Width, height, stride and size do not describe a framebuffer that fits.
    Geometry,
}

fn chan(mask: u32, name: char) -> Result<Chan, FormatError> {
    if mask == 0 {
        return Err(FormatError::EmptyMask(name));
    }
    let shift = mask.trailing_zeros();
    let bits = mask.count_ones();
    if (mask >> shift) != u32::MAX >> (32 - bits) {
        return Err(FormatError::NotContiguous(name));
    }
    if bits > 16 {
        return Err(FormatError::TooWide(name));
    }
    Ok(Chan { shift: shift as u8, bits: bits as u8 })
}

impl Layout {
    /// From the four GOP masks (red, green, blue, reserved). The two 8-bit byte orders become `Bgrx`/`Rgbx`.
    pub fn from_masks(m: [u32; 4]) -> Result<Layout, FormatError> {
        let (r, g, b) = (chan(m[0], 'r')?, chan(m[1], 'g')?, chan(m[2], 'b')?);
        if m[0] & m[1] != 0 || m[0] & m[2] != 0 || m[1] & m[2] != 0 {
            return Err(FormatError::Overlap);
        }
        let all = m[0] | m[1] | m[2] | m[3];
        let bytes = ((32 - all.leading_zeros()) as u8).div_ceil(8);
        let c = |shift, bits| Chan { shift, bits };
        Ok(match (r, g, b, bytes) {
            (r, g, b, 4) if r == c(16, 8) && g == c(8, 8) && b == c(0, 8) => Layout::Bgrx,
            (r, g, b, 4) if r == c(0, 8) && g == c(8, 8) && b == c(16, 8) => Layout::Rgbx,
            _ => Layout::Mask { r, g, b, bytes },
        })
    }

    pub fn from_bootinfo(fb: &Framebuffer) -> Result<Layout, FormatError> {
        match fb.format {
            BiFormat::Bgrx => Ok(Layout::Bgrx),
            BiFormat::Rgbx => Ok(Layout::Rgbx),
            BiFormat::Bitmask => Layout::from_masks(fb.masks),
            BiFormat::None => Err(FormatError::NoFramebuffer),
        }
    }

    pub fn bytes_per_pixel(&self) -> usize {
        match self {
            Layout::Bgrx | Layout::Rgbx => 4,
            Layout::Mask { bytes, .. } => *bytes as usize,
        }
    }

    /// The (red, green, blue, reserved) masks this layout writes (reserved = the unused bits of the pixel).
    pub fn masks(&self) -> [u32; 4] {
        let m = |c: Chan| (u32::MAX >> (32 - c.bits as u32)) << c.shift;
        let (r, g, b) = match *self {
            Layout::Bgrx => (0xFF_0000, 0xFF00, 0xFF),
            Layout::Rgbx => (0xFF, 0xFF00, 0xFF_0000),
            Layout::Mask { r, g, b, .. } => (m(r), m(g), m(b)),
        };
        let all = if self.bytes_per_pixel() == 4 { u32::MAX } else { (1u32 << (8 * self.bytes_per_pixel())) - 1 };
        [r, g, b, all & !(r | g | b)]
    }

    /// Short name for events.
    pub fn name(&self) -> &'static str {
        match self {
            Layout::Bgrx => "bgrx",
            Layout::Rgbx => "rgbx",
            Layout::Mask { .. } => "bitmask",
        }
    }

    /// The scanout value of one 0x00RRGGBB pixel (the low `bytes_per_pixel` bytes are written, little-endian).
    #[inline]
    pub fn encode(&self, p: u32) -> u32 {
        match *self {
            Layout::Bgrx => p & 0x00FF_FFFF,
            Layout::Rgbx => ((p & 0xFF) << 16) | (p & 0xFF00) | ((p >> 16) & 0xFF),
            Layout::Mask { r, g, b, .. } => put(p >> 16, r) | put(p >> 8, g) | put(p, b),
        }
    }
}

/// The low 8 bits of `v` scaled to the channel and moved into place.
#[inline]
fn put(v: u32, c: Chan) -> u32 {
    let v = v & 0xFF;
    let bits = c.bits as u32;
    let scaled = if bits >= 8 { (v << (bits - 8)) | (v >> (16 - bits)) } else { v >> (8 - bits) };
    scaled << c.shift
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_orders_normalise() {
        assert_eq!(Layout::from_masks([0xFF_0000, 0xFF00, 0xFF, 0xFF00_0000]), Ok(Layout::Bgrx));
        assert_eq!(Layout::from_masks([0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000]), Ok(Layout::Rgbx));
        assert_eq!(Layout::Bgrx.encode(0xAB12_3456), 0x12_3456);
        assert_eq!(Layout::Rgbx.encode(0x0012_3456), 0x56_3412);
        assert_eq!(Layout::Rgbx.masks(), [0xFF, 0xFF00, 0xFF_0000, 0xFF00_0000]);
    }

    #[test]
    fn masks_of_other_sizes() {
        let l = Layout::from_masks([0xF800, 0x07E0, 0x001F, 0]).unwrap();
        assert_eq!(l.bytes_per_pixel(), 2);
        assert_eq!(l.encode(0x00FF_FFFF), 0xFFFF);
        assert_eq!(l.encode(0x0080_4020), (0x10 << 11) | (0x10 << 5) | 0x04);
        assert_eq!(l.masks(), [0xF800, 0x07E0, 0x001F, 0]);
        let l24 = Layout::from_masks([0xFF_0000, 0xFF00, 0xFF, 0]).unwrap();
        assert_eq!(l24.bytes_per_pixel(), 3, "no reserved byte: 24-bit pixels");
        let l10 = Layout::from_masks([0x3FF0_0000, 0xF_FC00, 0x3FF, 0xC000_0000]).unwrap();
        assert_eq!(l10.bytes_per_pixel(), 4);
        assert_eq!(l10.encode(0x00FF_0000), 0x3FF0_0000, "full scale stays full scale");
        assert_eq!(l10.encode(0x0000_0080), 0x202, "0x80 -> 0b1000000010");
        let perm = Layout::from_masks([0xFF, 0xFF_0000, 0xFF00, 0xFF00_0000]).unwrap();
        assert_eq!(perm.encode(0x0011_2233), 0x22_3311);
        assert_eq!(perm.masks(), [0xFF, 0xFF_0000, 0xFF00, 0xFF00_0000]);
    }

    #[test]
    fn sparse_masks_use_the_highest_bit() {
        // 8-8-8 at bits 8..31, no reserved mask: 4 bytes (libstub's channel sum would say 3).
        let l = Layout::from_masks([0xFF00_0000, 0xFF_0000, 0xFF00, 0]).unwrap();
        assert_eq!(l.bytes_per_pixel(), 4);
        assert_eq!(l.encode(0x0011_2233), 0x1122_3300, "the low byte stays unused");
        assert_eq!(l.masks(), [0xFF00_0000, 0xFF_0000, 0xFF00, 0xFF]);
        // 5-5-5 at bits 1..15 (a gap at bit 0): 2 bytes.
        let l = Layout::from_masks([0xF800, 0x07C0, 0x003E, 0]).unwrap();
        assert_eq!(l.bytes_per_pixel(), 2);
        assert_eq!(l.encode(0x00FF_FFFF), 0xFFFE);
        // The stride check in GopFb::new uses the same size: 4-byte sparse pixels need 4 bytes per pixel.
        let t = alloc::sync::Arc::new(crate::RamTarget::new(4 * 8 * 2 - 1, 0));
        let sparse = Layout::from_masks([0xFF00_0000, 0xFF_0000, 0xFF00, 0]).unwrap();
        assert!(crate::GopFb::new(t, 8, 2, 8, sparse, "test").is_err(), "one byte short of 2 rows of 8 four-byte pixels");
    }

    #[test]
    fn bad_masks_refused() {
        assert_eq!(Layout::from_masks([0, 0xFF00, 0xFF, 0]), Err(FormatError::EmptyMask('r')));
        assert_eq!(Layout::from_masks([0xF0F0_0000, 0xFF00, 0xFF, 0]), Err(FormatError::NotContiguous('r')));
        assert_eq!(Layout::from_masks([0x1_FFFF << 8, 0xFF00_0000, 0xFF, 0]), Err(FormatError::TooWide('r')));
        assert_eq!(Layout::from_masks([0xFF_0000, 0xFF_FF00, 0xFF, 0]), Err(FormatError::Overlap));
        assert_eq!(Layout::from_masks([0x8000_0000, 0x4000_0000, 0x1, 0]).unwrap().bytes_per_pixel(), 4);
    }
}
