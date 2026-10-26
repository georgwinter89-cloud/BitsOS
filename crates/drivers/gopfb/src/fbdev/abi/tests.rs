//! Host tests of the fbdev rules against what linux-6.6 does (each case names the code it follows).

use super::*;
use alloc::vec::Vec;
use core::cell::RefCell;
use uapi::errno::{E2BIG, EFAULT, EINVAL, ENOMEM};

const G: Geometry = Geometry { width: 2560, height: 1440, rows: 2880, smem_len: 2560 * 4 * 2880 };

fn cur() -> Var {
    G.initial_var()
}

#[test]
fn layout_and_initial_values() {
    let v = cur();
    let b = v.to_bytes();
    assert_eq!(b.len(), 160);
    assert_eq!(Var::from_bytes(&b), v);
    assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 2560);
    assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 2880, "yres_virtual = the whole buffer");
    assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 32);
    // red at byte 32 (offset 16, length 8), transp at 68 (0, 0): XRGB8888 as DRM reports it.
    assert_eq!(&b[32..44], &[16, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(&b[68..80], &[0; 12]);
    assert_eq!(v.0[HEIGHT], u32::MAX);
    let f = G.fix_bytes();
    assert_eq!(f.len(), 80);
    assert_eq!(&f[..12], b"BitsOS gopfb");
    assert_eq!(f[12..16], [0; 4], "id NUL-padded");
    assert_eq!(f[16..24], [0; 8], "smem_start hidden");
    assert_eq!(u32::from_le_bytes(f[24..28].try_into().unwrap()), G.smem_len);
    assert_eq!(u32::from_le_bytes(f[36..40].try_into().unwrap()), FB_VISUAL_TRUECOLOR);
    assert_eq!(f[40..46], [1, 0, 1, 0, 0, 0], "xpanstep 1, ypanstep 1, ywrapstep 0");
    assert_eq!(u32::from_le_bytes(f[48..52].try_into().unwrap()), 10_240);
    assert_eq!(f[52..], [0; 28]);
}

#[test]
fn set_var_follows_fb_set_var() {
    let c = cur();
    // Equal var without FORCE: nothing to do (fbmem.c:789).
    let mut v = c;
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Unchanged));
    // FORCE: checked and applied.
    v.0[ACTIVATE] = FB_ACTIVATE_FORCE;
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Applied));
    // A typical double-buffer request: yres_virtual 2*yres, pixclock set, margins set: accepted, adjusted.
    let mut v = c;
    v.0[YRES_VIRTUAL] = 2880;
    v.0[PIXCLOCK] = 12345;
    v.0[LEFT_MARGIN] = 40;
    v.0[RESERVED] = 9;
    v.0[YOFFSET] = 1440;
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Applied));
    assert_eq!((v.0[PIXCLOCK], v.0[LEFT_MARGIN], v.0[RESERVED], v.0[YOFFSET]), (0, 0, 0, 1440));
    // A smaller virtual size is filled up to the buffer (__fill_var).
    let mut v = c;
    v.0[YRES_VIRTUAL] = 1440;
    v.0[ACTIVATE] = 2; // FB_ACTIVATE_TEST
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Tested));
    assert_eq!(v.0[YRES_VIRTUAL], 2880);
    // Only the native mode.
    for (i, val) in [(XRES, 1920), (YRES, 1080), (XRES, 2561), (YRES_VIRTUAL, 2881), (XRES_VIRTUAL, 4000), (BITS_PER_PIXEL, 64)] {
        let mut v = c;
        v.0[i] = val;
        assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL), "field {i} = {val}");
    }
    // xres < 8 and overflowing products come first (fbmem.c:815-821).
    let mut v = c;
    v.0[XRES] = 4;
    assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL));
    let mut v = c;
    v.0[XRES_VIRTUAL] = 0x10000;
    v.0[YRES_VIRTUAL] = 0x10000;
    assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL));
    // Offsets past the virtual area, grayscale, nonstd.
    for (i, val) in [(YOFFSET, 1441), (XOFFSET, 1), (GRAYSCALE, 1), (NONSTD, 1)] {
        let mut v = c;
        v.0[i] = val;
        assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL), "field {i} = {val}");
    }
    // 16 bpp with a 565 format: bpp is filled to 32, the format then differs (drm_fb_pixel_format_equal).
    let mut v = c;
    v.0[BITS_PER_PIXEL] = 16;
    v.0[RED..NONSTD].copy_from_slice(&[11, 5, 0, 5, 6, 0, 0, 5, 0, 0, 0, 0]);
    assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL));
    // SDL 1.2's all-zero format with bpp 16: filled, accepted as the 32-bit mode.
    let mut v = c;
    v.0[BITS_PER_PIXEL] = 16;
    v.0[RED..NONSTD].fill(0);
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Applied));
    assert_eq!(v.0[BITS_PER_PIXEL], 32);
    assert_eq!(v.0[RED..NONSTD], c.0[RED..NONSTD]);
    // An ARGB request (transp 24/8) is a format change: refused.
    let mut v = c;
    v.0[TRANSP] = 24;
    v.0[TRANSP + 1] = 8;
    assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL));
}

#[test]
fn inv_mode_refuses_only_the_current_mode() {
    let c = cur();
    let mut v = c;
    v.0[ACTIVATE] = FB_ACTIVATE_INV_MODE;
    assert_eq!(set_var(&mut v, &c, &G), Err(EINVAL));
    v.0[XRES] = 640;
    assert_eq!(set_var(&mut v, &c, &G), Ok(SetOutcome::Unchanged));
}

#[test]
fn pan_follows_fb_pan_display() {
    let mut c = cur();
    let mut v = c;
    v.0[YOFFSET] = 1440;
    assert_eq!(pan(&v, &mut c), Ok(()));
    assert_eq!(c.0[YOFFSET], 1440);
    v.0[YOFFSET] = 1441;
    assert_eq!(pan(&v, &mut c), Err(EINVAL));
    assert_eq!(c.0[YOFFSET], 1440, "unchanged after a refusal");
    v.0[YOFFSET] = 7;
    assert_eq!(pan(&v, &mut c), Ok(()), "ypanstep 1: any row");
    v.0[XOFFSET] = 1;
    assert_eq!(pan(&v, &mut c), Err(EINVAL), "no horizontal room");
    v.0[XOFFSET] = 0;
    v.0[VMODE] = FB_VMODE_YWRAP;
    assert_eq!(pan(&v, &mut c), Err(EINVAL), "ywrapstep 0");
    v.0[YOFFSET] = 0;
    assert_eq!(pan(&v, &mut c), Ok(()), "YWRAP at offset 0 passes and is remembered");
    assert_eq!(c.0[VMODE] & FB_VMODE_YWRAP, FB_VMODE_YWRAP);
    v.0[VMODE] = 0;
    assert_eq!(pan(&v, &mut c), Ok(()));
    assert_eq!(c.0[VMODE] & FB_VMODE_YWRAP, 0);
    // Single buffer: no vertical room either.
    let g1 = Geometry { rows: 1440, ..G };
    let mut c1 = g1.initial_var();
    let mut v1 = c1;
    v1.0[YOFFSET] = 1;
    assert_eq!(pan(&v1, &mut c1), Err(EINVAL));
}

/// Fake program memory: a buffer at `BASE`, everything else faults.
struct Mem(RefCell<Vec<u8>>);
const BASE: u64 = 0x1000_0000;
impl Mem {
    fn new(n: usize) -> Mem {
        Mem(RefCell::new(alloc::vec![0; n]))
    }
    fn span(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let off = addr.checked_sub(BASE)? as usize;
        (off.checked_add(n)? <= self.0.borrow().len()).then(|| off..off + n)
    }
    fn put_u16s(&self, addr: u64, v: &[u16]) {
        let r = self.span(addr, v.len() * 2).unwrap();
        let b: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        self.0.borrow_mut()[r].copy_from_slice(&b);
    }
    fn u16s(&self, addr: u64, n: usize) -> Vec<u16> {
        let r = self.span(addr, n * 2).unwrap();
        self.0.borrow()[r].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
    }
}
impl UserIo for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        match self.span(addr, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.0.borrow()[r]);
                true
            }
            None => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.span(addr, data.len()) {
            Some(r) => {
                self.0.borrow_mut()[r].copy_from_slice(data);
                true
            }
            None => false,
        }
    }
}

fn user(start: u32, len: u32) -> CmapUser {
    CmapUser { start, len, red: BASE, green: BASE + 0x1000, blue: BASE + 0x2000, transp: 0 }
}

#[test]
fn cmap_get_hands_out_the_text_colours() {
    let m = Mem::new(0x4000);
    let c = Cmap::default();
    assert_eq!(c.to_user(&user(0, 16), &m), Ok(()));
    let (lo, hi, half) = (0xaaaa, 0xffff, 0x5555);
    assert_eq!(m.u16s(BASE, 16), [0, 0, 0, 0, lo, lo, lo, lo, half, half, half, half, hi, hi, hi, hi], "red");
    assert_eq!(m.u16s(BASE + 0x1000, 16), [0, 0, lo, lo, 0, 0, half, lo, half, half, hi, hi, half, half, hi, hi], "green");
    assert_eq!(m.u16s(BASE + 0x2000, 16), [0, lo, 0, lo, 0, lo, 0, lo, half, hi, half, hi, half, hi, half, hi], "blue");
    assert_eq!(c.to_user(&user(14, 4), &m), Ok(()), "entries 14..17");
    assert_eq!(m.u16s(BASE + 0x1000, 4), [0xffff, 0xffff, 0, 0]);
    assert_eq!(c.to_user(&user(250, 100), &m), Ok(()), "clipped to the map's 256");
    assert_eq!(c.to_user(&user(256, 1), &m), Err(EINVAL), "start past the map");
    assert_eq!(c.to_user(&user(0, 0), &m), Err(EINVAL), "length 0");
    let mut bad = user(0, 16);
    bad.green = 0x10;
    assert_eq!(c.to_user(&bad, &m), Err(EFAULT));
}

#[test]
fn cmap_put_follows_fb_set_user_cmap_and_the_pseudo_palette() {
    let m = Mem::new(0x4000);
    let mut c = Cmap::default();
    m.put_u16s(BASE, &[1, 2, 3, 4]);
    m.put_u16s(BASE + 0x1000, &[5, 6, 7, 8]);
    m.put_u16s(BASE + 0x2000, &[9, 10, 11, 12]);
    assert_eq!(c.set_user(&user(2, 4), &m), Ok(()));
    assert_eq!(c.red[..7], [0, 0, 1, 2, 3, 4, 0xaaaa]);
    assert_eq!(c.blue[2..6], [9, 10, 11, 12]);
    assert_eq!(c.set_user(&user(14, 4), &m), Err(EINVAL), "past the 16-entry pseudo palette");
    assert_eq!(c.set_user(&user(0, 0), &m), Err(EINVAL), "length 0");
    assert_eq!(c.set_user(&user(0, 1 << 30), &m), Err(E2BIG));
    assert_eq!(c.set_user(&user(0, (2 << 20) + 1), &m), Err(ENOMEM), "past kzalloc's 4 MiB");
    assert_eq!(c.set_user(&user(0, 0x1800), &m), Err(EFAULT), "the channel runs past the readable memory");
    let mut t = user(0, 2);
    t.transp = 0x20;
    assert_eq!(c.set_user(&t, &m), Err(EFAULT), "an unreadable transp is EFAULT");
    let mut neg = user(0x8000_0000, 1);
    neg.red = BASE;
    assert_eq!(c.set_user(&neg, &m), Err(EINVAL), "start < 0 as an int");
    assert_eq!(c.red[2], 1, "refusals change nothing");
}

#[test]
fn cmap_user_layout() {
    let mut b = [0u8; CMAP_USER_SIZE];
    b[0..4].copy_from_slice(&3u32.to_le_bytes());
    b[4..8].copy_from_slice(&7u32.to_le_bytes());
    b[8..16].copy_from_slice(&0x1111u64.to_le_bytes());
    b[32..40].copy_from_slice(&0x4444u64.to_le_bytes());
    let c = CmapUser::from_bytes(&b);
    assert_eq!((c.start, c.len, c.red, c.green, c.transp), (3, 7, 0x1111, 0, 0x4444));
}
