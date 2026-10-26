//! The fbdev ABI as data and its rules, without any kernel object (host-tested): `struct fb_var_screeninfo`,
//! `struct fb_fix_screeninfo`, the colour map, and what each ioctl accepts and answers.
//!
//! Model: programs must see what they see on Linux when its DRM fbdev emulation runs on a firmware framebuffer
//! (simpledrm): a shadow buffer in RAM with deferred io, one fixed mode, no fbcon. The answers (which input gets
//! which errno) follow that; the code is BitsOS's own. The one BitsOS rule on top (DP-2 brief): only the native
//! mode is accepted (`xres`/`yres` must equal the screen), where DRM would accept a smaller one and ignore it.

use uapi::errno::{E2BIG, EFAULT, EINVAL, ENOMEM};

pub const FBIOGET_VSCREENINFO: u64 = 0x4600;
pub const FBIOPUT_VSCREENINFO: u64 = 0x4601;
pub const FBIOGET_FSCREENINFO: u64 = 0x4602;
pub const FBIOGETCMAP: u64 = 0x4604;
pub const FBIOPUTCMAP: u64 = 0x4605;
pub const FBIOPAN_DISPLAY: u64 = 0x4606;
/// `_IOWR('F', 0x08, struct fb_cursor)` (struct fb_cursor is 104 bytes on x86-64).
pub const FBIO_CURSOR: u64 = 0xC068_4608;
pub const FBIOGET_CON2FBMAP: u64 = 0x460F;
pub const FBIOPUT_CON2FBMAP: u64 = 0x4610;
pub const FBIOBLANK: u64 = 0x4611;
/// `_IOW('F', 0x20, __u32)`.
pub const FBIO_WAITFORVSYNC: u64 = 0x4004_4620;

pub const FB_MAJOR: u32 = 29;
pub const FB_TYPE_PACKED_PIXELS: u32 = 0;
pub const FB_VISUAL_TRUECOLOR: u32 = 2;
pub const FB_ACCEL_NONE: u32 = 0;
pub const FB_BLANK_POWERDOWN: u64 = 4;

pub const FB_ACTIVATE_NOW: u32 = 0;
pub const FB_ACTIVATE_MASK: u32 = 15;
pub const FB_ACTIVATE_FORCE: u32 = 128;
pub const FB_ACTIVATE_INV_MODE: u32 = 256;
pub const FB_ACTIVATE_KD_TEXT: u32 = 512;
pub const FB_VMODE_MASK: u32 = 255;
pub const FB_VMODE_YWRAP: u32 = 256;

/// `struct fb_var_screeninfo`: 40 `__u32`s (160 bytes); the indices below name them.
pub const VAR_SIZE: usize = 160;
/// `struct fb_fix_screeninfo` on x86-64 (80 bytes with the tail padding).
pub const FIX_SIZE: usize = 80;
/// `struct fb_cmap_user` (start, len, four pointers).
pub const CMAP_USER_SIZE: usize = 40;

pub const XRES: usize = 0;
pub const YRES: usize = 1;
pub const XRES_VIRTUAL: usize = 2;
pub const YRES_VIRTUAL: usize = 3;
pub const XOFFSET: usize = 4;
pub const YOFFSET: usize = 5;
pub const BITS_PER_PIXEL: usize = 6;
pub const GRAYSCALE: usize = 7;
/// Bit fields: offset, length, msb_right each.
pub const RED: usize = 8;
pub const GREEN: usize = 11;
pub const BLUE: usize = 14;
pub const TRANSP: usize = 17;
pub const NONSTD: usize = 20;
pub const ACTIVATE: usize = 21;
pub const HEIGHT: usize = 22;
pub const WIDTH: usize = 23;
pub const ACCEL_FLAGS: usize = 24;
pub const PIXCLOCK: usize = 25;
pub const LEFT_MARGIN: usize = 26;
pub const RIGHT_MARGIN: usize = 27;
pub const UPPER_MARGIN: usize = 28;
pub const LOWER_MARGIN: usize = 29;
pub const HSYNC_LEN: usize = 30;
pub const VSYNC_LEN: usize = 31;
pub const SYNC: usize = 32;
pub const VMODE: usize = 33;
pub const ROTATE: usize = 34;
pub const COLORSPACE: usize = 35;
pub const RESERVED: usize = 36;

/// `struct fb_var_screeninfo` as its 40 words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Var(pub [u32; 40]);

impl Var {
    pub fn from_bytes(b: &[u8; VAR_SIZE]) -> Var {
        let mut w = [0u32; 40];
        for (i, c) in b.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        Var(w)
    }
    pub fn to_bytes(&self) -> [u8; VAR_SIZE] {
        let mut b = [0u8; VAR_SIZE];
        for (c, w) in b.as_chunks_mut::<4>().0.iter_mut().zip(self.0) {
            c.copy_from_slice(&w.to_le_bytes());
        }
        b
    }
    fn set_field(&mut self, at: usize, offset: u32, length: u32) {
        self.0[at] = offset;
        self.0[at + 1] = length;
        self.0[at + 2] = 0;
    }
    /// XRGB8888 as DRM describes it (`drm_fb_helper_fill_pixel_fmt`, depth 24: no transparency).
    fn fill_pixel_fmt(&mut self) {
        self.set_field(RED, 16, 8);
        self.set_field(GREEN, 8, 8);
        self.set_field(BLUE, 0, 8);
        self.set_field(TRANSP, 0, 0);
    }
    /// `drm_fb_pixel_format_equal`.
    fn format_equal(&self, o: &Var) -> bool {
        self.0[BITS_PER_PIXEL] == o.0[BITS_PER_PIXEL] && self.0[GRAYSCALE] == o.0[GRAYSCALE] && self.0[RED..NONSTD] == o.0[RED..NONSTD]
    }
    /// The fields `fb_var_to_videomode` + `fb_mode_is_equal` compare (modedb.c).
    fn mode_equal(&self, o: &Var) -> bool {
        let f = |v: &Var| {
            [
                v.0[XRES],
                v.0[YRES],
                v.0[PIXCLOCK],
                v.0[HSYNC_LEN],
                v.0[VSYNC_LEN],
                v.0[LEFT_MARGIN],
                v.0[RIGHT_MARGIN],
                v.0[UPPER_MARGIN],
                v.0[LOWER_MARGIN],
                v.0[SYNC],
                v.0[VMODE] & FB_VMODE_MASK,
            ]
        };
        f(self) == f(o)
    }
}

/// The fixed facts of the one mode: the screen, the buffer rows (`yres_virtual`), the memory size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Rows of the buffer: `height`, or twice it when memory allowed double buffering.
    pub rows: u32,
    /// Bytes of the buffer (page-aligned, like DRM's dumb buffer).
    pub smem_len: u32,
}

impl Geometry {
    pub const BPP: u32 = 32;
    pub fn line_length(&self) -> u32 {
        self.width * 4
    }
    /// The var after boot (`drm_fb_helper_fill_var`): offsets 0, virtual = the whole buffer, XRGB8888,
    /// activate NOW, timing fields 0 (DRM's), the picture size in mm unknown (-1, efifb's value: GOP has no EDID).
    pub fn initial_var(&self) -> Var {
        let mut v = Var([0; 40]);
        v.0[XRES] = self.width;
        v.0[YRES] = self.height;
        v.0[XRES_VIRTUAL] = self.width;
        v.0[YRES_VIRTUAL] = self.rows;
        v.0[BITS_PER_PIXEL] = Self::BPP;
        v.fill_pixel_fmt();
        v.0[ACTIVATE] = FB_ACTIVATE_NOW;
        v.0[HEIGHT] = u32::MAX;
        v.0[WIDTH] = u32::MAX;
        v
    }
    /// `struct fb_fix_screeninfo` (`drm_fb_helper_fill_fix`). `smem_start` is 0: the buffer is kernel RAM whose
    /// physical address no program needs (Linux's FBINFO_HIDE_SMEM_START does the same).
    pub fn fix_bytes(&self) -> [u8; FIX_SIZE] {
        let mut b = [0u8; FIX_SIZE];
        let id = b"BitsOS gopfb";
        b[..id.len()].copy_from_slice(id);
        // 16: smem_start (unsigned long) = 0
        b[24..28].copy_from_slice(&self.smem_len.to_le_bytes());
        b[28..32].copy_from_slice(&FB_TYPE_PACKED_PIXELS.to_le_bytes());
        // 32: type_aux = 0
        b[36..40].copy_from_slice(&FB_VISUAL_TRUECOLOR.to_le_bytes());
        b[40..42].copy_from_slice(&1u16.to_le_bytes()); // xpanstep
        b[42..44].copy_from_slice(&1u16.to_le_bytes()); // ypanstep
        // 44: ywrapstep = 0; 46: padding
        b[48..52].copy_from_slice(&self.line_length().to_le_bytes());
        // 56: mmio_start, 64: mmio_len, 68: accel (FB_ACCEL_NONE), 72: capabilities, 74: reserved[2] = 0
        b[68..72].copy_from_slice(&FB_ACCEL_NONE.to_le_bytes());
        b
    }
}

/// What FBIOPUT_VSCREENINFO did with a var it accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// Nothing to do (equal var without FB_ACTIVATE_FORCE, or FB_ACTIVATE_INV_MODE of another mode).
    Unchanged,
    /// Checked and adjusted but not applied (FB_ACTIVATE_TEST and the other non-NOW activations).
    Tested,
    /// The caller stores the var and pans to its offsets.
    Applied,
}

/// Turns the var a program proposes into the var the one mode really has, or refuses it (always EINVAL).
/// A program may choose the panning offsets and the activation; the size must be the native one, the pixel
/// format must be the one there is (all zero means "whatever you have", which SDL 1.2 sends), and everything else
/// (virtual size, depth, timings, the picture size in mm) is the device's. Programs see the same answers as from
/// Linux's DRM fbdev emulation, except that a smaller mode is refused instead of silently ignored.
pub fn check_var(var: &mut Var, cur: &Var, g: &Geometry) -> Result<(), i64> {
    let asked = &var.0;
    let native = asked[XRES] == g.width && asked[YRES] == g.height;
    let fits = asked[BITS_PER_PIXEL] <= Geometry::BPP && asked[XRES_VIRTUAL] <= g.width && asked[YRES_VIRTUAL] <= g.rows;
    let window_inside = asked[XOFFSET] == 0 && asked[YOFFSET] <= g.rows - g.height;
    let plain = asked[GRAYSCALE] == 0 && asked[NONSTD] == 0;
    if !(native && fits && window_inside && plain) {
        return Err(EINVAL);
    }
    let mut out = g.initial_var();
    for at in [XOFFSET, YOFFSET, ACTIVATE] {
        out.0[at] = asked[at];
    }
    // the picture size in mm is whatever the device reported, not what the program claims
    out.0[HEIGHT] = cur.0[HEIGHT];
    out.0[WIDTH] = cur.0[WIDTH];
    if asked[RED..NONSTD].iter().any(|&x| x != 0) {
        out.0[RED..NONSTD].copy_from_slice(&asked[RED..NONSTD]);
    }
    if !out.format_equal(cur) {
        return Err(EINVAL);
    }
    *var = out;
    Ok(())
}

/// FBIOPUT_VSCREENINFO: `var` is adjusted in place (the ioctl copies it back); on `Applied` the caller makes it
/// current and pans to its offsets. Refusals are EINVAL.
pub fn set_var(var: &mut Var, cur: &Var, g: &Geometry) -> Result<SetOutcome, i64> {
    let activate = var.0[ACTIVATE];
    if activate & FB_ACTIVATE_INV_MODE != 0 {
        // Deleting the current mode from the mode list is refused; any other mode is "deleted" (V2 keeps no
        // list, and without fbcon nothing uses it).
        return if var.mode_equal(cur) { Err(EINVAL) } else { Ok(SetOutcome::Unchanged) };
    }
    if activate & FB_ACTIVATE_FORCE == 0 && var == cur {
        return Ok(SetOutcome::Unchanged);
    }
    // below 8 pixels a side is no mode at all, even on a tiny screen
    if var.0[XRES].min(var.0[YRES]) < 8 {
        return Err(EINVAL);
    }
    check_var(var, cur, g)?;
    Ok(if var.0[ACTIVATE] & FB_ACTIVATE_MASK == FB_ACTIVATE_NOW { SetOutcome::Applied } else { SetOutcome::Tested })
}

/// FBIOPAN_DISPLAY: moves the shown window inside the buffer. The device pans by single pixels in both
/// directions and cannot wrap, so a wrapping request with a row offset is refused; the window must stay inside
/// the buffer. On success the offsets and the YWRAP bit go into `cur`.
pub fn pan(var: &Var, cur: &mut Var) -> Result<(), i64> {
    let asked = &var.0;
    let wrap = asked[VMODE] & FB_VMODE_YWRAP != 0;
    let spare_rows = cur.0[YRES_VIRTUAL].saturating_sub(cur.0[YRES]);
    let spare_cols = cur.0[XRES_VIRTUAL].saturating_sub(cur.0[XRES]);
    if (wrap && asked[YOFFSET] != 0) || asked[YOFFSET] > spare_rows || asked[XOFFSET] > spare_cols {
        return Err(EINVAL);
    }
    cur.0[XOFFSET] = asked[XOFFSET];
    cur.0[YOFFSET] = asked[YOFFSET];
    cur.0[VMODE] = (cur.0[VMODE] & !FB_VMODE_YWRAP) | (asked[VMODE] & FB_VMODE_YWRAP);
    Ok(())
}

/// The largest colour map a program may hand in, per channel: Linux's kmalloc ceiling on x86-64 (4 MiB), so a
/// hostile length gets ENOMEM there and here (R1).
pub const CMAP_MAX_BYTES: u64 = 4 << 20;

/// The device's colour map: 256 entries without transparency, the first 16 the text-mode colours below, the
/// rest black.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmap {
    pub red: [u16; 256],
    pub green: [u16; 256],
    pub blue: [u16; 256],
}

/// The 16 CGA/EGA/VGA text colours (8-bit red, green, blue): low intensity 0xaa, the bright half adds 0x55,
/// and colour 6 is brown rather than dark yellow.
const TEXT_COLOURS: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00],
    [0x00, 0x00, 0xaa],
    [0x00, 0xaa, 0x00],
    [0x00, 0xaa, 0xaa],
    [0xaa, 0x00, 0x00],
    [0xaa, 0x00, 0xaa],
    [0xaa, 0x55, 0x00],
    [0xaa, 0xaa, 0xaa],
    [0x55, 0x55, 0x55],
    [0x55, 0x55, 0xff],
    [0x55, 0xff, 0x55],
    [0x55, 0xff, 0xff],
    [0xff, 0x55, 0x55],
    [0xff, 0x55, 0xff],
    [0xff, 0xff, 0x55],
    [0xff, 0xff, 0xff],
];

impl Default for Cmap {
    fn default() -> Self {
        let mut c = Cmap { red: [0; 256], green: [0; 256], blue: [0; 256] };
        for (i, [r, g, b]) in TEXT_COLOURS.into_iter().enumerate() {
            // fbdev channels are 16 bit: 0xaa becomes 0xaaaa
            (c.red[i], c.green[i], c.blue[i]) = (r as u16 * 0x101, g as u16 * 0x101, b as u16 * 0x101);
        }
        c
    }
}

/// `struct fb_cmap_user` as a program hands it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CmapUser {
    pub start: u32,
    pub len: u32,
    pub red: u64,
    pub green: u64,
    pub blue: u64,
    pub transp: u64,
}

impl CmapUser {
    pub fn from_bytes(b: &[u8; CMAP_USER_SIZE]) -> CmapUser {
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let u64_at = |o: usize| {
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[o..o + 8]);
            u64::from_le_bytes(a)
        };
        CmapUser { start: u32_at(0), len: u32_at(4), red: u64_at(8), green: u64_at(16), blue: u64_at(24), transp: u64_at(32) }
    }
}

/// Program memory as the colour-map calls reach it (the caller's address space; a fake one in tests).
pub trait UserIo {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool;
    fn write(&self, addr: u64, data: &[u8]) -> bool;
}

fn words_le(w: &[u16]) -> alloc::vec::Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Reads one channel of `entries` 16-bit values at `ptr` (every byte must be readable, else EFAULT) and keeps
/// its first 16 values, the only ones a truecolour device can use.
fn read_channel(io: &dyn UserIo, ptr: u64, entries: u32) -> Result<[u16; 16], i64> {
    let mut first = [0u16; 16];
    let total = entries as u64 * 2;
    let mut buf = [0u8; 4096];
    let mut pos = 0u64;
    while pos < total {
        let n = (total - pos).min(buf.len() as u64) as usize;
        if !io.read(ptr.checked_add(pos).ok_or(EFAULT)?, &mut buf[..n]) {
            return Err(EFAULT);
        }
        for (k, w) in buf[..n].chunks_exact(2).enumerate() {
            if let Some(slot) = first.get_mut(pos as usize / 2 + k) {
                *slot = u16::from_le_bytes([w[0], w[1]]);
            }
        }
        pos += n as u64;
    }
    Ok(first)
}

impl Cmap {
    /// FBIOGETCMAP: hands out the entries from `to.start` on, as many as both `to.len` and the map allow.
    /// A window that starts past the map or is empty is EINVAL; the device has no transparency to copy.
    pub fn to_user(&self, to: &CmapUser, io: &dyn UserIo) -> Result<(), i64> {
        if to.start >= 256 || to.len == 0 {
            return Err(EINVAL);
        }
        let first = to.start as usize;
        let count = (to.len as usize).min(256 - first);
        for (dst, src) in [(to.red, &self.red), (to.green, &self.green), (to.blue, &self.blue)] {
            if !io.write(dst, &words_le(&src[first..first + count])) {
                return Err(EFAULT);
            }
        }
        Ok(())
    }

    /// FBIOPUTCMAP on a truecolour device: only entries below 16 exist for it. The answers come in the order
    /// Linux gives them, because programs see them: E2BIG for a length whose byte size overflows an int,
    /// EINVAL for length 0, ENOMEM past the per-channel ceiling, EFAULT for an unreadable channel (transparency
    /// included when given), and only then EINVAL for a range outside the 16 entries.
    pub fn set_user(&mut self, c: &CmapUser, io: &dyn UserIo) -> Result<(), i64> {
        if c.len >= 1 << 30 {
            return Err(E2BIG);
        }
        if c.len == 0 {
            return Err(EINVAL);
        }
        if c.len as u64 * 2 > CMAP_MAX_BYTES {
            return Err(ENOMEM);
        }
        let red = read_channel(io, c.red, c.len)?;
        let green = read_channel(io, c.green, c.len)?;
        let blue = read_channel(io, c.blue, c.len)?;
        if c.transp != 0 {
            read_channel(io, c.transp, c.len)?;
        }
        let end = c.start as u64 + c.len as u64;
        if (c.start as i32) < 0 || end > 16 {
            return Err(EINVAL);
        }
        let (s, n) = (c.start as usize, c.len as usize);
        self.red[s..s + n].copy_from_slice(&red[..n]);
        self.green[s..s + n].copy_from_slice(&green[..n]);
        self.blue[s..s + n].copy_from_slice(&blue[..n]);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
