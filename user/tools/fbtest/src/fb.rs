//! /dev/fb0 through its ioctls (linux-6.6/include/uapi/linux/fb.h) and a shared mapping, plus `fbtest check`.

use usys::dev::{SharedMap, ioctl_buf};
use usys::{Errno, PROT_READ, PROT_WRITE};

const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CLOEXEC: usize = 0o2000000;
const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOPUT_VSCREENINFO: u32 = 0x4601;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
const FBIOPAN_DISPLAY: u32 = 0x4606;
const FBIOBLANK: u32 = 0x4611;
const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
/// _IOW('F', 0x20, __u32).
const FBIO_WAITFORVSYNC: u32 = 0x4004_4620;

/// abi-fbdev's pattern (user/abi-tests/c/fbdev.c `pat`), which the dp.fbdev.picture ktest computes too.
pub fn pat(seed: u32, x: u32, y: u32) -> u32 {
    ((x.wrapping_mul(3).wrapping_add(seed.wrapping_mul(7)) & 0xff) << 16)
        | ((y.wrapping_mul(5).wrapping_add(seed.wrapping_mul(11)) & 0xff) << 8)
        | ((x ^ y ^ seed) & 0xff)
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// struct fb_var_screeninfo (160 bytes).
#[derive(Clone)]
pub struct Var(pub [u8; 160]);

impl Var {
    pub fn u32(&self, o: usize) -> u32 {
        u32_at(&self.0, o)
    }
    pub fn set(&mut self, o: usize, v: u32) {
        self.0[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    pub fn xres(&self) -> u32 {
        self.u32(0)
    }
    pub fn yres(&self) -> u32 {
        self.u32(4)
    }
    pub fn xres_virtual(&self) -> u32 {
        self.u32(8)
    }
    pub fn yres_virtual(&self) -> u32 {
        self.u32(12)
    }
    pub fn xoffset(&self) -> u32 {
        self.u32(16)
    }
    pub fn yoffset(&self) -> u32 {
        self.u32(20)
    }
    pub fn bpp(&self) -> u32 {
        self.u32(24)
    }
}

/// struct fb_fix_screeninfo (80 bytes on x86-64).
pub struct Fix(pub [u8; 80]);

impl Fix {
    pub fn id(&self) -> String {
        let end = self.0[..16].iter().position(|&c| c == 0).unwrap_or(16);
        String::from_utf8_lossy(&self.0[..end]).into_owned()
    }
    pub fn smem_len(&self) -> u32 {
        u32_at(&self.0, 24)
    }
    pub fn type_(&self) -> u32 {
        u32_at(&self.0, 28)
    }
    pub fn visual(&self) -> u32 {
        u32_at(&self.0, 36)
    }
    pub fn xpanstep(&self) -> u16 {
        u16::from_le_bytes([self.0[40], self.0[41]])
    }
    pub fn ypanstep(&self) -> u16 {
        u16::from_le_bytes([self.0[42], self.0[43]])
    }
    pub fn ywrapstep(&self) -> u16 {
        u16::from_le_bytes([self.0[44], self.0[45]])
    }
    pub fn line_length(&self) -> u32 {
        u32_at(&self.0, 48)
    }
}

pub struct Fb {
    pub fd: usize,
    pub var: Var,
    pub fix: Fix,
}

impl Drop for Fb {
    fn drop(&mut self) {
        let _ = usys::close(self.fd);
    }
}

impl Fb {
    pub fn open(dev: &str, write: bool) -> Result<Fb, Errno> {
        let fd = usys::open(dev, if write { O_RDWR } else { O_RDONLY } | O_CLOEXEC, 0)?;
        let mut f = Fb { fd, var: Var([0; 160]), fix: Fix([0; 80]) };
        f.get_var()?;
        ioctl_buf(fd, FBIOGET_FSCREENINFO, &mut f.fix.0)?;
        Ok(f)
    }
    pub fn get_var(&mut self) -> Result<(), Errno> {
        ioctl_buf(self.fd, FBIOGET_VSCREENINFO, &mut self.var.0).map(|_| ())
    }
    /// FBIOPUT_VSCREENINFO, then the fix struct again: Linux recomputes line_length and smem_len for a new
    /// mode (fbmem.c `fb_set_var` -> `fb_set_fix`), and `fill`/`pwrite_rows`/`map` use them.
    pub fn put_var(&mut self, mut v: Var) -> Result<(), Errno> {
        ioctl_buf(self.fd, FBIOPUT_VSCREENINFO, &mut v.0)?;
        self.var = v;
        ioctl_buf(self.fd, FBIOGET_FSCREENINFO, &mut self.fix.0)?;
        Ok(())
    }
    /// fbDOOM's dance: ask for a double-height virtual screen; true when the buffer holds two screens.
    pub fn ask_double(&mut self) -> bool {
        let mut v = self.var.clone();
        v.set(12, self.var.yres() * 2);
        v.set(20, 0);
        let _ = self.put_var(v);
        let _ = self.get_var();
        self.var.yres_virtual() >= 2 * self.var.yres()
    }
    pub fn pan_to(&mut self, xoffset: u32, yoffset: u32) -> Result<(), Errno> {
        let mut v = self.var.clone();
        v.set(16, xoffset);
        v.set(20, yoffset);
        ioctl_buf(self.fd, FBIOPAN_DISPLAY, &mut v.0)?;
        self.var.set(16, xoffset);
        self.var.set(20, yoffset);
        Ok(())
    }
    pub fn pan(&mut self, yoffset: u32) -> Result<(), Errno> {
        self.pan_to(0, yoffset)
    }
    pub fn vsync(&self) -> Result<(), Errno> {
        let mut crtc = [0u8; 4];
        ioctl_buf(self.fd, FBIO_WAITFORVSYNC, &mut crtc).map(|_| ())
    }
    pub fn map(&self) -> Result<SharedMap, Errno> {
        SharedMap::new(self.fd, self.fix.smem_len() as usize, PROT_READ | PROT_WRITE, 0)
    }
    /// One screen (yres rows) from buffer row `top`, pixel (x, y) = f(x, y) with y counted from `top`.
    pub fn fill(&self, m: &SharedMap, top: u32, f: impl Fn(u32, u32) -> u32) {
        let (w, ll) = (self.var.xres(), self.fix.line_length() as usize);
        let mut row = vec![0u32; w as usize];
        for y in 0..self.var.yres() {
            for (x, p) in row.iter_mut().enumerate() {
                *p = f(x as u32, y);
            }
            m.write_u32s((top + y) as usize * ll, &row);
        }
    }
    /// `n` rows written with pwrite at buffer row `at`; pixel (x, y) = f(x, y), y counted from `at`.
    pub fn pwrite_rows(&self, at: u32, n: u32, f: impl Fn(u32, u32) -> u32) -> Result<(), Errno> {
        let ll = self.fix.line_length() as usize;
        let mut b = vec![0u8; n as usize * ll];
        for y in 0..n {
            for x in 0..self.var.xres() {
                let o = y as usize * ll + x as usize * 4;
                b[o..o + 4].copy_from_slice(&f(x, y).to_le_bytes());
            }
        }
        let w = usys::pwrite64(self.fd, &b, (at as usize * ll) as i64)?;
        if w == b.len() { Ok(()) } else { Err(Errno(5)) }
    }
}

struct Checks(u32, u32);

impl Checks {
    fn line(&mut self, what: &str, r: Result<(), String>) {
        match r {
            Ok(()) => {
                self.0 += 1;
                println!("ok   {what}");
            }
            Err(e) => {
                self.1 += 1;
                println!("FAIL {what}: {e}");
            }
        }
    }
}

fn is<T: PartialEq + std::fmt::Debug>(got: T, want: T) -> Result<(), String> {
    if got == want { Ok(()) } else { Err(format!("{got:?}, want {want:?}")) }
}

/// `fbtest check`: what a compositor or an SDL program relies on. Returns the exit code.
pub fn check(dev: &str) -> i32 {
    let mut f = match Fb::open(dev, true) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("fbtest: {dev}: {}", e.name());
            return if e.0 == 25 { 4 } else { 1 };
        }
    };
    let mut c = Checks(0, 0);
    let (w, h, ll, smem) = (f.var.xres(), f.var.yres(), f.fix.line_length(), f.fix.smem_len());
    let v = f.var.clone();
    c.line("XRGB8888 (bpp 32, red 16/8, green 8/8, blue 0/8)", is((v.bpp(), v.u32(32), v.u32(36), v.u32(44), v.u32(48), v.u32(56), v.u32(60)), (32, 16, 8, 8, 8, 0, 8)));
    c.line("line_length >= xres * 4", if ll >= w * 4 { Ok(()) } else { Err(format!("{ll} < {}", w * 4)) });
    c.line("type FB_TYPE_PACKED_PIXELS, visual FB_VISUAL_TRUECOLOR", is((f.fix.type_(), f.fix.visual()), (FB_TYPE_PACKED_PIXELS, FB_VISUAL_TRUECOLOR)));
    let dbl = f.ask_double();
    c.line("double-height virtual screen granted", is(dbl, true));
    let (yv, xv) = (f.var.yres_virtual(), f.var.xres_virtual());
    c.line("virtual screen at least the visible one", if yv >= h && xv >= w { Ok(()) } else { Err(format!("{xv}x{yv} < {w}x{h}")) });
    // A driver reporting less would wrap the pan offsets below; saturate and let the lines above say FAIL.
    let (ypan, xpan) = (yv.saturating_sub(h), xv.saturating_sub(w));
    c.line("smem_len covers yres_virtual rows", if smem as u64 >= ll as u64 * yv as u64 { Ok(()) } else { Err(format!("{smem} < {ll} * {yv}")) });
    c.line("PUT_VSCREENINFO of the current var: 0, unchanged", {
        let before = f.var.clone();
        f.put_var(before.clone()).map_err(|e| e.name()).and_then(|_| {
            f.get_var().map_err(|e| e.name())?;
            is((f.var.xres(), f.var.yres(), f.var.yres_virtual()), (before.xres(), before.yres(), before.yres_virtual()))
        })
    });
    c.line("pan to the second screen, GET shows it", f.pan(ypan).map_err(|e| e.name()).and_then(|_| {
        f.get_var().map_err(|e| e.name())?;
        is(f.var.yoffset(), ypan)
    }));
    c.line("pan back to 0", f.pan(0).map_err(|e| e.name()).and_then(|_| {
        f.get_var().map_err(|e| e.name())?;
        is(f.var.yoffset(), 0)
    }));
    c.line("pan past yres_virtual - yres: EINVAL", is(f.pan(ypan + 1).err(), Some(usys::EINVAL)));
    c.line("pan past xres_virtual - xres: EINVAL", is(f.pan_to(xpan + 1, 0).err(), Some(usys::EINVAL)));
    c.line("FBIO_WAITFORVSYNC: 0", f.vsync().map_err(|e| e.name()));
    // FB_BLANK_UNBLANK: SDL and fbdev compositors unblank at start (fbmem.c FBIOBLANK -> fb_blank).
    c.line("FBIOBLANK(FB_BLANK_UNBLANK): 0", usys::dev::ioctl_val(f.fd, FBIOBLANK, 0).map(|_| ()).map_err(|e| e.name()));
    match f.map() {
        Ok(m) => {
            c.line("mmap smem_len MAP_SHARED", Ok(()));
            f.fill(&m, 0, |x, y| pat(77, x, y));
            c.line("mmap stores read back by pread", {
                let mut b = vec![0u8; ll as usize * 2];
                usys::pread64(f.fd, &mut b, ll as i64 * (h as i64 / 2)).map_err(|e| e.name()).and_then(|n| {
                    is(n, b.len())?;
                    let y0 = h / 2;
                    (0..2u32)
                        .flat_map(|y| (0..w).map(move |x| (x, y)))
                        .find(|&(x, y)| u32_at(&b, (y * ll + x * 4) as usize) != pat(77, x, y0 + y))
                        .map_or(Ok(()), |(x, y)| Err(format!("pixel ({x}, {}) differs", y0 + y)))
                })
            });
            c.line("pwrite rows read back through the mapping", f.pwrite_rows(h / 3, 2, |x, y| pat(78, x, y)).map_err(|e| e.name()).and_then(|_| {
                let mut b = vec![0u8; ll as usize * 2];
                m.read_at((h / 3 * ll) as usize, &mut b);
                (0..2u32)
                    .flat_map(|y| (0..w).map(move |x| (x, y)))
                    .find(|&(x, y)| u32_at(&b, (y * ll + x * 4) as usize) != pat(78, x, y))
                    .map_or(Ok(()), |(x, y)| Err(format!("pixel ({x}, {y}) differs")))
            }));
        }
        Err(e) => c.line("mmap smem_len MAP_SHARED", Err(e.name())),
    }
    let mut one = [0u8; 4];
    c.line("pread at smem_len: 0 bytes", is(usys::pread64(f.fd, &mut one, smem as i64), Ok(0)));
    c.line("pwrite at smem_len: ENOSPC", is(usys::pwrite64(f.fd, &one, smem as i64), Err(usys::Errno(28))));
    println!("check: {} ok, {} failed", c.0, c.1);
    if c.1 == 0 { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_matches_abi_fbdev() {
        // (x*3 + seed*7) & 0xff << 16 | (y*5 + seed*11) & 0xff << 8 | (x^y^seed) & 0xff
        assert_eq!(pat(5, 10, 20), (65 << 16) | (155 << 8) | (10 ^ 20 ^ 5));
        assert_eq!(pat(0, 0, 0), 0);
    }
}
